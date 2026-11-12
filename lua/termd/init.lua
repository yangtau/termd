-- Preview Markdown buffers with termd, inside a Neovim window.
--
-- termd renders the page (`termd --serve`) and hands over the images, as
-- Kitty graphics escape sequences that this plugin writes to the terminal.
-- The preview window holds text: Kitty Unicode placeholders, cells that the
-- terminal replaces with part of an image. A cell's foreground color names
-- the image and two combining marks name the row and column in it. So the
-- window scrolls, splits and redraws like any other; termd is only asked
-- for the rows that come into view.
--
-- Needs a terminal with Unicode placeholders (kitty, Ghostty) and
-- 'termguicolors'.

local M = {}

local uv = vim.uv or vim.loop

---@class termd.Config
local defaults = {
  -- The termd executable, and arguments for every preview (e.g. { "--sans" }).
  cmd = "termd",
  args = {},
  -- Where the preview window opens next to the buffer's: "right", "left",
  -- "bottom", "top" or "tab".
  position = "right",
  -- Width (or height, for "bottom" and "top") of the window in cells; half
  -- of the current window when nil.
  size = nil,
  -- Move the focus to the preview when it opens.
  focus = false,
  -- Preview the buffer as it is, not as it was last saved.
  live = true,
  -- Scroll the preview along with the cursor.
  follow = true,
  -- Milliseconds to wait after the last change before updating the preview.
  debounce = 100,
  -- Size of a terminal cell in pixels, { width, height }, when the
  -- terminal does not report it.
  cell_size = nil,
}

M.config = vim.deepcopy(defaults)

---@type table<integer, table> Open previews by source buffer.
local sessions = {}

local ns = vim.api.nvim_create_namespace("termd")

local function notify(msg, level)
  vim.schedule(function()
    vim.notify("termd: " .. msg, level or vim.log.levels.INFO)
  end)
end

-- Terminal output -------------------------------------------------------

--- The descriptor of the terminal: a Neovim that runs a UI points its
--- stdout at it.
local tty_fd
local function tty()
  if tty_fd == nil then
    tty_fd = false
    for _, fd in ipairs({ 1, 2 }) do
      if uv.guess_handle(fd) == "tty" then
        tty_fd = fd
        break
      end
    end
  end
  return tty_fd
end

local function write_now(data)
  local fd = tty()
  if not fd then
    return
  end
  -- Neovim's UI writes to the same terminal from its own process. One
  -- write() to a terminal is not interleaved with another's, but stdio
  -- would split a long sequence into several; so write it in one call.
  -- That is not enough when the terminal falls behind: the UI's writes are
  -- then cut short and ours can land inside one. So termd keeps sequences
  -- short (images go through shared memory or files) wherever it can.
  while #data > 0 do
    local n = uv.fs_write(fd, data, -1)
    if not n or n <= 0 then
      return
    end
    data = data:sub(n + 1)
  end
end

--- Write escape sequences to the terminal Neovim runs in.
function M._write(data)
  if vim.api.nvim_ui_send and pcall(vim.api.nvim_ui_send, data) then
    return
  end
  write_now(data)
end

--- Inside tmux a sequence for the terminal has to be passed through.
local function passthrough(seq)
  if not vim.env.TMUX then
    return seq
  end
  return "\27Ptmux;" .. seq:gsub("\27", "\27\27") .. "\27\\"
end

