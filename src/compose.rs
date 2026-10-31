//! Starting, editing and sending drafts.

use super::*;

impl App {
    pub(crate) fn start_draft(&mut self, start: draft::Start) {
        let reply = start != draft::Start::New;
        if matches!(self.source, MailboxSource::Demo) {
            self.notice = Some("Open a Maildir mailbox to save drafts.".into());
            return;
        }
        let mail = reply
            .then(|| self.active_mail_id())
            .flatten()
            .and_then(|id| self.mails.iter().find(|mail| mail.id == id));
        if reply && mail.is_none() {
            self.notice = Some("Select a message first.".into());
            return;
        }
        let account = match &self.source {
            MailboxSource::Unified(_) => Some(
                mail.and_then(|mail| self.account_index(mail))
                    .unwrap_or_else(|| self.config.active_account_index()),
            ),
            _ => None,
        };
        let (_, config) = self.compose_context(account);
        // Unified views prefix message IDs with the account; replies need the real ones.
        let opened = mail.and_then(|mail| match self.opened.get(&mail.id) {
            Some(Ok((content, _))) => mail_parser::MessageParser::default()
                .parse(content)
                .and_then(|message| message.body_text(0).map(|text| text.into_owned())),
            _ => None,
        });
        let encrypted = mail.is_some_and(|mail| {
            matches!(
                mail.security,
                Some(crypto::Kind::PgpEncrypted | crypto::Kind::SmimeEncrypted)
            )
        });
        let original = mail.map(|mail| {
            let mut mail = mail.clone();
            // Quote the decrypted text, never the ciphertext.
            if let Some(text) = &opened {
                mail.body = text.clone();
            }
            if let Some(prefix) = mail.account.as_ref().map(|account| format!("{account}:")) {
                let strip = |value: &str| value.strip_prefix(&prefix).unwrap_or(value).to_owned();
                mail.id = strip(&mail.id);
                mail.reply_to = mail.reply_to.as_deref().map(strip);
                mail.references = mail.references.iter().map(|id| strip(id)).collect();
            }
            mail
        });
        match draft::Composer::start(
            start,
            original.as_ref(),
            config.identity().as_deref(),
            config.signature.as_deref(),
        ) {
            Ok(composer) => {
                let forward = start == draft::Start::Forward;
                let state = ComposeState {
                    field: if reply && !forward {
                        ComposeState::BODY_FIELD
                    } else {
                        0
                    },
                    composer,
                    error: None,
                    account,
                    suggestion: None,
                    protection: {
                        let mut protection = crypto::Protection::from_config(&config.crypto);
                        if encrypted {
                            // Answers to encrypted mail stay encrypted.
                            protection.encrypt = true;
                            protection.standard =
                                match original.as_ref().and_then(|mail| mail.security) {
                                    Some(crypto::Kind::SmimeEncrypted) => crypto::Standard::Smime,
                                    _ => crypto::Standard::OpenPgp,
                                };
                        }
                        protection
                    },
                };
                if reply && !forward && self.config.uses_builtin_editor() {
                    self.open_body_editor(state);
                } else if reply && !forward {
                    self.pending_editor = Some(state);
                } else {
                    self.overlay = Some(Overlay::Compose(state));
                }
            }
            Err(error) => self.notice = Some(format!("Unable to create draft: {error}")),
        }
    }

    pub(crate) fn account_index(&self, mail: &Mail) -> Option<usize> {
        let account = mail.account.as_deref()?;
        self.config
            .accounts
            .iter()
            .position(|candidate| candidate.id == account)
    }

    /// The mailbox and settings a draft is saved and sent with.
    pub(crate) fn compose_context(&self, account: Option<usize>) -> (MailboxSource, Config) {
        let mut config = self.config.clone();
        match account {
            Some(index) if config.activate(index) => (
                config
                    .maildir
                    .clone()
                    .map_or(MailboxSource::Demo, MailboxSource::Maildir),
                config,
            ),
            _ => (self.source.clone(), config),
        }
    }

