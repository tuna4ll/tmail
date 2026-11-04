# Configuring tmail

tmail reads one TOML file at start. Everything in it is optional. With no file at all you get the demo mailbox and a prompt to add an account.

This page covers every setting. If you want to change behaviour beyond what settings allow, see [scripting.md](scripting.md).

## Where the file lives

tmail looks in these places, in order:

1. the path given with `--config PATH`
2. the `TMAIL_CONFIG` environment variable
3. the default location for your system:
   - Linux and BSD: `~/.config/tmail/config.toml`
   - macOS: `~/Library/Application Support/com.tunakilic.tmail/config.toml`
   - Windows: `%APPDATA%\tunakilic\tmail\config\config.toml`

Relative paths inside the file (for `maildir`, `download_dir` and `ui.script`) are read relative to the folder the config file is in.

After editing, run `tmail doctor`. It checks the file, your Maildir, your editor and how mail gets sent, without connecting to any server.

A mistake in the file stops tmail at start with a message that names the problem. Inside the tables described here, such as `[ui]` or `[remote]`, an unknown name is an error too, so a typo does not go unnoticed.

You don't have to restart after an edit. tmail notices when the file is saved and applies it within a couple of seconds: colours, keys, language, the `[ui]`, `[compose]`, `[reader]` and `[crypto]` tables, your script, names, signatures and aliases. Type `:reload config` to do it by hand. If the new file has a mistake, tmail says so in the status line and keeps the old settings. Changes to how an account connects, such as a server name or a Maildir path, wait for the next start so that running syncs and signed-in sessions are not cut off; tmail tells you when that is the case.

## A small example

```toml
name = "Ada Lovelace"
email = "ada@example.com"
maildir = "~/Mail/personal"

[remote]
imap_host = "imap.example.com"
smtp_host = "smtp.example.com"
username = "ada@example.com"
keyring = true

[ui]
inbox_width = 36
confirm_quit = true

[keys]
archive = "e"
```

Start tmail, sign in once when it asks, and the password is kept in your system keyring from then on.

## Identity and local mail

| Setting | What it does |
| --- | --- |
| `name` | Your name, shown to the people you write to. |
| `email` | Your address. Also used to recognise your own messages. |
| `signature` | Added below new messages and replies. Use `\n` for line breaks. |
| `maildir` | The folder that holds your mail, in Maildir format. tmail creates it if needed. |
| `download_dir` | Where saved attachments go. Defaults to your Downloads folder. |
| `editor` | The program that edits message bodies, as a list: `["nvim", "+set tw=72"]`. Defaults to `$EDITOR`. Use `["builtin"]` for the editor inside tmail. |
| `send_command` | A program that sends mail, such as `["msmtp", "-a", "work"]`. Only for accounts without `[remote]`. |
| `notifications` | Set to `false` to turn off desktop notifications for new mail. |
| `aliases` | Other addresses you send from, such as `["Ada <ada@alias.example>", "info@example.com"]`. |
| `contacts` | The address book file. Defaults to `contacts.vcf` next to the config. |

### Other addresses

When a message reached you through one of your `aliases`, the reply goes out from that address. Reply all leaves all your own addresses out. In the composer, press `ctrl+o` to go through your addresses on the From line.

### The address book

The address book is a plain vCard file, so you can import it from or export it to other programs. Press `K` on a message to add its sender. Type `:contacts` to see the book: type to filter, `enter` writes to the selected person and `ctrl+x` removes them. Addresses from the book come first when the composer suggests recipients, followed by everyone you have exchanged mail with.

### Drafts

While you write, tmail saves the message to Drafts every 15 seconds once something changed, and the composer shows when it last did. If tmail or your terminal closes, the draft is waiting in Drafts. When you discard a message, the copy saved this way is deleted too.

## Several accounts

Put each account in its own `[[accounts]]` table. Every account needs an `id` and its own `maildir`. The account settings are the same as the identity settings above, plus an optional `[accounts.remote]` table.

