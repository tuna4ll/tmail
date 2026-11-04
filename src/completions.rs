//! Shell completions and the manual page, printed by `tmail completions SHELL` and
//! `tmail man`. Written by hand to match the options in `cli.rs`.

pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        "fish" => Some(FISH),
        _ => None,
    }
}

const BASH: &str = r#"# tmail completion for bash. Load with: source <(tmail completions bash)
_tmail() {
    local current previous
    current="${COMP_WORDS[COMP_CWORD]}"
    previous="${COMP_WORDS[COMP_CWORD-1]}"
    case "$previous" in
        --config|--log|--maildir)
            COMPREPLY=($(compgen -f -- "$current"))
            return ;;
        completions)
            COMPREPLY=($(compgen -W "bash zsh fish" -- "$current"))
            return ;;
    esac
    COMPREPLY=($(compgen -W "doctor oauth completions man --demo --maildir --config --log --help --version" -- "$current"))
}
complete -o filenames -F _tmail tmail
"#;

const ZSH: &str = r#"#compdef tmail
# tmail completion for zsh. Save as _tmail somewhere in $fpath.
_tmail() {
    _arguments \
        '--demo[open the built-in mailbox]' \
        '--maildir[open a Maildir mailbox]:folder:_files -/' \
        '--config[load a config file]:file:_files' \
        '--log[append connection errors to a file]:file:_files' \
        '(- *)--help[show the options]' \
        '(- *)--version[show the version]' \
        '1:command:((doctor\:"check the setup" oauth\:"sign in with OAuth2" completions\:"print shell completions" man\:"print the manual page"))' \
        '*::argument:->argument'
    case $state in
        argument)
            [[ $words[1] == completions ]] && _values shell bash zsh fish ;;
    esac
}
_tmail "$@"
"#;

const FISH: &str = r#"# tmail completion for fish. Save as ~/.config/fish/completions/tmail.fish
complete -c tmail -f
complete -c tmail -n __fish_use_subcommand -a doctor -d 'check the setup'
complete -c tmail -n __fish_use_subcommand -a oauth -d 'sign in with OAuth2'
complete -c tmail -n __fish_use_subcommand -a completions -d 'print shell completions'
complete -c tmail -n __fish_use_subcommand -a man -d 'print the manual page'
complete -c tmail -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
complete -c tmail -l demo -d 'open the built-in mailbox'
complete -c tmail -l maildir -r -F -d 'open a Maildir mailbox'
complete -c tmail -l config -r -F -d 'load a config file'
complete -c tmail -l log -r -F -d 'append connection errors to a file'
complete -c tmail -s h -l help -d 'show the options'
complete -c tmail -s V -l version -d 'show the version'
"#;

pub fn man_page() -> String {
    MAN.replace("@VERSION@", env!("CARGO_PKG_VERSION"))
}

const MAN: &str = r#".TH TMAIL 1 "" "tmail @VERSION@" "User Commands"
.SH NAME
tmail \- mail in the terminal
.SH SYNOPSIS
.B tmail
[\fB\-\-config\fR \fIPATH\fR] [\fB\-\-log\fR \fIPATH\fR] [\fB\-\-demo\fR | \fB\-\-maildir\fR \fIPATH\fR]
.br
.B tmail doctor
.br
.B tmail oauth
[\fIACCOUNT\fR]
.br
.B tmail completions
\fBbash\fR|\fBzsh\fR|\fBfish\fR
.br
.B tmail man
.SH DESCRIPTION
tmail reads and sends mail from the terminal. It keeps mail in a Maildir, syncs
it with IMAP servers in the background, and sends through SMTP or a program such as
msmtp. Conversations show as a list and as a map of who answered whom.
.PP
With no account set up, tmail opens a form that finds the server settings from
the address. Press \fB?\fR inside tmail to see every key.
.SH COMMANDS
.TP
.B doctor
Checks the accounts, the Maildir, the editor and the way mail is sent, then exits.
.TP
.BI oauth " [ACCOUNT]"
Signs in through the provider's page and keeps the refresh token in the system
keyring.
.TP
.BI completions " SHELL"
Prints completions for bash, zsh or fish.
.TP
.B man
Prints this page.
.SH OPTIONS
.TP
.BI \-\-config " PATH"
Loads this config file instead of the default one.
.TP
.BI \-\-maildir " PATH"
Opens a Maildir folder directly.
.TP
.B \-\-demo
Opens the built-in sample mailbox.
.TP
.BI \-\-log " PATH"
Appends connection errors to a file.
.TP
.BR \-h ", " \-\-help
Shows the options.
.TP
.BR \-V ", " \-\-version
Shows the version.
.SH KEYS
.TP
.B j k enter esc
Move through the list, open and close a message.
.TP
.B c r A F
Write, reply, reply to all, forward.
.TP
.B a d J m s u
Archive, move to Trash, mark as spam, read or unread, star, undo.
.TP
.B / g p :
Search, folders, accounts, commands.
.TP
.B K
Add the sender to the address book.
.TP
.B ?
Show every key.
.SH FILES
.TP
.I ~/.config/tmail/config.toml
Settings and accounts. tmail notices when it changes and applies it. Run
\fB:reload config\fR to do that by hand.
.TP
.I ~/.config/tmail/init.lua
A Lua script that can add keys, commands and rules.
.TP
.I ~/.config/tmail/contacts.vcf
The address book, as vCard.
.TP
.I ~/.config/tmail/locales/LANG.toml
A translation of the interface, or changes to a built-in one.
.PP
On macOS the folder is \fI~/Library/Application Support/com.tunakilic.tmail\fR.
.SH ENVIRONMENT
.TP
.B TMAIL_CONFIG
The config file when \fB\-\-config\fR is not given.
.TP
.B TMAIL_LOG
The log file when \fB\-\-log\fR is not given.
.TP
.B EDITOR
The editor for message bodies, unless the config names one.
.TP
.B NO_COLOR
When set, tmail uses the terminal's own colours unless the config picks a theme.
.TP
.BR LANG ", " LC_ALL ", " LC_MESSAGES
Pick the interface language when \fBui.language\fR is \fBauto\fR.
.SH SEE ALSO
.BR gpg (1),
.BR msmtp (1),
.BR maildir (5)
"#;
