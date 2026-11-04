//! Optional diagnostics for connection problems. Messages shown in the UI stay short
//! and never include server text; the log keeps the underlying error when enabled with
//! `--log PATH` or `TMAIL_LOG`.

use std::{
    fmt::Display,
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn init(path: Option<PathBuf>) -> std::io::Result<()> {
    let Some(path) = path
        .or_else(|| std::env::var_os("TMAIL_LOG").map(PathBuf::from))
        .or_else(|| std::env::var_os("TERMAIL_LOG").map(PathBuf::from))
    else {
        return Ok(());
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let _ = FILE.set(Mutex::new(options.open(path)?));
    Ok(())
}

pub fn write(area: &str, detail: impl Display) {
    let Some(file) = FILE.get() else {
        return;
    };
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{time} {area}: {detail}");
    }
}

/// Maps an error to a user-facing message, logging the original first.
pub fn fail<E: Display>(message: impl Into<String>) -> impl FnOnce(E) -> String {
    let message = message.into();
    move |error| {
        write("error", format_args!("{message} ({error})"));
        message
    }
}
