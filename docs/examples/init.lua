-- An example init.lua for tmail.
-- Copy it next to your config.toml and keep the parts you like.

-- Keys -----------------------------------------------------------------------

-- e archives and # deletes, as in many other mail clients.
tmail.map("e", "archive")
tmail.map("#", "trash")

-- Star the message and jump to the next conversation.
tmail.map("S", function()
  local mail = tmail.selected()
  if not mail then return end
  tmail.mark(mail.id, { starred = true })
  tmail.run("next")
end)

-- In the reader, space goes to the next conversation instead of paging.
tmail.map("space", function() tmail.run("next") end, { mode = "reader" })

-- Commands -------------------------------------------------------------------

-- :unread shows only unread mail in this folder.
tmail.command("unread", function()
  tmail.search("is:unread")
end)

-- :from ada shows everything from ada.
tmail.command("from", function(who)
  if who == "" then
    tmail.notice("Usage: :from NAME")
  else
    tmail.search("from:" .. who)
  end
end)

-- :standup starts the daily update.
tmail.command("standup", function()
  tmail.compose({
    to = "team@example.com",
    subject = "Standup " .. os.date("%Y-%m-%d"),
    body = "Yesterday:\n\nToday:\n\nBlocked on:\n",
  })
end)

-- Filing new mail ------------------------------------------------------------

tmail.on("new_mail", function(mail)
  if mail.address:match("@github%.com$") then
    tmail.move(mail.id, "GitHub")
  elseif mail.list and mail.subject:lower():match("newsletter") then
    tmail.mark(mail.id, { read = true })
    tmail.archive(mail.id)
  end
end)

-- Before sending -------------------------------------------------------------

tmail.on("before_send", function(message)
  if message.subject == "" then
    return false, "Add a subject before sending."
  end
end)

-- Looks ----------------------------------------------------------------------

-- Unread count and folder at the start of the bottom line.
tmail.status(function()
  local state = tmail.state()
  if state.unread == 0 then return nil end
  return state.unread .. " unread"
end)

-- Show the number of messages after the subject.
tmail.format(function(row)
  if row.count > 1 then
    return row.subject .. "  (" .. row.count .. ")", nil
  end
  return row.subject, nil
end)

-- Light colors during the day, dark ones at night.
local function follow_the_sun()
  local hour = tonumber(os.date("%H"))
  local day = hour >= 8 and hour < 19
  tmail.theme({ preset = day and "light" or "dark" })
end
follow_the_sun()
tmail.every(600, follow_the_sun)
