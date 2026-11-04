-- Counts unread mail older than a day in the status line.
-- Needs "read" for tmail.mails, "interface" for tmail.status,
-- "commands" for :overdue and "navigate" for tmail.search.

local day = 24 * 60 * 60

local function overdue()
  local count = 0
  for _, mail in ipairs(tmail.mails("is:unread")) do
    if mail.time and os.time() - mail.time > day then
      count = count + 1
    end
  end
  return count
end

tmail.status(function()
  local count = overdue()
  if count > 0 then
    return count .. " waiting over a day"
  end
end)

tmail.command("overdue", function()
  tmail.search("is:unread before:1d")
end)
