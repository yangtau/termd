-- :checkhealth termd
local M = {}

function M.check()
  local health = vim.health
  local termd = require("termd")
  local cfg = termd.config

  health.start("termd")
  if vim.fn.has("nvim-0.9") == 1 then
    local v = vim.version()
    health.ok(("Neovim %d.%d.%d"):format(v.major, v.minor, v.patch))
  else
    health.error("Neovim 0.9 or newer is needed")
  end

  if vim.fn.executable(cfg.cmd) == 0 then
    health.error(("`%s` not found"):format(cfg.cmd), { "Install termd (see its README) or set `cmd` in setup()" })
  else
    local help = table.concat(vim.fn.systemlist({ cfg.cmd, "--help" }), "\n")
    if help:find("--serve", 1, true) then
      health.ok(("`%s` found: %s"):format(cfg.cmd, vim.fn.exepath(cfg.cmd)))
    else
      health.error(("`%s` is too old: it lacks --serve"):format(cfg.cmd), { "Update termd" })
    end
  end

  if vim.o.termguicolors then
    health.ok("'termguicolors' is on")
  else
    health.error("'termguicolors' is off", { "The preview tells the terminal which image to show by color: set termguicolors" })
  end

  local term, program = vim.env.TERM or "", vim.env.TERM_PROGRAM or ""
  if vim.env.KITTY_WINDOW_ID or term == "xterm-kitty" then
    health.ok("Running in kitty")
  elseif program == "ghostty" or term == "xterm-ghostty" then
    health.ok("Running in Ghostty")
  elseif program == "WezTerm" then
    health.error("WezTerm does not support Kitty Unicode placeholders, which the preview is drawn with")
  elseif vim.env.TMUX then
    health.warn("Inside tmux: the preview needs `set -g allow-passthrough on` and kitty or Ghostty outside")
  else
    health.warn(("Unknown terminal (%s): the preview needs Kitty Unicode placeholders (kitty, Ghostty)"):format(term))
  end
  if vim.env.SSH_CONNECTION or vim.env.SSH_TTY then
    health.warn("Over SSH images are sent inline; when the screen is busy stray characters can appear (CTRL-L redraws)")
  end

  local w, h = termd._cell_size()
  if w then
    health.ok(("Cell size: %dx%d pixels"):format(w, h))
  else
    health.warn("The terminal does not report its cell size; 10x20 pixels is assumed", { "Set `cell_size` in setup()" })
  end
end

return M
