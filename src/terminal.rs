//! Entering and leaving the full-screen interface, and keeping the terminal usable
//! when something panics.

use std::{io::stdout, panic::PanicHookInfo, sync::OnceLock};

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
};
use ratatui::DefaultTerminal;

type Hook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// The panic hook that was in place before the interface started (color-eyre's).
static REPORT: OnceLock<Hook> = OnceLock::new();

/// Remembers the reporting hook. Call once, before the first `enter`.
pub fn install() {
    let _ = REPORT.set(std::panic::take_hook());
}

/// Switches to the full-screen interface with mouse and paste support.
pub fn enter() -> std::io::Result<DefaultTerminal> {
    let terminal = ratatui::init();
    if crate::settings::with(|ui| ui.mouse) {
        execute!(stdout(), EnableMouseCapture)?;
    }
    execute!(stdout(), EnableBracketedPaste)?;
    // ratatui chains its own restoring hook on every init; replace it with one that
    // also turns the mouse off and ignores panics on worker threads.
    let _ = std::panic::take_hook();
    std::panic::set_hook(Box::new(handle_panic));
    Ok(terminal)
}

/// Leaves the full-screen interface.
pub fn leave() -> std::io::Result<()> {
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::restore();
    Ok(())
}

fn handle_panic(info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
    if thread.name() != Some("main") {
        // A worker thread failed. Its caller sees the error; the screen stays as it is.
        crate::log::write(
            "panic",
            format_args!("{} · {info}", thread.name().unwrap_or("worker")),
        );
        return;
    }
    let _ = leave();
    if let Some(report) = REPORT.get() {
        report(info);
    }
}
