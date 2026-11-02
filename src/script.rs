//! Lua scripting. `init.lua` next to the config file can bind keys, add commands,
//! react to events, change settings and the theme, and format the interface.
//!
//! Scripts run on the interface thread. Each call gets a time limit, so a runaway
//! loop shows an error instead of freezing termail.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, Value, VmState};

use crate::mail::Mail;

/// How long one script call may run before it is stopped.
const TIME_LIMIT: Duration = Duration::from_secs(2);
/// Calls made while drawing get much less, since they run on every frame.
const DRAW_LIMIT: Duration = Duration::from_millis(50);

/// Something a script asked termail to do. Applied once the script call returns.
#[derive(Debug)]
pub enum Effect {
    Notice(String),
    Notify {
        title: String,
        body: String,
    },
    /// A built-in action by name, such as `archive`, or a `:` command.
    Run(String),
    Move {
        id: String,
        folder: String,
    },
    Mark {
        id: String,
        read: Option<bool>,
        starred: Option<bool>,
    },
    Compose {
        to: String,
        subject: String,
        body: String,
    },
    Open(String),
    Folder(String),
    Search(String),
    /// A ui setting or the theme changed.
    Settings,
}

#[derive(Clone)]
enum Target {
    Action(String),
    Function(Function),
}

#[derive(Clone)]
struct Binding {
    code: KeyCode,
    modifiers: KeyModifiers,
    mode: String,
    target: Target,
}

struct Timer {
    every: Duration,
    next: Instant,
    function: Function,
}

/// What a script can read about the app.
#[derive(Default)]
pub struct Context {
    pub selected: Option<Mail>,
    pub folder: String,
    pub account: String,
    pub query: String,
    /// Every loaded message, without bodies.
    pub mails: Vec<Mail>,
}

#[derive(Default)]
struct Shared {
    effects: RefCell<Vec<Effect>>,
    bindings: RefCell<Vec<Binding>>,
    commands: RefCell<HashMap<String, Function>>,
    hooks: RefCell<HashMap<String, Vec<Function>>>,
    timers: RefCell<Vec<Timer>>,
    status: RefCell<Option<Function>>,
    format: RefCell<Option<Function>>,
    context: RefCell<Context>,
    deadline: Cell<Option<Instant>>,
}

pub struct Engine {
    lua: Lua,
    shared: Rc<Shared>,
    path: PathBuf,
    /// Formatted list rows by conversation state, so formatting runs once per change.
    rows: RefCell<HashMap<String, (String, String)>>,
}

/// Events scripts can listen to.
pub const EVENTS: &[&str] = &[
    "startup",
    "new_mail",
    "open",
    "before_send",
    "sync",
    "folder",
];

/// The script for this config: `ui.script`, or `init.lua` next to the config file.
pub fn path(config: &crate::config::Config) -> Option<PathBuf> {
    let configured = crate::settings::with(|ui| ui.script.clone());
    let base = config
        .path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    match configured {
        Some(path) if path.is_relative() => base.map(|base| base.join(&path)).or(Some(path)),
        Some(path) => Some(path),
        None => base
            .map(|base| base.join("init.lua"))
            .filter(|path| path.is_file()),
    }
}

