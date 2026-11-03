# termail

A mail client for the terminal. Next to the usual list of conversations, termail draws each thread as a map, so you can see who answered whom at a glance.

## What it does

- Syncs with any IMAP server in the background and sends over SMTP, or through a program such as msmtp. Mail is kept in a local Maildir, so it reads fine offline and other tools can use it too.
- Sets up accounts from just your address. Gmail, Outlook, iCloud, Fastmail, Proton Mail Bridge and many others work out of the box, OAuth2 included. Passwords stay in your system keyring.
- Shows HTML mail the way it was designed, in a window you can move, resize and scroll with the mouse. Remote images stay blocked until you ask for them.
- Signs and encrypts with OpenPGP or S/MIME through your own GnuPG, with Autocrypt, key lookup at the recipient's domain and hidden subjects.
- Warns you about links that lead somewhere other than they claim and about senders your mail server could not verify.
- Answers meeting invitations, keeps an address book in a vCard file, saves drafts as you type and searches thousands of messages instantly.
- Lets you change nearly everything in one TOML file, applied as soon as you save it, and anything beyond that with a Lua script.
- Speaks English and Turkish, and has a mode for screen readers and magnifiers.

## Install

You need a recent Rust toolchain (edition 2024).

```sh
cargo install --path .
```

For signing and encryption, install GnuPG (`gpg`, and `gpgsm` for S/MIME).

## First start

Run `termail`. With no config yet, it asks for your address, finds the servers and checks the account before saving anything. To look around first, try the built-in mailbox:

```sh
termail --demo
```

Or the sandbox in this repository, which needs no mail server:

```sh
cargo run -- --config sandbox/config.toml
```

Inside termail, `c` writes a message, `r` replies, `g` shows folders, `p` switches accounts, `/` searches and `?` lists every key.

## Configuration

Everything is described in [docs/configuration.md](docs/configuration.md). For scripting, see [docs/scripting.md](docs/scripting.md). `termail doctor` checks your setup without connecting to a server.

To get tab completion and `man termail`:

```sh
termail completions bash > ~/.local/share/bash-completion/completions/termail
termail man > ~/.local/share/man/man1/termail.1
```

zsh and fish work the same way.

## Development

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt -- --check
```

A few tests talk to a real mail server. CI runs them against GreenMail with `cargo test -- --ignored real_server`.

## License

MIT
