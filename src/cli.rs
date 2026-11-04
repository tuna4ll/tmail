//! Command-line options.

use super::*;

pub(crate) struct CliOptions {
    pub(crate) source: Option<MailboxSource>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) doctor: bool,
    pub(crate) log: Option<PathBuf>,
    /// `oauth [ACCOUNT]`: sign in with OAuth2 and store the refresh token.
    pub(crate) oauth: Option<Option<String>>,
    /// Something to print instead of starting: completions, the manual or the version.
    pub(crate) print: Option<String>,
}

pub(crate) fn parse_options(
    args: impl IntoIterator<Item = OsString>,
) -> color_eyre::Result<Option<CliOptions>> {
    let mut args = args.into_iter();
    let mut source = None;
    let mut config_path = None;
    let mut doctor = false;
    let mut log = None;
    let mut oauth = None;
    let mut print = None;

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
            Some("completions") => {
                let shell = args
                    .next()
                    .and_then(|shell| shell.into_string().ok())
                    .unwrap_or_default();
                let script = crate::completions::script(&shell).ok_or_else(|| {
                    color_eyre::eyre::eyre!("completions needs a shell: bash, zsh or fish")
                })?;
                print = Some(script.to_owned());
            }
            Some("man") => print = Some(crate::completions::man_page()),
            Some("--version" | "-V") => {
                print = Some(format!("tmail {}\n", env!("CARGO_PKG_VERSION")));
            }
            Some("doctor") => doctor = true,
            Some("oauth") => oauth = Some(None),
            Some(value) if oauth == Some(None) && !value.starts_with('-') => {
                oauth = Some(Some(value.to_owned()));
            }
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
        oauth,
        print,
    }))
}

pub(crate) fn print_help() {
    println!(
        "tmail [doctor | oauth [ACCOUNT] | completions SHELL | man] [--config PATH] [--log PATH] [--demo | --maildir PATH]"
    );
    println!();
    println!("  --demo          open the built-in mailbox");
    println!("  --maildir PATH  open a Maildir mailbox");
    println!("  --config PATH   load a config file");
    println!("  --log PATH      append connection errors to a file (or set TMAIL_LOG)");
    println!("  doctor          check account, Maildir, editor, and sender setup");
    println!("  oauth [ACCOUNT] sign in through the provider's page and keep the refresh token");
    println!("  completions SHELL  print completions for bash, zsh or fish");
    println!("  man             print the manual page");
    println!("  --version       show the version");
}

pub(crate) fn expand_path(value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/")
        && let Some(base) = BaseDirs::new()
    {
        return base.home_dir().join(relative);
    }
    PathBuf::from(value)
}