```toml
default_account = "personal"

[[accounts]]
id = "personal"
name = "Ada"
email = "ada@example.com"
maildir = "~/Mail/personal"

[accounts.remote]
imap_host = "imap.example.com"
smtp_host = "smtp.example.com"
username = "ada@example.com"
keyring = true

[[accounts]]
id = "work"
email = "ada@work.example"
maildir = "~/Mail/work"
send_command = ["msmtp", "-a", "work"]
```

Press `p` inside tmail to switch accounts or to see all of them together.

## Adding an account in tmail

You rarely need to write the server settings yourself. Press `p` and then `a`, or start tmail without a config, and type your address. When you press tab, tmail fills in the servers:

- for well-known providers (Gmail, Outlook, iCloud, Yahoo, Fastmail, GMX, Proton Mail Bridge and others) from a list built into tmail, without going online
- otherwise from Thunderbird's provider database, the domain's own autoconfig file, its DNS records, or its mail servers, which also finds Google Workspace and Microsoft 365 on your own domain

Online lookups send only the domain of your address, never the address or a password, to `autoconfig.thunderbird.net`, to the domain itself, and to Cloudflare's DNS resolver. Press `ctrl+f` to look again, or type the servers in yourself.

The form also lets you pick the sign-in type (a password, or OAuth2 for Google, Microsoft or another provider) and the security of each server. `ctrl+r` shows what you typed in the password fields. tmail checks the account before saving it and never sends a message while doing so.

If a server is rejected because of its certificate, as with Proton Mail Bridge or a server you run yourself, the form shows the certificate's SHA-256 fingerprint. Compare it with the one your server shows, then press `ctrl+t` to trust it. tmail saves it in `certs/` next to the config and checks again.

When the server turns your password down, tmail shows the reason it gave, with a hint for the usual fixes such as an app password. It stops trying that account, so a wrong password never gets the account locked, and asks you to sign in again. Other accounts that need a password are marked "sign in needed" in the account list; choose one and press `l`.

## Server settings: `[remote]`

With a `[remote]` table tmail syncs the account over IMAP and sends over SMTP by itself. You do not need `send_command` then, and setting both is an error.

| Setting | Default | What it does |
| --- | --- | --- |
| `imap_host` | | The IMAP server name or IP address (IPv6 works too), without `imaps://` or a port. |
| `imap_port` | `993` | |
| `imap_security` | `auto` | `tls`, `starttls` or `auto`. Auto uses STARTTLS on ports 143, 587 and 25 and TLS everywhere else. |
| `smtp_host` | | The SMTP server name. |
| `smtp_port` | `465` | |
| `smtp_security` | `auto` | Same choices as `imap_security`. |
| `username` | | Usually your address. |
| `auth` | `password` | `password` or `xoauth2`. |
| `inbox` | `INBOX` | The server's name for the inbox. |
| `sent` | | The Sent folder name, if tmail does not find it by itself. |
| `sync_interval_secs` | `60` | How often to sync, at least 30. With `0` tmail syncs once at start and then only when you type `:sync`. |
| `batch_size` | `200` | How many older messages to download per sync, per folder. Newest first. |
| `timeout_secs` | `30` | How long to wait for the server before giving up. |
| `watch` | the inbox | Folders to watch for new mail as it arrives (IMAP IDLE): `["INBOX", "Lists"]`. Each one keeps a connection open. |
| `max_download_bytes` | `0` | Messages larger than this arrive as headers only. Press `D` on one to download the rest. `0` downloads everything. |
| `smtp_username` | | A different username for SMTP, for the few providers that need one. Its password comes from `smtp_password_env`, `smtp_password_command` or the keyring, like the main one. |
| `tls_certificate` | | A PEM file with certificates to trust for this account, besides the ones your system trusts. tmail writes it when you trust a certificate in the account form. |

### Passwords

tmail never stores a password in the config file. Pick one of these:

