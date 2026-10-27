//! Command-line options.

use super::*;

pub(crate) struct CliOptions {
    pub(crate) source: Option<MailboxSource>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) doctor: bool,
    pub(crate) log: Option<PathBuf>,
}

pub(crate) fn parse_options(
    args: impl IntoIterator<Item = OsString>,
) -> color_eyre::Result<Option<CliOptions>> {
    let mut args = args.into_iter();
    let mut source = None;
    let mut config_path = None;
    let mut doctor = false;
    let mut log = None;

    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--maildir") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--maildir needs a path"))?;
                source = Some(MailboxSource::Maildir(PathBuf::from(path)));
            }
            Some("--config") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--config needs a path"))?;
                config_path = Some(PathBuf::from(path));
            }
            Some("--log") => {
                let path = args
                    .next()
                    .ok_or_else(|| color_eyre::eyre::eyre!("--log needs a path"))?;
                log = Some(PathBuf::from(path));
            }
            Some("--demo") => source = Some(MailboxSource::Demo),
            Some("doctor") => doctor = true,
            Some("--help" | "-h") => return Ok(None),
            Some(value) => return Err(color_eyre::eyre::eyre!("unknown option: {value}")),
            None => return Err(color_eyre::eyre::eyre!("option is not valid utf-8")),
        }
    }

    Ok(Some(CliOptions {
        source,
        config_path,
        doctor,
        log,
    }))
}

pub(crate) fn print_help() {
    println!("termail [doctor] [--config PATH] [--log PATH] [--demo | --maildir PATH]");
    println!();
    println!("  --demo          open the built-in mailbox");
    println!("  --maildir PATH  open a Maildir mailbox");
    println!("  --config PATH   load a config file");
    println!("  --log PATH      append connection errors to a file (or set TERMAIL_LOG)");
    println!("  doctor          check account, Maildir, editor, and sender setup");
}

pub(crate) fn expand_path(value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/")
        && let Some(base) = BaseDirs::new()
    {
        return base.home_dir().join(relative);
    }
    PathBuf::from(value)
}
