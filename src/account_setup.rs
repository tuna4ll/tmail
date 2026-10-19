use crate::{
    config::{Account, Config},
    remote::{RemoteConfig, SessionPassword},
    theme,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use zeroize::Zeroize;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Add,
    Edit,
    Login,
}

pub struct Form {
    pub mode: Mode,
    pub account: Account,
    pub values: [String; 10],
    pub field: usize,
    pub make_default: bool,
    pub error: Option<String>,
    pub busy: bool,
    pub request: u64,
}

impl Drop for Form {
    fn drop(&mut self) {
        self.values[3].zeroize();
    }
}

impl Form {
    pub fn new(config: &Config, mode: Mode) -> Self {
        let account = if mode == Mode::Add {
            let base = config.path.as_ref().and_then(|path| path.parent());
            let mut number = 1;
            while config
                .accounts
                .iter()
                .any(|account| account.id == format!("account-{number}"))
                || base.is_some_and(|root| {
                    root.join("mail").join(format!("account-{number}")).exists()
                })
            {
                number += 1;
            }
            let id = format!("account-{number}");
            let maildir = config
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .map(|root| root.join("mail").join(&id));
            Account {
                id,
                maildir,
                ..Default::default()
            }
        } else {
            config.current_account()
        };
        let remote = account.remote.clone().unwrap_or_default();
        let values = [
            account.name.clone().unwrap_or_default(),
            account.email.clone().unwrap_or_default(),
            remote.username,
            String::new(),
            remote.imap_host,
            remote.imap_port.to_string(),
            remote.smtp_host,
            remote.smtp_port.to_string(),
            remote.inbox,
            remote.sent,
        ];
        let make_default = config.maildir.is_none()
            || config.default_account.as_deref() == Some(account.id.as_str());
        Self {
            mode,
            account,
            values,
            field: if mode == Mode::Login { 3 } else { 0 },
            make_default,
            error: None,
            busy: false,
            request: 0,
        }
    }

    pub fn move_field(&mut self, backwards: bool) {
        if self.mode == Mode::Login {
            return;
        }
        self.field = if backwards {
            (self.field + 10) % 11
        } else {
            (self.field + 1) % 11
        };
        self.error = None;
    }

    pub fn input(&mut self, text: &str) {
        if let Some(value) = self.values.get_mut(self.field) {
            value.extend(text.chars().filter(|character| !character.is_control()));
            self.error = None;
        }
    }

    pub fn build(&self) -> Result<Account, (usize, String)> {
        let mut account = self.account.clone();
        if self.mode != Mode::Login {
            let email = self.values[1].trim();
            if !email_address::EmailAddress::is_valid(email) {
                return Err((1, "Enter a valid email address.".into()));
            }
            account.name = Some(self.values[0].trim().to_owned());
            account.email = Some(email.to_owned());
            let mut remote = account.remote.clone().unwrap_or_else(|| RemoteConfig {
                password_env: format!(
                    "TERMAIL_{}_PASSWORD",
                    account.id.replace('-', "_").to_uppercase()
                ),
                ..Default::default()
            });
            remote.username = if self.values[2].trim().is_empty() {
                email.to_owned()
            } else {
                self.values[2].trim().to_owned()
            };
            remote.imap_host = self.values[4].trim().to_owned();
            remote.imap_port = self.values[5]
                .trim()
                .parse()
                .ok()
                .filter(|port| *port > 0)
                .ok_or((5, "Enter an IMAP port from 1 to 65535.".into()))?;
            remote.smtp_host = self.values[6].trim().to_owned();
            remote.smtp_port = self.values[7]
                .trim()
                .parse()
                .ok()
                .filter(|port| *port > 0)
                .ok_or((7, "Enter an SMTP port from 1 to 65535.".into()))?;
            remote.inbox = self.values[8].trim().to_owned();
            remote.sent = self.values[9].trim().to_owned();
            remote.validate().map_err(|error| (4, error))?;
            if let Some(old) = &account.remote
                && (old.imap_host != remote.imap_host
                    || old.imap_port != remote.imap_port
                    || old.username != remote.username
                    || old.inbox != remote.inbox)
            {
                return Err((4, "Add a new account to change the IMAP server, username or Inbox. This keeps cached mail separate.".into()));
            }
            account.remote = Some(remote);
        }
        let remote = account
            .remote
            .as_mut()
            .ok_or((3, "This account has no IMAP settings.".into()))?;
        if !self.values[3].is_empty() {
            remote.session_password = Some(std::sync::Arc::new(SessionPassword(
                zeroize::Zeroizing::new(self.values[3].clone()),
            )));
        }
        if remote.password().is_err() {
            return Err((
                3,
                "Enter the account password. It stays in memory until you quit.".into(),
            ));
        }
        if account.maildir.is_none() {
            return Err((
                0,
                "Start with --config PATH to choose where settings are saved.".into(),
            ));
        }
        Ok(account)
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let title = match self.mode {
            Mode::Add => " add account ",
            Mode::Edit => " account settings ",
            Mode::Login => " sign in ",
        };
        let block = Block::bordered()
            .title(title)
            .border_style(theme::border(true))
            .title_style(theme::title(true))
            .style(theme::text().bg(theme::SURFACE));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(inner);
        let hint = if self.mode == Mode::Login {
            format!(
                "{} · {}",
                self.account.email.as_deref().unwrap_or_default(),
                self.values[4]
            )
        } else {
            "IMAPS + SMTPS · settings are saved; passwords are not\nUsername defaults to your email address.".into()
        };
        frame.render_widget(
            Paragraph::new(hint)
                .style(theme::muted())
                .wrap(Wrap { trim: false }),
            rows[0],
        );
        let labels = [
            "Name",
            "Email",
            "Username",
            "Password",
            "IMAP host",
            "IMAP port",
            "SMTP host",
            "SMTP port",
            "Inbox",
            "Sent folder",
        ];
        let fields: Vec<usize> = if self.mode == Mode::Login {
            vec![3]
        } else {
            (0..11).collect()
        };
        let items: Vec<ListItem> = fields
            .iter()
            .map(|&index| {
                let (label, value) = if index == 10 {
                    (
                        "Use by default",
                        if self.make_default {
                            "[x]".into()
                        } else {
                            "[ ]".into()
                        },
                    )
                } else {
                    (
                        labels[index],
                        if index == 3 {
                            "•".repeat(self.values[3].chars().count())
                        } else {
                            self.values[index].clone()
                        },
                    )
                };
                let value: String = value
                    .chars()
                    .rev()
                    .take(rows[1].width.saturating_sub(18) as usize)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                ListItem::new(format!(
                    "{label:<15} {value}{}",
                    if index == self.field { "▏" } else { "" }
                ))
            })
            .collect();
        let mut selection = ListState::default()
            .with_selected(fields.iter().position(|index| *index == self.field));
        frame.render_stateful_widget(
            List::new(items)
                .style(theme::text())
                .highlight_style(theme::selection()),
            rows[1],
            &mut selection,
        );
        let message = if self.busy {
            "Checking IMAP and SMTP… No message is sent."
        } else {
            self.error.as_deref().unwrap_or(if self.mode == Mode::Edit {
                "Password stays in memory. Leave it blank to keep the current password or use password_env."
            } else {
                "Password stays in memory. You will sign in again after restarting unless password_env is set."
            })
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(theme::muted())
                .wrap(Wrap { trim: false }),
            rows[2],
        );
        let actions = if self.busy {
            "esc  Cancel setup"
        } else if self.mode == Mode::Login {
            "enter  Sign in · esc  Work offline"
        } else {
            "tab / shift+tab  Move · ctrl+s  Save and connect\nesc  Cancel · space  Toggle default account"
        };
        frame.render_widget(
            Paragraph::new(actions)
                .style(theme::muted())
                .wrap(Wrap { trim: false }),
            rows[3],
        );
    }
}