- `keyring = true` keeps it in the system keyring (Secret Service on Linux, Keychain on macOS, Credential Manager on Windows). tmail asks once and remembers.
- `password_command = ["pass", "show", "mail/ada"]` runs a program and uses the first line it prints. The result is cached for `password_cache_secs` seconds (300 by default), so a GPG prompt does not appear every minute.
- `password_env = "MAIL_PASSWORD"` reads an environment variable.

With none of these, tmail asks for the password each time it starts.

### OAuth2 for Gmail and Outlook

Some providers no longer accept passwords. For them, set `auth = "xoauth2"` and add an `oauth2` table:

```toml
[remote]
imap_host = "imap.gmail.com"
smtp_host = "smtp.gmail.com"
username = "ada@gmail.com"
auth = "xoauth2"
keyring = true

[remote.oauth2]
provider = "gmail"
client_id = "1234-abcd.apps.googleusercontent.com"
client_secret = "your-client-secret"
```

`provider` can be `gmail` or `outlook`. For anything else, give `auth_url`, `token_url` and `scope` yourself. You need a client ID from your provider's developer console. Register it as a desktop app.

Then sign in from the account form, which opens your browser, or run:

```sh
tmail oauth
```

Your browser opens the provider's sign-in page. If it does not, the form shows the link and `ctrl+y` copies it. After you approve, tmail keeps the refresh token in the keyring and gets new access tokens by itself. With several accounts, name the one you mean: `tmail oauth work`.

### The first sync

If you point tmail at a Maildir that already has mail, the first sync does not copy those messages to the server. tmail tells you how many it kept back. Type `:upload` when you do want them on the server. Messages you save or send after that sync normally.

## Signing and encryption: `[crypto]`

tmail signs and encrypts with your own GnuPG setup: `gpg` for OpenPGP and `gpgsm` for S/MIME. Your keys stay where they are.

| Setting | Default | What it does |
| --- | --- | --- |
| `sign` | `false` | New messages start signed. |
| `encrypt` | `false` | New messages start encrypted. |
| `smime` | `false` | Use S/MIME instead of OpenPGP by default. |
| `key` | your address | The key or certificate to sign with. |
| `gpg` | `gpg` | Path to the OpenPGP program. |
| `gpgsm` | `gpgsm` | Path to the S/MIME program. |
| `home` | | A GnuPG home folder other than your usual one. |
| `autocrypt` | `false` | Send your public key with every message and save the keys others send this way. |
| `wkd` | `true` | Before encrypting, look up missing recipient keys at their domain. |

You can change the choice for each message on the Security line of the composer. Encrypted messages are encrypted to you as well, so you can read your own Sent copy. Replies to encrypted mail start encrypted.

When you open a signed message, the reader shows who signed it and whether the key is verified. Encrypted messages are decrypted when you open them; if your key has a passphrase, the usual GnuPG prompt appears.

The subject of an encrypted message travels inside the encryption. Servers and anyone in between see `...` instead. When you open the message, tmail shows the real subject, in the reader and in the list. Other programs that support protected headers, such as Thunderbird, do the same.

With `autocrypt = true`, every message you send carries your public key in an `Autocrypt` header, so the other side's mail program can encrypt its replies without any key exchange. When you open a message that carries a key this way, tmail saves it in your keyring if the key belongs to the sender's address, and tells you when a new one arrives.

If you encrypt to someone whose key you don't have, tmail asks their domain for it first (the Web Key Directory). Many providers publish keys this way, Proton Mail and Posteo among them. The lookup gives up after a few seconds. Set `wkd = false` if you would rather not ask.

## Interface: `[ui]`

