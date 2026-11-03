//! Shell completions and the manual page, printed by `termail completions SHELL` and
//! `termail man`. Written by hand to match the options in `cli.rs`.

pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        "fish" => Some(FISH),
        _ => None,
    }
}

const BASH: &str = r#"# termail completion for bash. Load with: source <(termail completions bash)
_termail() {
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
complete -o filenames -F _termail termail
"#;

const ZSH: &str = r#"#compdef termail
# termail completion for zsh. Save as _termail somewhere in $fpath.
_termail() {
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
_termail "$@"
"#;

const FISH: &str = r#"# termail completion for fish. Save as ~/.config/fish/completions/termail.fish
complete -c termail -f
complete -c termail -n __fish_use_subcommand -a doctor -d 'check the setup'
complete -c termail -n __fish_use_subcommand -a oauth -d 'sign in with OAuth2'
complete -c termail -n __fish_use_subcommand -a completions -d 'print shell completions'
complete -c termail -n __fish_use_subcommand -a man -d 'print the manual page'
complete -c termail -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
complete -c termail -l demo -d 'open the built-in mailbox'
complete -c termail -l maildir -r -F -d 'open a Maildir mailbox'
complete -c termail -l config -r -F -d 'load a config file'
complete -c termail -l log -r -F -d 'append connection errors to a file'
complete -c termail -s h -l help -d 'show the options'
complete -c termail -s V -l version -d 'show the version'
"#;

pub fn man_page() -> String {
    MAN.replace("@VERSION@", env!("CARGO_PKG_VERSION"))
}

const MAN: &str = r#".TH TERMAIL 1 "" "termail @VERSION@" "User Commands"
.SH NAME
termail \- mail in the terminal
.SH SYNOPSIS
.B termail
[\fB\-\-config\fR \fIPATH\fR] [\fB\-\-log\fR \fIPATH\fR] [\fB\-\-demo\fR | \fB\-\-maildir\fR \fIPATH\fR]
.br
.B termail doctor
.br
.B termail oauth
[\fIACCOUNT\fR]
.br
.B termail completions
\fBbash\fR|\fBzsh\fR|\fBfish\fR
.br
.B termail man
.SH DESCRIPTION
termail reads and sends mail from the terminal. It keeps mail in a Maildir, syncs
it with IMAP servers in the background, and sends through SMTP or a program such as
msmtp. Conversations show as a list and as a map of who answered whom.
.PP
With no account set up, termail opens a form that finds the server settings from
the address. Press \fB?\fR inside termail to see every key.
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
.I ~/.config/termail/config.toml
Settings and accounts. termail notices when it changes and applies it. Run
\fB:reload config\fR to do that by hand.
.TP
.I ~/.config/termail/init.lua
A Lua script that can add keys, commands and rules.
.TP
.I ~/.config/termail/contacts.vcf
The address book, as vCard.
.TP
.I ~/.config/termail/locales/LANG.toml
A translation of the interface, or changes to a built-in one.
.PP
On macOS the folder is \fI~/Library/Application Support/com.tunakilic.termail\fR.
.SH ENVIRONMENT
.TP
.B TERMAIL_CONFIG
The config file when \fB\-\-config\fR is not given.
.TP
.B TERMAIL_LOG
The log file when \fB\-\-log\fR is not given.
.TP
.B EDITOR
The editor for message bodies, unless the config names one.
.TP
.B NO_COLOR
When set, termail uses the terminal's own colours unless the config picks a theme.
.TP
.BR LANG ", " LC_ALL ", " LC_MESSAGES
Pick the interface language when \fBui.language\fR is \fBauto\fR.
.SH SEE ALSO
.BR gpg (1),
.BR msmtp (1),
.BR maildir (5)
"#;
