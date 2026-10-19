use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};

pub fn unique_name(prefix: &str) -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{}-{nonce}-{sequence}", std::process::id())
}

pub fn create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = create(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    sync_parent(path)
}

pub fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)?;
    sync_parent(to)?;
    sync_parent(from)
}