impl Engine {
    /// Runs the script. A script that fails still gives an engine, with what it
    /// registered before the error, and the error to show.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let engine = Self {
            lua: Lua::new(),
            shared: Rc::new(Shared::default()),
            path: path.to_path_buf(),
            rows: RefCell::default(),
        };
        let error = engine
            .install()
            .and_then(|()| {
                let source = std::fs::read_to_string(path).map_err(|error| {
                    mlua::Error::external(format!("{}: {error}", path.display()))
                })?;
                engine.guard(TIME_LIMIT, || {
                    engine
                        .lua
                        .load(&source)
                        .set_name(format!("@{}", path.display()))
                        .exec()
                })
            })
            .err()
            .map(|error| describe(&error));
        (engine, error)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs `call` with a time limit enforced by an instruction hook.
    fn guard<R>(&self, limit: Duration, call: impl FnOnce() -> mlua::Result<R>) -> mlua::Result<R> {
        let outer = self.shared.deadline.get();
        self.shared.deadline.set(Some(Instant::now() + limit));
        let result = call();
        self.shared.deadline.set(outer);
        result
    }

    fn install(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let shared = self.shared.clone();
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(20_000),
            move |_, _| match shared.deadline.get() {
                Some(deadline) if Instant::now() > deadline => Err(mlua::Error::external(
                    "the script ran too long and was stopped",
                )),
                _ => Ok(VmState::Continue),
            },
        )?;
        let api = lua.create_table()?;
        api.set("version", env!("CARGO_PKG_VERSION"))?;
        api.set("events", lua.create_sequence_from(EVENTS.iter().copied())?)?;
        api.set(
            "actions",
            lua.create_sequence_from(crate::keymap::ACTIONS.iter().map(|(name, ..)| *name))?,
        )?;

        let effect = |shared: &Rc<Shared>, value: Effect| shared.effects.borrow_mut().push(value);

        let shared = self.shared.clone();
        api.set(
            "map",
            lua.create_function(
                move |_, (key, target, options): (String, Value, Option<Table>)| {
                    let (code, modifiers) = crate::keymap::parse_key(&key)
                        .ok_or_else(|| mlua::Error::external(format!("unknown key {key:?}")))?;
                    let target = match target {
                        Value::Function(function) => Target::Function(function),
                        Value::String(name) => Target::Action(name.to_str()?.to_owned()),
                        _ => {
                            return Err(mlua::Error::external(
                                "map needs an action name or a function",
                            ));
                        }
                    };
                    let mode = options
                        .and_then(|options| options.get::<Option<String>>("mode").ok().flatten())
                        .unwrap_or_else(|| "normal".into());
                    if !["normal", "inbox", "map", "reader"].contains(&mode.as_str()) {
                        return Err(mlua::Error::external(
                            "mode must be normal, inbox, map or reader",
                        ));
                    }
                    let mut bindings = shared.bindings.borrow_mut();
                    bindings.retain(|binding| {
                        !(binding.code == code
                            && binding.modifiers == modifiers
                            && binding.mode == mode)
                    });
                    bindings.push(Binding {
                        code,
                        modifiers,
                        mode,
                        target,
                    });
                    Ok(())
                },
            )?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "command",
            lua.create_function(move |_, (name, function): (String, Function)| {
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return Err(mlua::Error::external("command names are single words"));
                }
                shared.commands.borrow_mut().insert(name, function);
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "on",
            lua.create_function(move |_, (event, function): (String, Function)| {
                if !EVENTS.contains(&event.as_str()) {
                    return Err(mlua::Error::external(format!(
                        "unknown event {event:?}; use one of {}",
                        EVENTS.join(", ")
                    )));
                }
                shared
                    .hooks
                    .borrow_mut()
                    .entry(event)
                    .or_default()
                    .push(function);
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "every",
            lua.create_function(move |_, (seconds, function): (f64, Function)| {
                if seconds.is_nan() || seconds < 1.0 {
                    return Err(mlua::Error::external("every needs at least 1 second"));
                }
                let every = Duration::from_secs_f64(seconds);
                shared.timers.borrow_mut().push(Timer {
                    every,
                    next: Instant::now() + every,
                    function,
                });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "status",
            lua.create_function(move |_, function: Option<Function>| {
                *shared.status.borrow_mut() = function;
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "format",
            lua.create_function(move |_, function: Option<Function>| {
                *shared.format.borrow_mut() = function;
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "notice",
            lua.create_function(move |_, text: String| {
                effect(&shared, Effect::Notice(text));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "notify",
            lua.create_function(move |_, (title, body): (String, Option<String>)| {
                let (title, body) = match body {
                    Some(body) => (title, body),
                    None => ("termail".into(), title),
                };
                effect(&shared, Effect::Notify { title, body });
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "run",
            lua.create_function(move |_, name: String| {
                effect(&shared, Effect::Run(name));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "move",
            lua.create_function(move |_, (id, folder): (String, String)| {
                effect(&shared, Effect::Move { id, folder });
                Ok(())
            })?,
        )?;

        for (name, folder) in [("archive", "Archive"), ("trash", "Trash")] {
            let shared = self.shared.clone();
            api.set(
                name,
                lua.create_function(move |_, id: String| {
                    effect(
                        &shared,
                        Effect::Move {
                            id,
                            folder: folder.into(),
                        },
                    );
                    Ok(())
                })?,
            )?;
        }

        let shared = self.shared.clone();
        api.set(
            "mark",
            lua.create_function(move |_, (id, flags): (String, Table)| {
                effect(
                    &shared,
                    Effect::Mark {
                        id,
                        read: flags.get("read")?,
                        starred: flags.get("starred")?,
                    },
                );
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "compose",
            lua.create_function(move |_, fields: Option<Table>| {
                let get = |name: &str| -> mlua::Result<String> {
                    Ok(fields
                        .as_ref()
                        .map(|fields| fields.get::<Option<String>>(name))
                        .transpose()?
                        .flatten()
                        .unwrap_or_default())
                };
                effect(
                    &shared,
                    Effect::Compose {
                        to: get("to")?,
                        subject: get("subject")?,
                        body: get("body")?,
                    },
                );
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "open",
            lua.create_function(move |_, id: String| {
                effect(&shared, Effect::Open(id));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "folder",
            lua.create_function(move |_, name: String| {
                effect(&shared, Effect::Folder(name));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "search",
            lua.create_function(move |_, query: String| {
                effect(&shared, Effect::Search(query));
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "set",
            lua.create_function(move |lua, (name, value): (String, Value)| {
                let value: serde_json::Value = lua.from_value(value)?;
                crate::settings::set(&name, value).map_err(mlua::Error::external)?;
                effect(&shared, Effect::Settings);
                Ok(())
            })?,
        )?;

        api.set(
            "get",
            lua.create_function(|lua, name: String| {
                let value = crate::settings::value(&name).map_err(mlua::Error::external)?;
                lua.to_value(&value)
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "theme",
            lua.create_function(move |lua, colors: Value| {
                let colors: crate::theme::ThemeConfig = lua.from_value(colors)?;
                crate::theme::update(&colors).map_err(mlua::Error::external)?;
                effect(&shared, Effect::Settings);
                Ok(())
            })?,
        )?;

        api.set(
            "log",
            lua.create_function(|_, text: String| {
                crate::log::write("script", text);
                Ok(())
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "selected",
            lua.create_function(move |lua, ()| match &shared.context.borrow().selected {
                Some(mail) => mail_table(lua, mail, true).map(Value::Table),
                None => Ok(Value::Nil),
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "mails",
            lua.create_function(move |lua, query: Option<String>| {
                let context = shared.context.borrow();
                let query = query.unwrap_or_default();
                let list = lua.create_table()?;
                for mail in context.mails.iter().filter(|mail| mail.matches(&query)) {
                    list.push(mail_table(lua, mail, false)?)?;
                }
                Ok(list)
            })?,
        )?;

        let shared = self.shared.clone();
        api.set(
            "state",
            lua.create_function(move |lua, ()| {
                let context = shared.context.borrow();
                let state = lua.create_table()?;
                state.set("folder", context.folder.clone())?;
                state.set("account", context.account.clone())?;
                state.set("query", context.query.clone())?;
                state.set("count", context.mails.len())?;
                state.set(
                    "unread",
                    context
                        .mails
                        .iter()
                        .filter(|mail| mail.unread && folder_of(mail) == context.folder)
                        .count(),
                )?;
                Ok(state)
            })?,
        )?;

        lua.globals().set("termail", api)?;
        Ok(())
    }

    /// Takes what the last calls asked for.
    pub fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.shared.effects.borrow_mut())
    }

    pub fn context(&self) -> std::cell::RefMut<'_, Context> {
        self.shared.context.borrow_mut()
    }

    /// The script binding for a key in a mode, if any. Running it is up to the caller.
    pub fn binding(&self, key: KeyEvent, mode: &str) -> Option<Result<Option<String>, String>> {
        let modifiers = key.modifiers - KeyModifiers::SHIFT;
        let binding = self
            .shared
            .bindings
            .borrow()
            .iter()
            .rev()
            .find(|binding| {
                binding.code == key.code
                    && binding.modifiers == modifiers
                    && (binding.mode == mode || binding.mode == "normal")
            })
            .cloned()?;
        Some(match binding.target {
            Target::Action(name) => Ok(Some(name)),
            Target::Function(function) => self
                .guard(TIME_LIMIT, || function.call::<()>(()))
                .map(|()| None)
                .map_err(|error| describe(&error)),
        })
    }

    /// Runs a script command. Returns `None` when no script defines it.
    pub fn command(&self, name: &str, arguments: &str) -> Option<Result<(), String>> {
        let function = self.shared.commands.borrow().get(name).cloned()?;
        Some(
            self.guard(TIME_LIMIT, || function.call::<()>(arguments.to_owned()))
                .map_err(|error| describe(&error)),
        )
    }

    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.shared.commands.borrow().keys().cloned().collect();
        names.sort();
        names
    }

    fn handlers(&self, event: &str) -> Vec<Function> {
        self.shared
            .hooks
            .borrow()
            .get(event)
            .cloned()
            .unwrap_or_default()
    }

    pub fn has(&self, event: &str) -> bool {
        !self.handlers(event).is_empty()
    }

    /// Calls every handler of an event with a message, if it has any.
    pub fn emit_mail(&self, event: &str, mail: &Mail) -> Result<(), String> {
        let handlers = self.handlers(event);
        if handlers.is_empty() {
            return Ok(());
        }
        let table = mail_table(&self.lua, mail, true).map_err(|error| describe(&error))?;
        for handler in handlers {
            self.guard(TIME_LIMIT, || handler.call::<()>(table.clone()))
                .map_err(|error| describe(&error))?;
        }
        Ok(())
    }

    /// Calls every handler of an event with a table of plain values.
    pub fn emit(&self, event: &str, fields: &[(&str, String)]) -> Result<(), String> {
        let handlers = self.handlers(event);
        if handlers.is_empty() {
            return Ok(());
        }
        let table = self.lua.create_table().map_err(|error| describe(&error))?;
        for (name, value) in fields {
            table
                .set(*name, value.clone())
                .map_err(|error| describe(&error))?;
        }
        for handler in handlers {
            self.guard(TIME_LIMIT, || handler.call::<()>(table.clone()))
                .map_err(|error| describe(&error))?;
        }
        Ok(())
    }

    /// Lets `before_send` handlers change a message. Returns the fields as the
    /// handlers left them, or why sending was stopped.
    pub fn before_send(&self, draft: &mut Draft) -> Result<(), String> {
        let handlers = self.handlers("before_send");
        if handlers.is_empty() {
            return Ok(());
        }
        let fail = |error: mlua::Error| describe(&error);
        let table = self.lua.create_table().map_err(fail)?;
        for (name, value) in draft.fields() {
            table.set(name, value.clone()).map_err(fail)?;
        }
        for handler in handlers {
            let (keep, reason): (Option<bool>, Option<String>) = self
                .guard(TIME_LIMIT, || handler.call(table.clone()))
                .map_err(fail)?;
            if keep == Some(false) {
                return Err(reason.unwrap_or_else(|| "A script stopped this message.".into()));
            }
        }
        for (name, value) in draft.fields_mut() {
            *value = table
                .get::<Option<String>>(name)
                .map_err(fail)?
                .unwrap_or_default();
        }
        Ok(())
    }

    /// Runs timers that are due.
    pub fn run_timers(&self) -> Vec<String> {
        let now = Instant::now();
        let due: Vec<Function> = self
            .shared
            .timers
            .borrow_mut()
            .iter_mut()
            .filter(|timer| timer.next <= now)
            .map(|timer| {
                timer.next = now + timer.every;
                timer.function.clone()
            })
            .collect();
        due.into_iter()
            .filter_map(|function| {
                self.guard(TIME_LIMIT, || function.call::<()>(()))
                    .err()
                    .map(|error| describe(&error))
            })
            .collect()
    }

    /// Text for the status line from `termail.status`.
    pub fn status(&self) -> Option<String> {
        let function = self.shared.status.borrow().clone()?;
        match self.guard(DRAW_LIMIT, || function.call::<Option<String>>(())) {
            Ok(text) => text.filter(|text| !text.is_empty()),
            Err(error) => {
                crate::log::write("script", describe(&error));
                None
            }
        }
    }

    pub fn formats_rows(&self) -> bool {
        self.shared.format.borrow().is_some()
    }

    /// A list row from `termail.format`, cached until the conversation changes.
    pub fn format_row(&self, key: String, row: &Row) -> Option<(String, String)> {
        if let Some(cached) = self.rows.borrow().get(&key) {
            return Some(cached.clone());
        }
        let function = self.shared.format.borrow().clone()?;
        let result = (|| {
            let table = self.lua.create_table()?;
            table.set("subject", row.subject.clone())?;
            table.set("from", row.from.clone())?;
            table.set("date", row.date.clone())?;
            table.set("time", row.timestamp)?;
            table.set("count", row.count)?;
            table.set("unread", row.unread)?;
            table.set("starred", row.starred)?;
            table.set("folder", row.folder.clone())?;
            table.set("account", row.account.clone())?;
            table.set("title", row.title.clone())?;
            table.set("subtitle", row.subtitle.clone())?;
            self.guard(DRAW_LIMIT, || {
                function.call::<(Option<String>, Option<String>)>(table)
            })
        })();
        match result {
            Ok((title, subtitle)) => {
                let formatted = (
                    title.unwrap_or_else(|| row.title.clone()),
                    subtitle.unwrap_or_else(|| row.subtitle.clone()),
                );
                self.rows.borrow_mut().insert(key, formatted.clone());
                Some(formatted)
            }
            Err(error) => {
                crate::log::write("script", describe(&error));
                None
            }
        }
    }

    pub fn forget_rows(&self) {
        self.rows.borrow_mut().clear();
    }
}

/// One conversation as `termail.format` sees it, with the built-in text.
pub struct Row {
    pub subject: String,
    pub from: String,
    pub date: String,
    pub timestamp: i64,
    pub count: usize,
    pub unread: bool,
    pub starred: bool,
    pub folder: String,
    pub account: String,
    pub title: String,
    pub subtitle: String,
}

/// The fields of a message being sent that `before_send` may change.
pub struct Draft {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
}

impl Draft {
    fn fields(&self) -> [(&'static str, &String); 6] {
        [
            ("from", &self.from),
            ("to", &self.to),
            ("cc", &self.cc),
            ("bcc", &self.bcc),
            ("subject", &self.subject),
            ("body", &self.body),
        ]
    }

    fn fields_mut(&mut self) -> [(&'static str, &mut String); 6] {
        [
            ("from", &mut self.from),
            ("to", &mut self.to),
            ("cc", &mut self.cc),
            ("bcc", &mut self.bcc),
            ("subject", &mut self.subject),
            ("body", &mut self.body),
        ]
    }
}

fn folder_of(mail: &Mail) -> &str {
    mail.mailbox.as_deref().unwrap_or("INBOX")
}

fn mail_table(lua: &Lua, mail: &Mail, with_body: bool) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("id", mail.id.clone())?;
    table.set("from", mail.from.clone())?;
    table.set("address", crate::mail::email_of(&mail.from))?;
    table.set("to", lua.create_sequence_from(mail.to.clone())?)?;
    table.set("cc", lua.create_sequence_from(mail.cc.clone())?)?;
    table.set("subject", mail.subject.clone())?;
    table.set("folder", folder_of(mail).to_owned())?;
    table.set("account", mail.account.clone())?;
    table.set("unread", mail.unread)?;
    table.set("starred", mail.starred)?;
    table.set("time", mail.timestamp)?;
    table.set("date", mail.timestamp.map(crate::mail::long_date))?;
    table.set("attachments", mail.attachment_count)?;
    table.set("list", mail.unsubscribe.is_some())?;
    table.set("html", mail.html)?;
    if with_body {
        table.set("body", mail.body.clone())?;
        table.set("links", lua.create_sequence_from(mail.links.clone())?)?;
    }
    Ok(table)
}

/// A short, readable error with the script line when Lua gives one.
fn describe(error: &mlua::Error) -> String {
    let text = match error {
        mlua::Error::CallbackError { cause, .. } => return describe(cause),
        mlua::Error::RuntimeError(message) | mlua::Error::SyntaxError { message, .. } => {
            message.clone()
        }
        other => other.to_string(),
    };
    text.lines().next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(source: &str) -> (Engine, Option<String>) {
        let path = std::env::temp_dir().join(format!(
            "{}.lua",
            crate::storage::unique_name("termail-script")
        ));
        std::fs::write(&path, source).unwrap();
        let loaded = Engine::load(&path);
        let _ = std::fs::remove_file(&path);
        loaded
    }

    #[test]
    fn binds_keys_runs_commands_and_reports_errors_with_lines() {
        let (engine, error) = engine(
            r#"
            termail.map("e", "archive")
            termail.map("ctrl+t", function() termail.notice("pressed") end, { mode = "reader" })
            termail.command("hello", function(name) termail.notice("hi " .. name) end)
            "#,
        );
        assert!(error.is_none(), "{error:?}");
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(
            engine.binding(key, "inbox"),
            Some(Ok(Some("archive".into())))
        );
        let control = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(
            engine.binding(control, "inbox").is_none(),
            "reader keys stay in the reader"
        );
        assert_eq!(engine.binding(control, "reader"), Some(Ok(None)));
        assert!(
            matches!(engine.take_effects().as_slice(), [Effect::Notice(text)] if text == "pressed")
        );
        assert_eq!(engine.command("hello", "Ada"), Some(Ok(())));
        assert!(
            matches!(engine.take_effects().as_slice(), [Effect::Notice(text)] if text == "hi Ada")
        );
        assert!(engine.command("missing", "").is_none());
        let (_, error) = self::engine("termail.map('e', 'archive')\nthis is not lua\n");
        assert!(error.unwrap().contains(":2:"), "errors name the line");
    }

    #[test]
    fn stops_runaway_scripts() {
        let (engine, _) = engine(r#"termail.command("spin", function() while true do end end)"#);
        let started = Instant::now();
        let result = engine.command("spin", "").unwrap();
        assert!(result.unwrap_err().contains("too long"));
        assert!(started.elapsed() < TIME_LIMIT * 2);
    }

    #[test]
    fn before_send_can_change_or_stop_a_message() {
        let (engine, _) = engine(
            r#"termail.on("before_send", function(m)
                if m.subject == "" then return false, "Add a subject." end
                m.body = m.body .. "\n-- sent with lua"
            end)"#,
        );
        let mut draft = Draft {
            from: "a@x".into(),
            to: "b@x".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: "hello".into(),
        };
        assert_eq!(
            engine.before_send(&mut draft).unwrap_err(),
            "Add a subject."
        );
        draft.subject = "hi".into();
        engine.before_send(&mut draft).unwrap();
        assert!(draft.body.ends_with("-- sent with lua"));
    }

    #[test]
    fn settings_are_checked() {
        let (_, error) = engine(r#"termail.set("sort", "sideways")"#);
        assert!(error.unwrap().contains("ui.sort"));
        let (_, error) = engine(r#"termail.on("no_such_event", function() end)"#);
        assert!(error.unwrap().contains("unknown event"));
    }
}