pub fn verify(account: &Account, cancelled: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("Sign-in cancelled.".into());
    }
    let remote = account
        .remote
        .as_ref()
        .ok_or("Set IMAP and SMTP details before connecting.")?;
    let mut session = crate::imap_client::connect(remote)?;
    let folders = session
        .examine(&remote.inbox)
        .and_then(|_| session.examine(&remote.sent));
    let _ = session.logout();
    folders.map_err(|_| "Unable to open Inbox or Sent. Check the folder names.")?;
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("Sign-in cancelled.".into());
    }
    crate::sender::check_connection(remote)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    pub fn filled_form(config: &Config) -> Form {
        let mut form = Form::new(config, Mode::Add);
        form.values[0] = "Test account".into();
        form.values[1] = "test@example.test".into();
        form.values[3] = "fake-password".into();
        form.values[4] = "localhost".into();
        form.values[6] = "localhost".into();
        form
    }

    #[test]
    fn validates_fields_and_defaults_username_to_email() {
        let config = Config {
            path: Some(std::env::temp_dir().join("termail-form-test/config.toml")),
            ..Default::default()
        };
        let mut form = filled_form(&config);
        let account = form.build().unwrap();
        assert_eq!(
            account.remote.as_ref().unwrap().username,
            "test@example.test"
        );
        assert_eq!(
            account.remote.as_ref().unwrap().password().unwrap(),
            "fake-password"
        );
        assert!(account.maildir.unwrap().ends_with("mail/account-1"));
        form.values[1] = "invalid".into();
        assert_eq!(form.build().unwrap_err().0, 1);
        form.values[1] = "test@example.test".into();
        form.values[5] = "65536".into();
        assert_eq!(form.build().unwrap_err().0, 5);
        form.values[5] = "993".into();
        form.values[3].clear();
        assert_eq!(form.build().unwrap_err().0, 3);
    }

    #[test]
    fn editing_cannot_rebind_an_existing_mail_cache() {
        let mut config = Config {
            path: Some(std::env::temp_dir().join("termail-form-test/config.toml")),
            ..Default::default()
        };
        config.accounts.push(filled_form(&config).build().unwrap());
        config.activate(0);
        let mut form = Form::new(&config, Mode::Edit);
        assert!(form.build().is_ok());
        form.values[4] = "other.example.test".into();
        assert!(form.build().unwrap_err().1.contains("new account"));
    }

    #[test]
    fn renders_masked_passwords_and_handles_small_terminals() {
        let config = Config::default();
        let mut form = filled_form(&config);
        form.field = 3;
        for (width, height) in [(80, 24), (40, 12), (12, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| form.draw(frame, frame.area()))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!text.contains("fake-password"));
            if width == 80 {
                assert!(text.contains("Password"));
                assert!(text.contains("•••"));
            }
        }
        form.mode = Mode::Login;
        form.move_field(false);
        assert_eq!(form.field, 3);
    }
}