    pub(crate) fn handle_compose_key(&mut self, mut state: ComposeState, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match state.composer.summary(&base) {
                Ok(summary) => self.overlay = Some(Overlay::DraftReady { state, summary }),
                Err(error) => {
                    state.error = Some(error.to_string());
                    self.overlay = Some(Overlay::Compose(state));
                }
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.finish_composer(state, false);
            return;
        }
        match key.code {
            KeyCode::Esc => self.overlay = Some(Overlay::ComposeClose(state)),
            KeyCode::Tab if state.suggestion.is_some() => {
                if let (Some(suggestion), Some(value)) =
                    (state.suggestion.take(), state.value_mut())
                {
                    *value = suggestion;
                }
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Tab | KeyCode::Down => {
                state.move_next();
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::BackTab | KeyCode::Up => {
                state.move_previous();
                self.suggest(&mut state);
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if state.field == ComposeState::FORMAT_FIELD =>
            {
                state.composer.format.toggle();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Left if state.field == ComposeState::SECURITY_FIELD => {
                state.protection.previous();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Right | KeyCode::Char(' ') if state.field == ComposeState::SECURITY_FIELD => {
                state.protection.next();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Enter if state.field == ComposeState::BODY_FIELD => {
                if self.config.uses_builtin_editor() {
                    self.open_body_editor(state);
                } else {
                    self.pending_editor = Some(state);
                }
            }
            KeyCode::Char('e' | 'E')
                if state.field == ComposeState::BODY_FIELD
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.open_body_editor(state);
            }
            KeyCode::Enter if state.field == ComposeState::FORMAT_FIELD => {
                state.composer.format.toggle();
                state.move_next();
                self.overlay = Some(Overlay::Compose(state));
            }
            KeyCode::Enter => {
                state.move_next();
                self.overlay = Some(Overlay::Compose(state));
            }
            _ => {
                if let Some(value) = state.value_mut()
                    && edit_text(value, &mut self.cursor_back, key)
                {
                    state.error = None;
                    self.suggest(&mut state);
                }
                self.overlay = Some(Overlay::Compose(state));
            }
        }
    }

    pub(crate) fn suggest(&self, state: &mut ComposeState) {
        state.suggestion = match state.field {
            0..=2 => state
                .value_mut()
                .and_then(|value| complete::address(value, &self.contacts)),
            ComposeState::FILES_FIELD => {
                let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                complete::path(&state.composer.attachments, &base)
            }
            _ => None,
        };
    }

    pub(crate) fn edit_composer_body(
        &mut self,
        mut state: ComposeState,
    ) -> color_eyre::Result<DefaultTerminal> {
        terminal::leave()?;
        let result = self.run_editor(&state.composer.body_path);
        let terminal = terminal::enter()?;

        if let Err(error) = result {
            state.error = Some(format!(
                "Unable to open $EDITOR: {error}. Check editor in config.toml."
            ));
        }
        self.overlay = Some(Overlay::Compose(state));
        Ok(terminal)
    }

    /// Runs a job that may prompt in the terminal, with the UI suspended around it.
    pub(crate) fn run_terminal_job(
        &mut self,
        job: TerminalJob,
    ) -> color_eyre::Result<DefaultTerminal> {
        terminal::leave()?;
        let crypto = self.config.crypto.clone();
        let result = match &job {
            TerminalJob::Protect { state, contents } => {
                crypto::protect(contents, state.protection, &crypto)
                    .map(|message| (message, String::new()))
            }
            TerminalJob::Open(mail_id) => self
                .mails
                .iter()
                .find(|mail| &mail.id == mail_id)
                .and_then(|mail| Some((mail.source_path.clone()?, mail.security?)))
                .ok_or_else(|| "The message is no longer available.".to_owned())
                .and_then(|(path, kind)| {
                    let contents = fs::read(path).map_err(|error| error.to_string())?;
                    crypto::open(&contents, kind, &crypto)
                        .map(|opened| (opened.content, opened.status))
                }),
        };
        let terminal = terminal::enter()?;
        self.clear_screen = true;
        match job {
            TerminalJob::Protect { mut state, .. } => match result {
                Ok((message, _)) => self.complete_composer(*state, true, message),
                Err(error) => {
                    state.error = Some(format!("{error} Nothing was sent."));
                    self.overlay = Some(Overlay::Compose(*state));
                }
            },
            TerminalJob::Open(mail_id) => self.show_opened(mail_id, result),
        }
        Ok(terminal)
    }

    /// Edits the body inside termail.
    pub(crate) fn open_body_editor(&mut self, state: ComposeState) {
        let text = fs::read_to_string(&state.composer.body_path).unwrap_or_default();
        self.overlay = Some(Overlay::BodyEditor {
            state: Box::new(state),
            editor: widgets::editor::Editor::new(&text),
        });
    }

    pub(crate) fn handle_body_editor_key(
        &mut self,
        mut state: Box<ComposeState>,
        mut editor: widgets::editor::Editor,
        key: KeyEvent,
    ) {
        match editor.handle_key(key) {
            widgets::editor::Outcome::Done => {
                if editor.modified
                    && let Err(error) = fs::write(&state.composer.body_path, editor.text())
                {
                    state.error = Some(format!("Unable to save the body: {error}"));
                }
                self.overlay = Some(Overlay::Compose(*state));
            }
            _ => self.overlay = Some(Overlay::BodyEditor { state, editor }),
        }
    }

    pub(crate) fn run_editor(&self, path: &Path) -> color_eyre::Result<()> {
        let command = self.config.editor_command();
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| color_eyre::eyre::eyre!("editor command is empty"))?;
        let status = Command::new(program).args(arguments).arg(path).status()?;
        if !status.success() {
            return Err(color_eyre::eyre::eyre!("editor exited with {status}"));
        }
        Ok(())
    }

    pub(crate) fn finish_composer(&mut self, mut state: ComposeState, send: bool) {
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let contents = if send {
            state.composer.message(&base)
        } else {
            state.composer.draft_message(&base)
        };
        let contents = match contents {
            Ok(contents) => contents,
            Err(error) => {
                state.error = Some(error.to_string());
                self.overlay = Some(Overlay::Compose(state));
                return;
            }
        };
        if send && state.protection.is_active() {
            // Signing may ask for a passphrase, so it runs with the terminal restored.
            self.pending_terminal = Some(TerminalJob::Protect {
                state: Box::new(state),
                contents,
            });
            return;
        }
        self.complete_composer(state, send, contents);
    }

    /// Queues a finished message for delivery or saves it as a draft.
    pub(crate) fn complete_composer(
        &mut self,
        mut state: ComposeState,
        send: bool,
        contents: Vec<u8>,
    ) {
        let (source, config) = self.compose_context(state.account);
        if send {
            let delivery = match delivery::Delivery::queue(
                source.clone(),
                config.delivery_identity(),
                config.send_command.clone(),
                &contents,
            ) {
                Ok(delivery) => delivery,
                Err(error) => {
                    state.error = Some(format!(
                        "Unable to queue message: {error}. Nothing was sent."
                    ));
                    self.overlay = Some(Overlay::Compose(state));
                    return;
                }
            };
            if let Some(original) = &state.composer.original_draft
                && let Err(error) = fs::remove_file(original)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                self.notice = Some(format!(
                    "Message queued but not sent: unable to remove the original draft: {error}. Retry from Outbox."
                ));
                self.reload();
                return;
            }
            self.mark_answered(state.composer.replying_to(), state.account);
            self.start_delivery(delivery);
        } else {
            let saved = match &state.composer.original_draft {
                Some(path) if path.exists() => source.update_draft(path, &contents),
                _ => source.save_draft(&contents),
            };
            match saved {
                Ok(_) => {
                    let _ = state.composer.remove_body();
                    self.notice = Some("Draft saved".into());
                }
                Err(error) => {
                    state.error = Some(format!(
                        "Unable to save draft: {error}. Check the Maildir and retry."
                    ));
                    self.overlay = Some(Overlay::Compose(state));
                    return;
                }
            }
        }
        self.reload();
    }

    pub(crate) fn mark_answered(&mut self, id: Option<&str>, account: Option<usize>) {
        let Some(id) = id else {
            return;
        };
        let account = account.and_then(|index| self.config.accounts.get(index));
        let id = match account {
            Some(account) => format!("{}:{id}", account.id),
            None => id.to_owned(),
        };
        if let Some(index) = self.mails.iter().position(|mail| mail.id == id)
            && let Ok(Some(change)) = self.source.set_answered(&self.mails[index])
        {
            self.mails[index].source_path = Some(change.after_path().to_path_buf());
            self.refresh_fingerprint();
        }
    }
}
