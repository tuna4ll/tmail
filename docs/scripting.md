# Scripting termail with Lua

Settings cover the common cases. When you want termail to do something it does not do out of the box, write a little Lua.

termail runs `init.lua` from the same folder as your config file when it starts. You can point it somewhere else with `script` in the `[ui]` table. Type `:script reload` to run it again after editing, without restarting.

The script uses Lua 5.4. All of the standard library is there, including `io` and `os`, so a script can read files and run programs. Only run scripts you trust.

A working example lives in [examples/init.lua](examples/init.lua).

## A first script

```lua
-- Archive with e, like many other mail clients.
termail.map("e", "archive")

-- Star the selected message and move on to the next conversation.
termail.map("S", function()
  local mail = termail.selected()
  if mail then
    termail.mark(mail.id, { starred = true })
    termail.run("next")
  end
end)

-- A command you can type as :standup
termail.command("standup", function()
  termail.compose({
    to = "team@example.com",
    subject = "Standup " .. os.date("%Y-%m-%d"),
    body = "Yesterday:\n\nToday:\n\nBlocked on:\n",
  })
end)
```

If something in the script is wrong, termail still starts. The error and its line number show at the bottom of the screen. Anything the script set up before the error keeps working.

## Keys

`termail.map(key, what, options)`

`key` is written the same way as in the `[keys]` table: `"e"`, `"ctrl+t"`, `"alt+x"`, `"f5"`.