| Setting | Default | What it does |
| --- | --- | --- |
| `inbox_width` | `30` | Width of the conversation list. You can also drag its border with the mouse. |
| `reader_width` | `44` | Width of the reader beside the map, with `reader.layout = "docked"`. |
| `split_min_width` | `78` | On narrower terminals only one pane shows at a time. |
| `dock_min_width` | `112` | With `reader.layout = "docked"`, the reader sits beside the map on terminals at least this wide. |
| `reader_max_width` | `0` | The widest the reader window opens, in columns. `0` means no limit. |
| `time_format` | `%H:%M` | Dates for today's mail. |
| `day_format` | `%-d %b` | Dates for mail from this year. |
| `date_format` | `%-d %b %Y` | Dates for older mail. |
| `long_date_format` | `%a %-d %b %Y %H:%M` | The date in the message header. |
| `mark_read_on_open` | `true` | Opening a message marks it read. |
| `confirm_quit` | `false` | Press `q` twice to quit. |
| `show_hints` | `true` | Show key hints at the bottom. |
| `mouse` | `true` | Use the mouse. Turn off if you want your terminal's own text selection. |
| `sort` | `newest` | `newest` or `oldest` conversation first. |
| `thread_by_subject` | `true` | Group replies that lack reply headers by their subject. |
| `start_folder` | `INBOX` | The folder shown at start. |
| `start_filter` | `all` | `all`, `unread` or `starred`. |
| `scroll_lines` | `3` | Lines per mouse wheel step. |
| `page_lines` | `10` | Lines per page in the reader. |
| `unread_marker` | `●` | Shown before unread conversations. |
| `star_marker` | `★` | Shown before starred conversations. |
| `hide_folders` | `[]` | Folders to leave out of the folder list. |
| `notify_command` | | Your own program for new-mail notifications. `{title}` and `{body}` are filled in: `["dunstify", "{title}", "{body}"]`. |
| `script` | `init.lua` | The Lua script to run at start. See [scripting.md](scripting.md). |
| `language` | `auto` | The interface language: `en`, `tr`, or `auto` to follow your system. |
| `accessible` | `false` | Settings for screen readers and magnifiers, described below. |

Date formats use the usual strftime codes. `%d.%m.%Y` gives `14.03.2026`, `%b %e` gives `Mar 14`.

### Language

tmail comes in English and Turkish. With `language = "auto"` it follows `LC_ALL`, `LC_MESSAGES` or `LANG`, and uses English for languages it doesn't have.

To translate tmail into another language, or to change some of the built-in words, create `locales/CODE.toml` next to the config, for example `locales/de.toml`, and set `language = "de"`. Each line maps the English text to the translation:

```toml
"archive" = "archivieren"
"Moved to {}" = "Nach {} verschoben"
"Deleted {} messages permanently" = "{} Nachrichten endgültig gelöscht"
```

`{}` stands for the part that changes, such as a number or a folder name. Anything you leave out stays English. The Turkish file in the source tree, `locales/tr.toml`, is a good list of texts to start from.

### Accessibility

`accessible = true` helps when you use a screen reader or a magnifier:

- the terminal cursor sits on the selected message, so your tools follow it
- unread and starred conversations are marked with `N` and `*` instead of symbols, unless you set your own markers
- the loading animation is left out
- colours are left to your terminal, with the selection shown as reversed text, unless you pick a theme preset

