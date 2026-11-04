-- Counts unread mail older than a day in the status line.
-- Needs "read" for termail.mails, "interface" for termail.status,
-- "commands" for :overdue and "navigate" for termail.search.

local day = 24 * 60 * 60

local function overdue()
  local count = 0
  for _, mail in ipairs(termail.mails("is:unread")) do
    if mail.time and os.time() - mail.time > day then
      count = count + 1
    end
  end
  return count
end

termail.status(function()
  local count = overdue()
  if count > 0 then
    return count .. " waiting over a day"
  end
end)

termail.command("overdue", function()
  termail.search("is:unread before:1d")
end)
