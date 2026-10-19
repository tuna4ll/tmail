# termail

A terminal email client with a conversation map. Work in progress.

Currently uses local Maildir mailboxes and an external send command. Built-in server sync is not available yet.

Try the demo:

```sh
cargo run
```

Try the local sandbox (no mail server needed):

```sh
cargo run -- --config sandbox/config.toml
```

Use `c` to compose, `r` to reply, `g` for folders, `p` for accounts, and `?` for help.
Open a message in Drafts with Enter to continue editing.

Development checks:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt -- --check
```

MIT licensed.