tmail also follows the [NO_COLOR](https://no-color.org) convention: when the `NO_COLOR` environment variable is set, it uses your terminal's colours unless the config names a preset.

## Writing mail: `[compose]`

| Setting | Default | What it does |
| --- | --- | --- |
| `format` | `plain` | `plain`, `html` or `markdown` for new messages. You can switch per message with space on the Format line. |
| `reply_prefix` | `Re: ` | Put in front of the subject of replies. |
| `forward_prefix` | `Fwd: ` | Put in front of the subject of forwards. |
| `attribution` | `On {date}, {from} wrote:` | The line above quoted text. |
| `quote_prefix` | `> ` | Put in front of each quoted line. |
| `large_attachment_mb` | `18` | Warn before sending more than this in attachments. |

A German setup might use `reply_prefix = "AW: "` and `attribution = "{from} schrieb am {date}:"`.

### Writing in Markdown

With the Markdown format you write the body in Markdown and tmail sends two versions: an HTML one with headings, lists, tables, task lists, links and quotes laid out properly, and your Markdown text for people who read plain text. Code in a fenced block that names its language, such as ` ```rust `, is coloured. A line break you type stays a line break, as in chat apps, and the signature after `-- ` is left as you wrote it. Drafts keep the Markdown, so they open the same way again.

## Reading mail: `[reader]`

Messages open in a window over the list. Drag its title bar to move it, and its left or bottom edge or the `◢` corner to resize it. Double-click the title, or press `f`, for full screen. With the keyboard, `+` and `-` change the size, `ctrl` with the arrow keys resizes, `alt` with the arrow keys moves it, and `0` puts it back.

To scroll, use the wheel, drag the scroll bar on the right edge, click anywhere on it to jump there, or press on the page and drag it like a sheet of paper. `ctrl+d` and `ctrl+u` move half a window. Newsletters with a fixed width keep that width in a narrow window instead of being squeezed; scroll them sideways with the left and right arrow keys, `shift` and the wheel, or by dragging.

HTML mail is drawn the way its sender designed it: page and card backgrounds, coloured headers, buttons, tables with real columns, centred layouts and pictures where they belong. Links are clickable where they appear. Press `t` to switch between the message's own colours and your theme's.

| Setting | Default | What it does |
| --- | --- | --- |
| `html_colors` | `original` | `original` uses the message's colours. `theme` keeps your theme's background and only keeps coloured blocks and buttons. |
| `layout` | `popup` | `popup` opens messages in a window. `docked` puts the reader beside the conversation map on wide screens instead. |
| `popup_width` | `84` | Width of the window, in percent of the screen. |
| `popup_height` | `90` | Height of the window, in percent of the screen. |
| `remote_images` | `false` | Load remote images in every message. |
| `remote_images_from` | `[]` | Load them only for these senders. Full addresses or whole domains: `["@github.com", "friend@example.com"]`. |
| `clean_links` | `true` | Remove tracking parameters such as `utm_source` and `fbclid` from links before opening them. |

Remote images are blocked until you press `I`, because they tell the sender when and where you read the message. Pictures need a terminal that can show them, such as kitty, WezTerm, iTerm2, foot or Ghostty. Elsewhere you see their description instead.

### Checking senders and links

Your mail server checks whether a message really comes from the domain it claims (DKIM, SPF and DMARC) and writes down the result. tmail shows that result under the sender in the reader. When the check failed, or when the sender's name shows a different address than the one the mail comes from, the reader says so and the conversation gets a `⚠` in the list.

Before a link opens, tmail looks at where it really goes. It asks you first when the link text shows one site but leads to another, when the address hides its destination behind an `@`, points at a bare IP address, uses look-alike characters, or is not encrypted. Press `o` to open it anyway, `y` to copy it, or `esc` to leave it. Links that aren't web or mail links, such as `javascript:`, never open.

### Trackers

Many newsletters hide a tiny picture that tells the sender when, where and on which device you opened the message, and send every link through a click counter. tmail finds these pictures by their size, by being hidden, or by belonging to a known tracking service such as Mailchimp, HubSpot, SendGrid or Mailtrack, and never loads them, not even when you press `I`. The reader shows what it blocked and which services were involved, under "privacy". When a counted link carries its real destination in the address, tmail opens that destination directly, so the click is not counted.

### Meeting invitations

When a message carries a meeting invitation, the reader shows what it is, when and where, and who organises it. Type `:accept`, `:tentative` or `:decline` to answer. tmail prepares the answer for the organiser's calendar and opens it in the composer so you can add a note before sending. `:calendar` saves the event and opens it with your calendar program.

## Colors: `[theme]`

Start from a preset and change the colors you want:

```toml
[theme]
preset = "light"
accent = "#d75f00"
```

Presets are `dark` (the default), `light`, `contrast` and `mono`, which uses your terminal's own colours. The colors you can set are `canvas`, `surface`, `text`, `muted`, `subtle`, `accent`, `success` and `error`. Each takes a name (`red`, `lightblue`), a 256-color index (`81`) or a hex value (`#5fd7ff`).

## Keys: `[keys]`

Give any action a different key. The original key keeps working.

```toml
[keys]
archive = "e"
trash = "#"
toggle_star = "ctrl+s"
quit = "Q"
```

Keys are written as a single character (`e`, `E`, `#`), with `ctrl+` or `alt+` in front, or by name: `enter`, `tab`, `backspace`, `delete`, `home`, `end`, `pageup`, `pagedown`, `space`, `f1` to `f12`.

These are the actions you can rebind:

| Action | Key | | Action | Key |
| --- | --- | --- | --- | --- |
| `compose` | `c` | | `links` | `l` |
| `reply` | `r` | | `attachments` | `v` |
| `reply_all` | `A` | | `images` | `I` |
| `forward` | `F` | | `browser` | `O` |
| `archive` | `a` | | `download` | `D` |
| `trash` | `d` | | `unsubscribe` | `U` |
| `toggle_read` | `m` | | `reload` | `R` |
| `toggle_star` | `s` | | `next` | `n` |
| `undo` | `u` | | `previous` | `N` |
| `move` | `M` | | `retry` | `x` |
| `folders` | `g` | | `review` | `z` |
| `accounts` | `p` | | `quit` | `q` |
| `search` | `/` | | `help` | `?` |
| `command` | `:` | | `spam` | `J` |
| `contact` | `K` | | | |

For keys that run your own code, use a script.

## Searching

Type `/` in the inbox. Each word finds messages with a word that starts with it, in the sender, recipients, subject or body: `pay` finds "payment" and "Paying", but not "repay". Results appear as you type, even in very large mailboxes, because tmail keeps an index of every word. The index is saved in tmail's cache folder and updated in the background as mail arrives, so even the first search after a start is instant. You can also narrow it down:

```
from:ada  to:team  subject:invoice  body:"exact words"
in:Archive  in:all  is:unread  is:starred  has:attachment
before:2026-03-01  after:2026-02  on:today  after:7d
-word   leaves out messages containing word
```

Filters such as `from:` and `subject:` match anywhere in that field, so `subject:voice` finds "Invoice".

Dates can be a day (`2026-03-14`), a month (`2026-03`), `today`, `yesterday`, or an age like `7d`, `2w`, `3m` or `1y`.

tmail only downloads the newest messages of each folder. Press `ctrl+r` while searching to run the same search on the server, so older matches are downloaded too.

## Patches: `[patch]`

tmail understands patches sent by mail, the way Git and the Linux kernel work. When a message is part of a series such as `[PATCH v3 2/5]`, the reader says so and colours the diff. With a message of the series selected:

- `:patch apply` applies the whole series with `git am`, in order, using the newest version found in the conversation. It tells you when a part is missing. If `git am` stops on a conflict, finish it in the repository with `git am --continue` or `git am --abort`. `:patch apply ~/src/project` picks another repository for one time.
- `:patch save` writes the series as an mbox file to your download folder.
- `:patch b4` hands the message to [b4](https://b4.docs.kernel.org), which fetches the newest version of the series with its review tags and applies it.

`:lore ID` opens a whole thread from lore.kernel.org, by Message-ID or by a link to it. The thread opens as a mailbox of its own, so nothing is copied to your account or its server; press `p` to go back. You can reply and review from there as usual.

| Setting | Default | What it does |
| --- | --- | --- |
| `repo` | the folder tmail started in | The repository patches go into. |
| `am_args` | `["-3"]` | Arguments for `git am`, such as `["-3", "--signoff"]`. |
| `archive` | `https://lore.kernel.org/all` | The public-inbox archive `:lore` reads from. |

## Shell completions and the manual

tmail can print completions for your shell and a manual page:

```sh
tmail completions bash > ~/.local/share/bash-completion/completions/tmail
tmail completions zsh > ~/.zfunc/_tmail
tmail completions fish > ~/.config/fish/completions/tmail.fish
tmail man > ~/.local/share/man/man1/tmail.1
```

After that, `man tmail` works and tab completes the commands and options.
