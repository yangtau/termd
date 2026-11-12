-- Commands for previewing Markdown with termd; see :help termd.
if vim.g.loaded_termd then
  return
end
vim.g.loaded_termd = true

local function command(name, fn, desc)
  vim.api.nvim_create_user_command(name, function()
    require("termd")[fn]()
  end, { desc = desc })
end

command("TermdOpen", "open", "Preview the buffer with termd")
command("TermdClose", "close", "Close the buffer's termd preview")
command("TermdToggle", "toggle", "Open or close the buffer's termd preview")
