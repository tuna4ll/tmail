-- An example init.lua for termail.
-- Copy it next to your config.toml and keep the parts you like.

-- Keys -----------------------------------------------------------------------

-- e archives and # deletes, as in many other mail clients.
termail.map("e", "archive")
termail.map("#", "trash")

-- Star the message and jump to the next conversation.
termail.map("S", function()
  local mail = termail.selected()
  if not mail then return end
  termail.mark(mail.id, { starred = true })
  termail.run("next")
end)

-- In the reader, space goes to the next conversation instead of paging.
termail.map("space", function() termail.run("next") end, { mode = "reader" })

-- Commands -------------------------------------------------------------------

-- :unread shows only unread mail in this folder.
termail.command("unread", function()
  termail.search("is:unread")
end)

-- :from ada shows everything from ada.
termail.command("from", function(who)
  if who == "" then
    termail.notice("Usage: :from NAME")
  else
    termail.search("from:" .. who)
  end
end)

-- :standup starts the daily update.
termail.command("standup", function()
  termail.compose({
    to = "team@example.com",
    subject = "Standup " .. os.date("%Y-%m-%d"),
    body = "Yesterday:\n\nToday:\n\nBlocked on:\n",
  })
end)

-- Filing new mail ------------------------------------------------------------

termail.on("new_mail", function(mail)
  if mail.address:match("@github%.com$") then
    termail.move(mail.id, "GitHub")
  elseif mail.list and mail.subject:lower():match("newsletter") then
    termail.mark(mail.id, { read = true })
    termail.archive(mail.id)
  end
end)

-- Before sending -------------------------------------------------------------

termail.on("before_send", function(message)
  if message.subject == "" then
    return false, "Add a subject before sending."
  end
end)

-- Looks ----------------------------------------------------------------------

-- Unread count and folder at the start of the bottom line.
termail.status(function()
  local state = termail.state()
  if state.unread == 0 then return nil end
  return state.unread .. " unread"
end)

-- Show the number of messages after the subject.
termail.format(function(row)
  if row.count > 1 then
    return row.subject .. "  (" .. row.count .. ")", nil
  end
  return row.subject, nil
end)

-- Light colors during the day, dark ones at night.
local function follow_the_sun()
  local hour = tonumber(os.date("%H"))
  local day = hour >= 8 and hour < 19
  termail.theme({ preset = day and "light" or "dark" })
end
follow_the_sun()
termail.every(600, follow_the_sun)
