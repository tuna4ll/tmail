//! Connects the Lua engine to the app: loading the script, telling it what is on
//! screen, running its bindings and commands, and carrying out what it asks for.

use super::*;

impl App {
    /// Loads `init.lua`, or reloads it. Errors show in the status line.
    pub(crate) fn load_script(&mut self) {
        let Some(path) = script::path(&self.config) else {
            self.script = None;
            return;
        };
        let (engine, error) = script::Engine::load(&path);
        self.script = Some(engine);
        if let Some(error) = error {
            self.notice = Some(format!("Script error · {error}"));
            crate::log::write("script", &error);
        }
        self.apply_script_effects();
        self.update_script_mailbox();
        self.emit_event("startup", &[]);
    }

    /// Shares the loaded messages with the script, without their bodies.
    pub(crate) fn update_script_mailbox(&mut self) {
        let Some(engine) = &self.script else {
            return;
        };
        let mails = self
            .mails
            .iter()
            .map(|mail| Mail {
                body: String::new(),
                links: Vec::new(),
                ..mail.clone()
            })
            .collect();
        engine.context().mails = mails;
        engine.forget_rows();
    }

    fn update_script_context(&self) {
        let Some(engine) = &self.script else {
            return;
        };
        let selected = self
            .active_mail_id()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id))
            .cloned();
        let mut context = engine.context();
        context.selected = selected;
        context.folder = self.active_folder.clone();
        context.account = self.account_label.clone();
        context.query = self.query.clone();
    }

    fn script_mode(&self) -> &'static str {
        if self.reader.is_some() {
            "reader"
        } else if self.focus == Pane::Workspace {
            "map"
        } else {
            "inbox"
        }
    }

    /// Runs a script key binding. Returns whether the key was used.
    pub(crate) fn run_script_key(&mut self, key: KeyEvent) -> bool {
        self.update_script_context();
        let mode = self.script_mode();
        let Some(result) = self
            .script
            .as_ref()
            .and_then(|engine| engine.binding(key, mode))
        else {
            return false;
        };
        match result {
            Ok(Some(action)) => self.run_named(&action),
            Ok(None) => {}
            Err(error) => self.notice = Some(format!("Script error · {error}")),
        }
        self.apply_script_effects();
        true
    }

    /// Runs a `:` command a script defined. Returns whether one did.
    pub(crate) fn run_script_command(&mut self, command: &str) -> bool {
        let (name, arguments) = command.split_once(' ').unwrap_or((command, ""));
        self.update_script_context();
        let Some(result) = self
            .script
            .as_ref()
            .and_then(|engine| engine.command(name, arguments.trim()))
        else {
            return false;
        };
        if let Err(error) = result {
            self.notice = Some(format!("Script error · {error}"));
        }
        self.apply_script_effects();
        true
    }

    /// Tells the script about an event with plain fields.
    pub(crate) fn emit_event(&mut self, event: &str, fields: &[(&str, String)]) {
        let Some(engine) = &self.script else {
            return;
        };
        if !engine.has(event) {
            return;
        }
        self.update_script_context();
        if let Some(Err(error)) = self
            .script
            .as_ref()
            .map(|engine| engine.emit(event, fields))
        {
            self.notice = Some(format!("Script error in {event} · {error}"));
        }
        self.apply_script_effects();
    }

    /// Tells the script about an event concerning one message.
    pub(crate) fn emit_mail_event(&mut self, event: &str, mail_id: &str) {
        let Some(engine) = &self.script else {
            return;
        };
        if !engine.has(event) {
            return;
        }
        let Some(mail) = self.mails.iter().find(|mail| mail.id == mail_id).cloned() else {
            return;
        };
        self.update_script_context();
        if let Some(Err(error)) = self
            .script
            .as_ref()
            .map(|engine| engine.emit_mail(event, &mail))
        {
            self.notice = Some(format!("Script error in {event} · {error}"));
        }
        self.apply_script_effects();
    }

    /// Calls `new_mail` for unread messages that were not loaded before.
    pub(crate) fn emit_new_mail(&mut self, before: &std::collections::HashSet<String>) {
        if before.is_empty()
            || !self
                .script
                .as_ref()
                .is_some_and(|engine| engine.has("new_mail"))
        {
            return;
        }
        let arrived: Vec<String> = self
            .mails
            .iter()
            .filter(|mail| mail.unread && !before.contains(&mail.id))
            .map(|mail| mail.id.clone())
            .take(200)
            .collect();
        for id in arrived {
            self.emit_mail_event("new_mail", &id);
        }
    }

    /// Lets the script change or stop a message before it is sent.
    pub(crate) fn script_before_send(&mut self, state: &mut ComposeState) -> Result<(), String> {
        let Some(engine) = &self.script else {
            return Ok(());
        };
        if !engine.has("before_send") {
            return Ok(());
        }
        self.update_script_context();
        let composer = &mut state.composer;
        let mut draft = script::Draft {
            from: composer.from.clone(),
            to: composer.to.clone(),
            cc: composer.cc.clone(),
            bcc: composer.bcc.clone(),
            subject: composer.subject.clone(),
            body: fs::read_to_string(&composer.body_path).unwrap_or_default(),
        };
        let original_body = draft.body.clone();
        let result = self
            .script
            .as_ref()
            .map_or(Ok(()), |engine| engine.before_send(&mut draft));
        self.apply_script_effects();
        result?;
        composer.from = draft.from;
        composer.to = draft.to;
        composer.cc = draft.cc;
        composer.bcc = draft.bcc;
        composer.subject = draft.subject;
        if draft.body != original_body {
            fs::write(&composer.body_path, draft.body).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn run_script_timers(&mut self) {
        if self.script.is_none() {
            return;
        }
        self.update_script_context();
        let errors = self
            .script
            .as_ref()
            .map(script::Engine::run_timers)
            .unwrap_or_default();
        if let Some(error) = errors.into_iter().next() {
            self.notice = Some(format!("Script error in a timer · {error}"));
        }
        self.apply_script_effects();
    }

    pub(crate) fn script_status(&self) -> Option<String> {
        self.script.as_ref()?.status()
    }

    /// Runs a built-in action by name, or a `:` command.
    fn run_named(&mut self, name: &str) {
        if let Some(command) = name.strip_prefix(':') {
            self.run_command(command.trim());
            return;
        }
        match keymap::ACTIONS.iter().find(|(action, ..)| *action == name) {
            Some((_, key, _)) => {
                let key = KeyEvent::new(KeyCode::Char(*key), KeyModifiers::NONE);
                self.on_key_event(key);
            }
            None => self.run_command(name),
        }
    }

    /// Carries out what scripts asked for during their last calls.
    pub(crate) fn apply_script_effects(&mut self) {
        for _ in 0..8 {
            let effects = self
                .script
                .as_ref()
                .map(script::Engine::take_effects)
                .unwrap_or_default();
            if effects.is_empty() {
                return;
            }
            for effect in effects {
                self.apply_effect(effect);
            }
        }
    }

    fn apply_effect(&mut self, effect: script::Effect) {
        use script::Effect;
        match effect {
            Effect::Notice(text) => self.notice = Some(text),
            Effect::Notify { title, body } => background::notify(&title, &body),
            Effect::Run(name) => self.run_named(&name),
            Effect::Move { id, folder } => {
                let Some(index) = self.mails.iter().position(|mail| mail.id == id) else {
                    return;
                };
                let label = format!("Moved to {folder}");
                self.change_messages(&[index], &label, |source, mail| {
                    let change = source.move_to_named(mail, &folder)?;
                    mail.mailbox = Some(folder.clone());
                    Ok(change)
                });
            }
            Effect::Mark { id, read, starred } => {
                let Some(index) = self.mails.iter().position(|mail| mail.id == id) else {
                    return;
                };
                if let Some(read) = read {
                    self.change_messages(&[index], "Marked by script", |source, mail| {
                        let change = source.set_seen(mail, read)?;
                        mail.unread = !read;
                        Ok(change)
                    });
                }
                if let Some(starred) = starred {
                    self.change_messages(&[index], "Marked by script", |source, mail| {
                        let change = source.set_starred(mail, starred)?;
                        mail.starred = starred;
                        Ok(change)
                    });
                }
            }
            Effect::Compose { to, subject, body } => {
                self.start_draft(draft::Start::New);
                if let Some(Overlay::Compose(state)) = &mut self.overlay {
                    state.composer.to = to;
                    state.composer.subject = subject;
                    if !body.is_empty() {
                        let _ = fs::write(&state.composer.body_path, body);
                    }
                }
            }
            Effect::Open(id) => {
                let Some(folder) = self
                    .mails
                    .iter()
                    .find(|mail| mail.id == id)
                    .map(|mail| mail_folder(mail).to_owned())
                else {
                    return;
                };
                if folder != self.active_folder {
                    self.active_folder = folder;
                    self.rebuild_mailbox(Some(&id));
                }
                self.open_mail(&id);
            }
            Effect::Folder(name) => {
                self.active_folder = name;
                self.reader = None;
                self.rebuild_mailbox(None);
            }
            Effect::Search(query) => {
                self.query = query;
                self.refresh_inbox();
            }
            Effect::Settings => {
                settings::with(|ui| {
                    self.inbox_width = ui.inbox_width;
                    self.reader_width = ui.reader_width;
                });
                if let Some(engine) = &self.script {
                    engine.forget_rows();
                }
                self.rebuild_mailbox(self.active_mail_id().as_deref());
                self.clear_screen = true;
            }
        }
    }
}
