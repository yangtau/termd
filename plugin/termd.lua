-- Commands for reading Markdown with termd; see :help termd.
if vim.g.loaded_termd then
  return
end
vim.g.loaded_termd = true

local function command(name, fn, desc)
  vim.api.nvim_create_user_command(name, function()
    require("termd")[fn]()
  end, { desc = desc })
end

command("TermdRead", "read", "Read the buffer: the page over its window, until q or an edit")
command("TermdClose", "close", "Stop reading the buffer, and termd")