`what` is either the name of an action (see the list in [configuration.md](configuration.md#keys-keys)), a command starting with a colon such as `":sync"`, or a function.

`options.mode` limits where the key works:

- `normal` (the default) works everywhere outside dialogs and text fields
- `inbox` only while the conversation list has focus
- `map` only on the conversation map
- `reader` only while a message is open

```lua
termail.map("o", ":open maildir ~/Mail/archive")
termail.map("space", function() termail.run("next") end, { mode = "reader" })
```

Script keys are checked before the built-in ones, so you can also take over a key termail already uses.

## Commands

`termail.command(name, function(arguments) ... end)` adds `:name`. Whatever follows the name is passed in as one string.

```lua
termail.command("from", function(who)
  termail.search("from:" .. who)
end)
```

## Events

`termail.on(event, function(...) ... end)` runs your function when something happens. You can register several functions for the same event.

| Event | Your function gets |
| --- | --- |
| `startup` | nothing |
| `new_mail` | a message, once for each new unread message after a sync |
| `open` | the message just opened |
| `folder` | `{ name = "Archive" }` when you switch folders |
| `sync` | `{ account, summary, downloaded }` after each sync |
| `before_send` | the message about to be sent |

### Filing new mail

```lua
termail.on("new_mail", function(mail)
  if mail.address:match("@github%.com$") then
    termail.move(mail.id, "GitHub")
  elseif mail.list and mail.subject:match("^%[announce%]") then
    termail.mark(mail.id, { read = true })
    termail.archive(mail.id)
  end
end)
```

### Checking a message before it goes out

`before_send` gets a table with `from`, `to`, `cc`, `bcc`, `subject` and `body`. Change any of them and the message is sent with your changes. Return `false` and a reason to stop it; the composer stays open and shows the reason.

```lua
termail.on("before_send", function(message)
  if message.to:match("@competitor%.com") then
    return false, "That address is outside the company."
  end
  -- Keep a copy of everything in a shared archive.
  if message.bcc == "" then
    message.bcc = "archive@example.com"
  end
end)
```

## Messages

Functions that hand you a message give a table with these fields:

| Field | |
| --- | --- |
| `id` | The message ID. Pass it to `move`, `mark` and the others. |
| `from` | Name and address, as shown. |
| `address` | Just the sender's address, in lower case. |
| `to`, `cc` | Lists of recipients. |
| `subject` | |
| `body` | The plain text. Not filled in by `termail.mails()`. |
| `links` | Links found in the message. Not filled in by `termail.mails()`. |
| `folder` | `INBOX`, `Archive`, `Sent` and so on. |
| `account` | The account id in the all-accounts view. |
| `unread`, `starred` | |
| `time` | Seconds since 1970, for comparing. |
| `date` | The date as termail shows it. |
| `attachments` | How many. |
| `list` | Whether it came from a mailing list. |
| `html` | Whether it has an HTML part. |

## Doing things

| Function | |
| --- | --- |
| `termail.selected()` | The selected message, or `nil`. |
| `termail.mails(query)` | Every loaded message that matches a search, such as `"from:ada is:unread"`. Without a query, all of them. |
| `termail.state()` | `{ folder, account, query, count, unread }` |
| `termail.move(id, folder)` | Moves a message. |
| `termail.archive(id)`, `termail.trash(id)` | Shortcuts for the two common moves. |
| `termail.mark(id, { read = true, starred = false })` | Leave out what you do not want to change. |
| `termail.open(id)` | Opens a message in the reader. |
| `termail.folder(name)` | Shows a folder. |
| `termail.search(query)` | Searches the inbox, as if typed after `/`. |
| `termail.compose({ to, subject, body })` | Opens the composer with these filled in. |
| `termail.run(name)` | Runs an action by name, or a command such as `":sync"`. |
| `termail.notice(text)` | Shows a line at the bottom of the screen. |
| `termail.notify(title, body)` | A desktop notification. |
| `termail.log(text)` | Writes to the log file, when one is set with `--log`. |

Moves and marks made by a script can be undone with `u`, the same as your own.

## Timers

`termail.every(seconds, function() ... end)` runs a function again and again. The shortest interval is one second.

```lua
-- Remind me about unread mail every half hour.
termail.every(1800, function()
  local unread = #termail.mails("in:INBOX is:unread")
  if unread > 0 then
    termail.notify("Unread mail", unread .. " messages are waiting")
  end
end)
```

## Settings and colors

`termail.set(name, value)` changes any setting from [configuration.md](configuration.md) while termail runs. Names from `[ui]` can be written bare. Settings from the other tables take a prefix: `compose.` or `reader.`.

```lua
termail.set("inbox_width", 40)
termail.set("compose.reply_prefix", "AW: ")
termail.set("reader.remote_images_from", { "@github.com" })

termail.notice("Sorted by " .. termail.get("sort"))
```

A wrong value is an error with a message that says what was wrong, and the old value stays.

`termail.theme(colors)` changes colors the same way the `[theme]` table does:

```lua
-- Dark during the night, light during the day.
local function by_hour()
  local hour = tonumber(os.date("%H"))
  termail.theme({ preset = (hour >= 8 and hour < 19) and "light" or "dark" })
end
by_hour()
termail.every(600, by_hour)
```

## Changing what you see

### The bottom line

`termail.status(function() return text end)` puts your own text at the start of the bottom line. It runs every time the screen is drawn, so keep it quick.

```lua
termail.status(function()
  local state = termail.state()
  return state.unread .. " unread in " .. state.folder
end)
```

### Conversation rows

`termail.format(function(row) return title, subtitle end)` decides the two lines of each conversation in the list. `row` has `subject`, `from`, `date`, `time`, `count`, `unread`, `starred`, `folder` and `account`, plus `title` and `subtitle` with termail's own text. Return `nil` for a line to keep termail's version.

```lua
termail.format(function(row)
  local count = row.count > 1 and (" (" .. row.count .. ")") or ""
  return row.subject .. count, row.from .. "  " .. row.date
end)
```

termail remembers each result until the conversation changes, so formatting stays fast even in a large mailbox.

## Limits

A script call that runs longer than two seconds is stopped, and the bottom line says so. Calls made while drawing, from `status` and `format`, get 50 milliseconds. This keeps a stuck loop from freezing termail.

Scripts run one at a time on the same thread as the interface, and termail waits while they run. For slow work such as a web request, start it in the background instead of waiting for it, for example `os.execute("long-task &")` on Linux and macOS.

Do not use `print` in a script. termail owns the screen, and printed text lands in the middle of it. Use `termail.notice` or `termail.log` instead.