--- Free images `base` .. `base + count - 1` in the terminal.
local function free_images(base, count)
  local out = {}
  for id = base, base + count - 1 do
    out[#out + 1] = passthrough(("\27_Ga=d,d=I,q=2,i=%d\27\\"):format(id))
  end
  if #out > 0 then
    M._write(table.concat(out))
  end
end

--- Size of a terminal cell in pixels, from the terminal's window size.
local function cell_size()
  if M.config.cell_size then
    return M.config.cell_size[1], M.config.cell_size[2]
  end
  local ok, ffi = pcall(require, "ffi")
  if ok then
    pcall(ffi.cdef, [[
      struct termd_winsize { unsigned short row, col, xpixel, ypixel; };
      int ioctl(int fd, unsigned long request, ...);
    ]])
    local request = (jit and jit.os == "Linux") and 0x5413 or 0x40087468 -- TIOCGWINSZ
    local ws = ffi.new("struct termd_winsize")
    for _, fd in ipairs({ 1, 2, 0 }) do
      if ffi.C.ioctl(fd, request, ws) == 0 and ws.xpixel > 0 and ws.col > 0 and ws.row > 0 then
        return math.floor(ws.xpixel / ws.col), math.floor(ws.ypixel / ws.row)
      end
    end
  end
  return nil
end

-- Placeholders ----------------------------------------------------------

local PLACEHOLDER = vim.fn.nr2char(0x10EEEE)

-- The combining marks for row (and column) numbers, from kitty's
-- gen/rowcolumn-diacritics.txt.
-- stylua: ignore
local DIACRITICS = {
  0x0305,0x030D,0x030E,0x0310,0x0312,0x033D,0x033E,0x033F,0x0346,0x034A,0x034B,0x034C,
  0x0350,0x0351,0x0352,0x0357,0x035B,0x0363,0x0364,0x0365,0x0366,0x0367,0x0368,0x0369,
  0x036A,0x036B,0x036C,0x036D,0x036E,0x036F,0x0483,0x0484,0x0485,0x0486,0x0487,0x0592,
  0x0593,0x0594,0x0595,0x0597,0x0598,0x0599,0x059C,0x059D,0x059E,0x059F,0x05A0,0x05A1,
  0x05A8,0x05A9,0x05AB,0x05AC,0x05AF,0x05C4,0x0610,0x0611,0x0612,0x0613,0x0614,0x0615,
  0x0616,0x0617,0x0657,0x0658,0x0659,0x065A,0x065B,0x065D,0x065E,0x06D6,0x06D7,0x06D8,
  0x06D9,0x06DA,0x06DB,0x06DC,0x06DF,0x06E0,0x06E1,0x06E2,0x06E4,0x06E7,0x06E8,0x06EB,
  0x06EC,0x0730,0x0732,0x0733,0x0735,0x0736,0x073A,0x073D,0x073F,0x0740,0x0741,0x0743,
  0x0745,0x0747,0x0749,0x074A,0x07EB,0x07EC,0x07ED,0x07EE,0x07EF,0x07F0,0x07F1,0x07F3,
  0x0816,0x0817,0x0818,0x0819,0x081B,0x081C,0x081D,0x081E,0x081F,0x0820,0x0821,0x0822,
  0x0823,0x0825,0x0826,0x0827,0x0829,0x082A,0x082B,0x082C,0x082D,0x0951,0x0953,0x0954,
  0x0F82,0x0F83,0x0F86,0x0F87,0x135D,0x135E,0x135F,0x17DD,0x193A,0x1A17,0x1A75,0x1A76,
  0x1A77,0x1A78,0x1A79,0x1A7A,0x1A7B,0x1A7C,0x1B6B,0x1B6D,0x1B6E,0x1B6F,0x1B70,0x1B71,
  0x1B72,0x1B73,0x1CD0,0x1CD1,0x1CD2,0x1CDA,0x1CDB,0x1CE0,0x1DC0,0x1DC1,0x1DC3,0x1DC4,
  0x1DC5,0x1DC6,0x1DC7,0x1DC8,0x1DC9,0x1DCB,0x1DCC,0x1DD1,0x1DD2,0x1DD3,0x1DD4,0x1DD5,
  0x1DD6,0x1DD7,0x1DD8,0x1DD9,0x1DDA,0x1DDB,0x1DDC,0x1DDD,0x1DDE,0x1DDF,0x1DE0,0x1DE1,
  0x1DE2,0x1DE3,0x1DE4,0x1DE5,0x1DE6,0x1DFE,0x20D0,0x20D1,0x20D4,0x20D5,0x20D6,0x20D7,
  0x20DB,0x20DC,0x20E1,0x20E7,0x20E9,0x20F0,0x2CEF,0x2CF0,0x2CF1,0x2DE0,0x2DE1,0x2DE2,
  0x2DE3,0x2DE4,0x2DE5,0x2DE6,0x2DE7,0x2DE8,0x2DE9,0x2DEA,0x2DEB,0x2DEC,0x2DED,0x2DEE,
  0x2DEF,0x2DF0,0x2DF1,0x2DF2,0x2DF3,0x2DF4,0x2DF5,0x2DF6,0x2DF7,0x2DF8,0x2DF9,0x2DFA,
  0x2DFB,0x2DFC,0x2DFD,0x2DFE,0x2DFF,0xA66F,0xA67C,0xA67D,0xA6F0,0xA6F1,0xA8E0,0xA8E1,
  0xA8E2,0xA8E3,0xA8E4,0xA8E5,0xA8E6,0xA8E7,0xA8E8,0xA8E9,0xA8EA,0xA8EB,0xA8EC,0xA8ED,
  0xA8EE,0xA8EF,0xA8F0,0xA8F1,0xAAB0,0xAAB2,0xAAB3,0xAAB7,0xAAB8,0xAABE,0xAABF,0xAAC1,
  0xFE20,0xFE21,0xFE22,0xFE23,0xFE24,0xFE25,0xFE26,0x10A0F,0x10A38,0x1D185,0x1D186,0x1D187,
  0x1D188,0x1D189,0x1D1AA,0x1D1AB,0x1D1AC,0x1D1AD,0x1D242,0x1D243,0x1D244,
}

--- The text of row `row` (0-based) of an image `cols` cells wide. Only the
--- first cell names its row and column; the terminal counts on from there.
local function placeholder_row(row, cols)
  local first = PLACEHOLDER .. vim.fn.nr2char(DIACRITICS[row + 1]) .. vim.fn.nr2char(DIACRITICS[1])
  return first .. PLACEHOLDER:rep(cols - 1)
end
M._placeholder_row = placeholder_row

--- The highlight group whose foreground color is image `id`.
local function image_group(id)
  local name = "TermdImage" .. id
  vim.api.nvim_set_hl(0, name, { fg = id })
  return name
end

-- Sessions --------------------------------------------------------------

local function valid_win(win)
  return win and vim.api.nvim_win_is_valid(win)
end

--- The window showing the source buffer, preferring the one it was opened from.
local function source_win(s)
  if valid_win(s.swin) and vim.api.nvim_win_get_buf(s.swin) == s.buf then
    return s.swin
  end
  local win = vim.fn.bufwinid(s.buf)
  return win ~= -1 and win or nil
end

local function send(s, data)
  if s.job then
    pcall(vim.fn.chansend, s.job, data)
  end
end

local function send_text(s)
  local text = table.concat(vim.api.nvim_buf_get_lines(s.buf, 0, -1, false), "\n") .. "\n"
  send(s, ("text %d\n"):format(#text) .. text)
end

local function send_colors(s)
  local ok, normal = pcall(vim.api.nvim_get_hl, 0, { name = "Normal", link = false })
  local hex = function(c)
    return c and ("#%06x"):format(c) or "-"
  end
  if ok then
    send(s, ("colors %s %s\n"):format(hex(normal.bg), hex(normal.fg)))
  end
end

--- Tell termd the window's width and the cell size, if they changed.
local function send_size(s)
  if not valid_win(s.pwin) then
    return
  end
  local w, h = cell_size()
  if not w then
    if not s.warned_size then
      s.warned_size = true
      notify("the terminal does not report its cell size; guessing 10x20 pixels (set `cell_size`)", vim.log.levels.WARN)
    end
    w, h = 10, 20
  end
  local info = vim.fn.getwininfo(s.pwin)[1]
  local cols = math.max(1, info.width - info.textoff)
  local size = ("size %d %d %d\n"):format(cols, w, h)
  if size ~= s.size then
    s.size, s.cols = size, cols
    send(s, size)
  end
end

--- Tell termd which rows the window shows.
local function send_view(s)
  if not (s.doc and valid_win(s.pwin)) then
    return
  end
  local top, bottom = vim.fn.line("w0", s.pwin), vim.fn.line("w$", s.pwin)
  local view = ("show %d %d\n"):format(top - 1, bottom - 1)
  if view ~= s.view then
    s.view = view
    send(s, view)
  end
end

--- Ask termd something; `fn` gets the answer and the question's tag.
local function ask(s, what, n, fn)
  s.tag = s.tag + 1
  s.waiting[tostring(s.tag)] = fn
  send(s, ("%s %d %d\n"):format(what, s.tag, n))
  return s.tag
end

--- Scroll the preview to where the cursor is in the source window, at the
--- same height in the window.
local function follow(s)
  local win = source_win(s)
  if not (s.doc and win and valid_win(s.pwin)) or vim.api.nvim_get_current_win() == s.pwin then
    return
  end
  local line = vim.api.nvim_win_get_cursor(win)[1]
  -- (Assigned rather than returned: nvim_win_call can return a float as a table.)
  local frac = 0
  vim.api.nvim_win_call(win, function()
    frac = (vim.fn.winline() - 1) / math.max(1, vim.fn.winheight(0) - 1)
  end)
  s.want = ask(s, "where", line, function(row, tag)
    if tag ~= s.want or not valid_win(s.pwin) or vim.api.nvim_get_current_win() == s.pwin then
      return -- a newer question is on its way, or the preview is being used
    end
    local height = vim.api.nvim_win_get_height(s.pwin)
    local rows = vim.api.nvim_buf_line_count(s.pbuf)
    local lnum = math.max(1, math.min(rows, math.floor(row) + 1))
    local top = math.max(1, math.min(lnum - math.floor(frac * (height - 1) + 0.5), rows - height + 1))
    vim.api.nvim_win_call(s.pwin, function()
      vim.fn.winrestview({ topline = top, lnum = lnum, col = 0, leftcol = 0 })
    end)
    send_view(s)
  end)
end

--- Fill the preview buffer with placeholders for `rows` rows, in chunks of
--- `chunk` rows that are images `base`, `base + 1`, ...
local function fill(s, rows, chunk, base)
  local cols = s.cols
  local key = table.concat({ rows, chunk, base, cols }, " ")
  if key == s.filled then
    return
  end
  s.filled = key
  local templates = {}
  for r = 0, chunk - 1 do
    templates[r] = placeholder_row(r, cols)
  end
  local lines = {}
  for r = 0, rows - 1 do
    lines[r + 1] = templates[r % chunk]
  end
  vim.bo[s.pbuf].modifiable = true
  vim.api.nvim_buf_set_lines(s.pbuf, 0, -1, false, lines)
  vim.bo[s.pbuf].modifiable = false
  vim.bo[s.pbuf].modified = false
  vim.api.nvim_buf_clear_namespace(s.pbuf, ns, 0, -1)
  local chunks = math.ceil(rows / chunk)
  for c = 0, chunks - 1 do
    local last = math.min(rows, (c + 1) * chunk)
    vim.api.nvim_buf_set_extmark(s.pbuf, ns, c * chunk, 0, {
      end_row = last - 1,
      end_col = #lines[last],
      hl_group = image_group(base + c),
    })
  end
  s.chunks = math.max(s.chunks or 0, chunks)
end

--- A line from termd.
local function handle(s, line)
  local kind, rest = line:match("^(%S+) ?(.*)$")
  if kind == "G" then
    M._write(rest)
  elseif kind == "doc" then
    local rows, chunk, base = rest:match("^(%d+) (%d+) (%d+)")
    rows, chunk, base = tonumber(rows), tonumber(chunk), tonumber(base)
    if not (rows and valid_win(s.pwin)) then
      return
    end
    s.doc = { rows = rows, chunk = chunk, base = base }
    fill(s, rows, chunk, base)
    s.view = nil
    if M.config.follow then
      follow(s)
    end
    send_view(s)
  elseif kind == "at" or kind == "line" then
    local tag, value = rest:match("^(%S+) (%S+)")
    local fn = s.waiting[tag]
    s.waiting[tag] = nil
    if fn then
      fn(tonumber(value), tonumber(tag))
    end
  end
end

local function debounced(s, ms, fn)
  local timer = uv.new_timer()
  table.insert(s.timers, timer)
  return function()
    timer:stop()
    timer:start(ms, 0, vim.schedule_wrap(function()
      if sessions[s.buf] == s then
        fn()
      end
    end))
  end
end

--- The file termd is told about: links and images are found next to it.
local function document_path(buf)
  local name = vim.api.nvim_buf_get_name(buf)
  if name == "" then
    return (uv.cwd() or ".") .. "/untitled.md"
  end
  return vim.fn.fnamemodify(name, ":p")
end

local function close_session(s)
  if sessions[s.buf] ~= s then
    return
  end
  sessions[s.buf] = nil
  for _, timer in ipairs(s.timers) do
    timer:stop()
    timer:close()
  end
  pcall(vim.api.nvim_del_augroup_by_id, s.group)
  if s.doc then
    free_images(s.doc.base, s.chunks or 0)
  end
  if s.job then
    send(s, "quit\n")
    local job = s.job
    s.job = nil
    vim.defer_fn(function()
      pcall(vim.fn.jobstop, job)
    end, 1000)
  end
  if valid_win(s.pwin) and #vim.api.nvim_list_wins() > 1 then
    pcall(vim.api.nvim_win_close, s.pwin, true)
  end
  if vim.api.nvim_buf_is_valid(s.pbuf) then
    pcall(vim.api.nvim_buf_delete, s.pbuf, { force = true })
  end
end

--- Open a window for the preview buffer next to the current one.
local function open_window(pbuf)
  local cfg = M.config
  if cfg.position == "tab" then
    vim.cmd("tab split")
  else
    local split = ({
      right = "rightbelow vsplit",
      left = "leftabove vsplit",
      bottom = "rightbelow split",
      top = "leftabove split",
    })[cfg.position] or "rightbelow vsplit"
    local where, cmd = split:match("(%S+) (%S+)")
    vim.cmd(("%s %s%s"):format(where, cfg.size and tostring(cfg.size) or "", cmd))
  end
  local win = vim.api.nvim_get_current_win()
  vim.api.nvim_win_set_buf(win, pbuf)
  local wo = vim.wo[win]
  wo.number, wo.relativenumber, wo.signcolumn, wo.foldcolumn = false, false, "no", "0"
  wo.list, wo.wrap, wo.spell, wo.cursorline, wo.cursorcolumn = false, false, false, false, false
  wo.colorcolumn, wo.conceallevel, wo.scrolloff, wo.sidescrolloff = "", 0, 0, 0
  wo.winfixwidth = cfg.position == "right" or cfg.position == "left"
  wo.fillchars = "eob: "
  if vim.fn.exists("+statuscolumn") == 1 then
    wo.statuscolumn = ""
  end
  return win
end

--- Preview `buf` (default: the current buffer).
function M.open(buf)
  buf = (buf == nil or buf == 0) and vim.api.nvim_get_current_buf() or buf
  if M.is_open(buf) then
    return
  end
  local cfg = M.config
  if vim.fn.executable(cfg.cmd) == 0 then
    notify(("`%s` not found; install termd or set `cmd`"):format(cfg.cmd), vim.log.levels.ERROR)
    return
  end
  if not vim.o.termguicolors then
    notify("the preview needs 'termguicolors'", vim.log.levels.WARN)
  end

  local swin = vim.fn.bufwinid(buf) ~= -1 and vim.fn.bufwinid(buf) or vim.api.nvim_get_current_win()
  local pbuf = vim.api.nvim_create_buf(false, true)
  vim.bo[pbuf].bufhidden = "wipe"
  vim.bo[pbuf].filetype = "termd"
  pcall(vim.api.nvim_buf_set_name, pbuf, "termd://" .. vim.fn.fnamemodify(document_path(buf), ":t"))
  local pwin = vim.api.nvim_win_call(swin, function()
    return open_window(pbuf)
  end)

  local s = {
    buf = buf,
    swin = swin,
    pbuf = pbuf,
    pwin = pwin,
    tag = 0,
    waiting = {},
    timers = {},
    group = vim.api.nvim_create_augroup("termd_" .. buf, { clear = true }),
  }
  sessions[buf] = s

  local argv = { cfg.cmd, "--serve" }
  vim.list_extend(argv, cfg.args)
  table.insert(argv, document_path(buf))
  local partial, errors = "", {}
  local ok, job = pcall(vim.fn.jobstart, argv, {
    on_stdout = function(_, data)
      -- `data` is the output split at newlines; its first item continues
      -- the last line, its last item is a line not finished yet.
      data[1] = partial .. data[1]
      partial = table.remove(data)
      for _, line in ipairs(data) do
        if sessions[buf] == s then
          handle(s, line)
        end
      end
    end,
    on_stderr = function(_, data)
      for _, line in ipairs(data) do
        if line ~= "" then
          table.insert(errors, line)
        end
      end
    end,
    on_exit = function(_, code)
      if sessions[buf] == s then
        if code ~= 0 then
          notify(("termd exited with %d: %s"):format(code, table.concat(errors, "\n")), vim.log.levels.ERROR)
        end
        s.job = nil
        close_session(s)
      end
    end,
  })
  if not ok or job <= 0 then
    notify("could not start " .. cfg.cmd, vim.log.levels.ERROR)
    close_session(s)
    return
  end
  s.job = job

  send_size(s)
  send_colors(s)
  send_text(s)

  local au = function(events, opts)
    opts.group = s.group
    vim.api.nvim_create_autocmd(events, opts)
  end
  local update = function()
    send_text(s)
  end
  if cfg.live then
    au({ "TextChanged", "TextChangedI", "TextChangedP" }, { buffer = buf, callback = debounced(s, cfg.debounce, update) })
  else
    au("BufWritePost", { buffer = buf, callback = update })
  end
  if cfg.follow then
    au({ "CursorMoved", "CursorMovedI" }, {
      buffer = buf,
      callback = debounced(s, math.min(cfg.debounce, 20), function()
        follow(s)
      end),
    })
  end
  au("WinScrolled", {
    callback = function()
      send_view(s)
      if cfg.follow and vim.api.nvim_get_current_win() == source_win(s) then
        follow(s)
      end
    end,
  })
  au({ "WinResized", "VimResized" }, {
    callback = debounced(s, 50, function()
      send_size(s)
    end),
  })
  au("ColorScheme", {
    callback = function()
      s.filled = nil -- the colors that name the images were cleared too
      if s.doc then
        fill(s, s.doc.rows, s.doc.chunk, s.doc.base)
      end
      send_colors(s)
    end,
  })
  au({ "BufWipeout", "BufDelete" }, {
    buffer = buf,
    callback = function()
      close_session(s)
    end,
  })
  -- The preview buffer or window went: finish once Neovim is done with them.
  au("BufWipeout", {
    buffer = pbuf,
    callback = function()
      vim.schedule(function()
        close_session(s)
      end)
    end,
  })
  au("WinClosed", {
    pattern = tostring(pwin),
    callback = function()
      vim.schedule(function()
        close_session(s)
      end)
    end,
  })

  local map = function(lhs, fn, desc)
    vim.keymap.set("n", lhs, fn, { buffer = pbuf, nowait = true, desc = desc })
  end
  map("q", function()
    close_session(s)
  end, "Close the preview")
  for _, lhs in ipairs({ "<CR>", "e", "<2-LeftMouse>" }) do
    map(lhs, function()
      M.jump(buf)
    end, "Go to this place in the source")
  end

  if not cfg.focus and valid_win(swin) then
    vim.api.nvim_set_current_win(swin)
  end
end

--- Move the cursor in the source window to the line shown at the preview's
--- cursor, at the same height in the window.
function M.jump(buf)
  buf = (buf == nil or buf == 0) and vim.api.nvim_get_current_buf() or buf
  local s = sessions[buf]
  if not s or not valid_win(s.pwin) then
    return
  end
  local row = vim.api.nvim_win_get_cursor(s.pwin)[1]
  local height = 1
  vim.api.nvim_win_call(s.pwin, function()
    height = vim.fn.winline()
  end)
  ask(s, "which", row - 1, function(line)
    local win = source_win(s)
    if not (line and line > 0 and win) then
      return
    end
    line = math.min(line, vim.api.nvim_buf_line_count(buf))
    vim.api.nvim_set_current_win(win)
    vim.api.nvim_win_set_cursor(win, { line, 0 })
    vim.fn.winrestview({ topline = math.max(1, line - height + 1) })
  end)
end

--- The session previewing `buf`, or shown in it.
local function session_of(buf)
  buf = (buf == nil or buf == 0) and vim.api.nvim_get_current_buf() or buf
  for _, s in pairs(sessions) do
    if s.buf == buf or s.pbuf == buf then
      return s
    end
  end
end

--- Close the preview of `buf` (default: the current buffer, or the buffer
--- previewed in the current window).
function M.close(buf)
  local s = session_of(buf)
  if s then
    close_session(s)
  end
end

function M.is_open(buf)
  return session_of(buf) ~= nil
end

function M.toggle(buf)
  if M.is_open(buf) then
    M.close(buf)
  else
    M.open(buf)
  end
end

function M.setup(opts)
  M.config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), opts or {})
end

M._cell_size = cell_size
M._sessions = sessions

vim.api.nvim_create_autocmd("VimLeavePre", {
  group = vim.api.nvim_create_augroup("termd", { clear = true }),
  callback = function()
    for _, s in pairs(sessions) do
      close_session(s)
    end
  end,
})

return M
