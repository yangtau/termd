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
-- The page is shown in one of two ways: in a window of its own next to the
-- source, which it follows (a "split" session, `open`), or laid over the
-- source's window, following the cursor there (a "reader" session, `read`).
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
  -- Scroll the preview along with the source window.
  follow = true,
  -- Milliseconds the preview takes to catch up when it has more than a row
  -- to go; 0 makes it jump.
  smooth = 120,
  -- Milliseconds to wait after the last change before updating the preview.
  debounce = 100,
  -- Size of a terminal cell in pixels, { width, height }, when the
  -- terminal does not report it.
  cell_size = nil,
  -- Open Markdown files reading: the page over the source, following its
  -- cursor, until `q`, <CR> or editing (`i`, `dd`, ...).
  reader = true,
  -- The key that reads a Markdown buffer from Normal mode (so <Esc> twice
  -- from Insert mode). false for none; `:TermdRead` reads any time.
  read_key = "<Esc>",
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
--- first two cells name their row and column; the terminal counts on from
--- there. (Two: the first may be covered by the reader's mark.)
local function placeholder_row(row, cols)
  local mark = vim.fn.nr2char(DIACRITICS[row + 1])
  local text = PLACEHOLDER .. mark .. vim.fn.nr2char(DIACRITICS[1])
  if cols > 1 then
    text = text .. PLACEHOLDER .. mark .. vim.fn.nr2char(DIACRITICS[2])
  end
  return text .. PLACEHOLDER:rep(cols - 2)
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

local function clamp(value, low, high)
  return math.max(low, math.min(high, value))
end

--- The window showing the source buffer, preferring the one it was opened from.
local function source_win(s)
  if valid_win(s.swin) and vim.api.nvim_win_get_buf(s.swin) == s.buf then
    return s.swin
  end
  local win = vim.fn.bufwinid(s.buf)
  return win ~= -1 and win or nil
end

--- The window that shows the page right now, if any.
local function page_win(s)
  local win = s.pwin
  if valid_win(win) and vim.api.nvim_win_get_buf(win) == s.pbuf then
    return win
  end
end

local function send(s, data)
  if s.job then
    pcall(vim.fn.chansend, s.job, data)
  end
end

--- Send termd the buffer's text, unless it has what the buffer holds now.
local function send_text(s)
  local tick = vim.api.nvim_buf_get_changedtick(s.buf)
  if tick == s.tick or not s.job then
    return
  end
  s.tick = tick
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

--- Tell termd the width of the page's window and the cell size, if they
--- changed.
local function send_size(s)
  local win = s.pwin
  if not (valid_win(win) and s.job) then
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
  local info = vim.fn.getwininfo(win)[1]
  local cols = math.max(1, info.width - info.textoff)
  local size = ("size %d %d %d\n"):format(cols, w, h)
  if size ~= s.size then
    s.size, s.cols = size, cols
    send(s, size)
  end
end

--- Tell termd which rows (0-based) to have ready.
local function show(s, top, bottom)
  local view = ("show %d %d\n"):format(math.max(0, top), math.max(0, bottom))
  if view ~= s.view then
    s.view = view
    send(s, view)
  end
end

--- Tell termd which rows the window shows, and those it is scrolling to.
local function send_view(s)
  local win = page_win(s)
  if not (s.doc and win) then
    return
  end
  local top, bottom = vim.fn.line("w0", win) - 1, vim.fn.line("w$", win) - 1
  if s.target then
    top = math.min(top, s.target)
    bottom = math.max(bottom, s.target + vim.api.nvim_win_get_height(win) - 1)
  end
  show(s, top, bottom)
end

--- Ask termd something; `fn` gets the numbers of the answer.
local function ask(s, what, args, fn)
  s.tag = s.tag + 1
  s.waiting[tostring(s.tag)] = fn
  send(s, ("%s %d %s\n"):format(what, s.tag, args))
end

-- Where the source is on the page ------------------------------------------
--
-- With every page termd sends a map: the row at which each source line
-- starts (with a fraction; a line that shows nothing has its share of the
-- gap it is in), and the row at which the page ends. Positions between two
-- entries are interpolated, so both directions are continuous and never go
-- back.

--- The row (0-based, with a fraction) at which source line `line` (1-based,
--- with a fraction for a place inside the line) is on the page.
local function row_of(s, line)
  local map = s.map
  local lines = map and #map - 1 or 0
  if lines < 1 then
    return 0
  end
  line = clamp(line, 1, lines + 1)
  local i = math.min(math.floor(line), lines)
  return map[i] + (line - i) * (map[i + 1] - map[i])
end

--- The source line (with a fraction) that row `row` of the page shows.
local function line_of(s, row)
  local map = s.map
  local lines = map and #map - 1 or 0
  if lines < 1 then
    return 1
  end
  -- The last line that starts at or above the row.
  local low, high = 1, lines
  while low < high do
    local mid = math.ceil((low + high) / 2)
    if map[mid] <= row then
      low = mid
    else
      high = mid - 1
    end
  end
  local span = map[low + 1] - map[low]
  return low + (span > 0 and clamp((row - map[low]) / span, 0, 1) or 0)
end

M._row_of, M._line_of = row_of, line_of

--- What in the source the cell at `row`, `col` of the page shows: the line
--- (1-based) and the byte in it. Waits for termd's answer, so that keys
--- typed meanwhile find the cursor in place.
local function spot(s, row, col)
  local line, byte
  ask(s, "spot", ("%d %d"):format(row, col), function(l, b)
    line, byte = l or 0, b or 0
  end)
  vim.wait(300, function()
    return line ~= nil
  end, 2)
  if line and line > 0 then
    return line, byte
  end
  return math.floor(line_of(s, row + 0.5)), 0
end

-- Scrolling the page ----------------------------------------------------

--- What a window needs to show the page and nothing else.
local PAGE_OPTIONS = {
  number = false,
  relativenumber = false,
  signcolumn = "no",
  foldcolumn = "0",
  foldenable = false,
  statuscolumn = "",
  list = false,
  wrap = false,
  spell = false,
  cursorline = false,
  cursorcolumn = false,
  colorcolumn = "",
  conceallevel = 0,
  scrolloff = 0,
  sidescrolloff = 0,
  fillchars = "eob: ",
}

local function page_options(win)
  for name, value in pairs(PAGE_OPTIONS) do
    -- (Not every Neovim has all of them.)
    pcall(vim.api.nvim_set_option_value, name, value, { win = win, scope = "local" })
  end
end

local mark_ns = vim.api.nvim_create_namespace("termd_mark")

--- Mark row `row` of a reader's page as the one the cursor is on.
local function mark(s, row)
  if s.kind ~= "reader" or not vim.api.nvim_buf_is_valid(s.pbuf) then
    return
  end
  s.mark = vim.api.nvim_buf_set_extmark(s.pbuf, mark_ns, row, 0, {
    id = s.mark,
    virt_text = { { "▌", "TermdCursor" } },
    virt_text_pos = "overlay",
  })
end

--- Show the page from row `top` (0-based) in `win`, with the cursor on row
--- `row`, or as near to it as the window lets it be.
local function set_view(s, win, top, row, col)
  local rows = vim.api.nvim_buf_line_count(s.pbuf)
  local height = vim.api.nvim_win_get_height(win)
  top = clamp(top, 0, math.max(0, rows - 1))
  local lnum = clamp(row + 1, top + 1, math.min(rows, top + height))
  -- Remembered to tell this from the user scrolling the window.
  s.set = { top = top, lnum = lnum }
  vim.api.nvim_win_call(win, function()
    vim.fn.winrestview({ topline = top + 1, lnum = lnum, col = col or 0, leftcol = 0 })
  end)
  mark(s, lnum - 1)
  send_view(s)
end

local FRAME = 16 -- milliseconds

local function now()
  return uv.hrtime() / 1e6
end

local function stop_scroll(s)
  if s.anim then
    s.anim = nil
    s.scroller:stop()
  end
  s.target = nil
end

--- One frame of the preview scrolling to `s.target`: a share of the way
--- left, so it starts fast and slows down, but never less than a row per
--- frame, so it arrives. A new target only changes where it is heading.
local function scroll_step(s)
  local anim, pwin = s.anim, s.pwin
  if not (anim and s.target and valid_win(pwin)) then
    return stop_scroll(s)
  end
  local at = vim.fn.line("w0", pwin) - 1
  if s.set and at ~= s.set.top then
    return stop_scroll(s) -- the user scrolled it meanwhile, and knows best
  end
  local time = now()
  local passed = clamp(time - anim.time, 1, 4 * FRAME)
  anim.time = time
  local left = s.target - anim.pos
  local move = left * (1 - math.exp(-3 * passed / M.config.smooth))
  local least = passed / FRAME
  if math.abs(move) < least then
    move = (left < 0 and -1 or 1) * math.min(math.abs(left), least)
  end
  anim.pos = anim.pos + move
  if math.abs(s.target - anim.pos) < 0.5 then
    local target = s.target
    stop_scroll(s)
    set_view(s, pwin, target, s.cursor_row)
  else
    local top = math.floor(anim.pos + 0.5)
    if top ~= at then
      set_view(s, pwin, top, s.cursor_row)
    end
  end
end

--- Scroll the preview so that row `top` is its first, with its cursor on
--- row `row`: at once if that is a row away or `jump` is set, else over a
--- few frames.
local function scroll_to(s, top, row, jump)
  local pwin = s.pwin
  local rows = vim.api.nvim_buf_line_count(s.pbuf)
  local height = vim.api.nvim_win_get_height(pwin)
  top = clamp(math.floor(top + 0.5), 0, math.max(0, rows - height))
  s.cursor_row = clamp(math.floor(row), 0, rows - 1)
  local at = vim.fn.line("w0", pwin) - 1
  local smooth = tonumber(M.config.smooth) or 0
  if jump or smooth <= 0 or math.abs(top - at) <= 1 then
    stop_scroll(s)
    set_view(s, pwin, top, s.cursor_row)
    return
  end
  s.target = top
  if not s.anim then
    s.anim = { pos = at, time = now() - FRAME }
    s.scroller:start(
      FRAME,
      FRAME,
      vim.schedule_wrap(function()
        if sessions[s.buf] == s then
          scroll_step(s)
        end
      end)
    )
  end
  -- A long way is not travelled: only the last screenful of it.
  if math.abs(top - s.anim.pos) > 2 * height then
    s.anim.pos = top + (s.anim.pos < top and -height or height)
  end
  scroll_step(s)
end

--- What the source window shows and where its cursor is.
local function source_view(win)
  local v = {}
  -- (Fields are assigned rather than returned: nvim_win_call can return a
  -- float as a table.)
  vim.api.nvim_win_call(win, function()
    v.top, v.bottom, v.last = vim.fn.line("w0"), vim.fn.line("w$"), vim.fn.line("$")
    v.lnum, v.col, v.len = vim.fn.line("."), vim.fn.col("."), vim.fn.col("$")
    v.winline, v.height = vim.fn.winline(), vim.fn.winheight(0)
  end)
  -- Rows of the window that have text: not all of them at the end of the
  -- buffer.
  v.used = v.height
  if v.bottom >= v.last then
    local used = v.bottom - v.top + 1
    if vim.api.nvim_win_text_height then
      local ok, size = pcall(vim.api.nvim_win_text_height, win, { start_row = v.top - 1, end_row = v.bottom - 1 })
      used = ok and size.all or used
    end
    v.used = clamp(used, v.winline, v.height)
  end
  -- How far into its line the cursor is, which for a long paragraph is how
  -- far down in it.
  v.along = v.len > 1 and clamp((v.col - 1) / (v.len - 1), 0, 1) or 0
  return v
end

--- Scroll the preview along with the source window.
---
--- The source window shows some lines and the page has a stretch that
--- shows the same; as a rule that stretch is longer than the preview is
--- high. The preview slides over it as the cursor goes down the source
--- window: with the cursor on the first row, both windows start with the
--- same text; on the last row, both end with it. So the cursor's line is at
--- about the same height in both, and each step of the cursor moves the
--- preview by the same small amount, whatever the line it lands on looks
--- like on the page. (Putting the cursor's line at exactly the same height
--- makes the preview jump back and forth: an empty line takes less room on
--- the page than in the source, a heading more.)
local function follow(s, jump)
  local win = source_win(s)
  local pwin = s.pwin
  if not (s.doc and s.map and win and valid_win(pwin)) or vim.api.nvim_get_current_win() == pwin then
    return
  end
  local v = source_view(win)
  local height = vim.api.nvim_win_get_height(pwin)
  local first, last = row_of(s, v.top), row_of(s, v.bottom + 1)
  local down = v.used > 1 and clamp((v.winline - 1) / (v.used - 1), 0, 1) or 0
  local top = first + down * math.max(0, last - first - height)
  -- Whatever the page looks like there, the cursor's place has to show.
  local row = row_of(s, v.lnum + v.along)
  local margin = math.min(2, math.floor((height - 1) / 2))
  top = math.max(row - (height - 1 - margin), math.min(top, row - margin))
  scroll_to(s, top, row, jump)
end

--- Fill the preview buffer with placeholders for `rows` rows, in chunks of
--- `chunk` rows that are images `base`, `base + 1`, ... Only the end of the
--- buffer changes when the page got longer or shorter.
local function fill(s, rows, chunk, base)
  local cols = s.cols
  local old = s.filled
  local same = old and old.chunk == chunk and old.base == base and old.cols == cols
  if same and old.rows == rows then
    return
  end
  local kept = same and math.min(old.rows, rows) or 0
  local templates = {}
  for r = 0, chunk - 1 do
    templates[r] = placeholder_row(r, cols)
  end
  local lines = {}
  for r = kept, rows - 1 do
    lines[#lines + 1] = templates[r % chunk]
  end
  vim.bo[s.pbuf].modifiable = true
  vim.api.nvim_buf_set_lines(s.pbuf, kept, -1, false, lines)
  vim.bo[s.pbuf].modifiable = false
  vim.bo[s.pbuf].modified = false
  if not same then
    vim.api.nvim_buf_clear_namespace(s.pbuf, ns, 0, -1)
  end
  local chunks = math.ceil(rows / chunk)
  for c = math.floor(kept / chunk), chunks - 1 do
    local last = math.min(rows, (c + 1) * chunk)
    vim.api.nvim_buf_set_extmark(s.pbuf, ns, c * chunk, 0, {
      id = c + 1,
      end_row = last - 1,
      end_col = #templates[(last - 1) % chunk],
      hl_group = image_group(base + c),
    })
  end
  for c = chunks, same and old.chunks - 1 or -1 do
    vim.api.nvim_buf_del_extmark(s.pbuf, ns, c + 1)
  end
  s.filled = { rows = rows, chunk = chunk, base = base, cols = cols, chunks = chunks }
  s.chunks = math.max(s.chunks or 0, chunks)
end

-- The reader --------------------------------------------------------------
--
-- A reader session lays the page over the source's window, in a floating
-- window of the same size that cannot be entered (`s.pwin` while reading).
-- The source stays the window's buffer and has the focus: its cursor moves
-- and the page follows it as a preview follows its source window, a mark
-- at the left edge of the page showing where the cursor is. So whatever
-- looks at the current buffer (`:w`, a file tree, the status line, `/`)
-- finds the file. Reading ends when the text is changed or Normal mode
-- left for anything but the command line; `q` and <CR> end it too. termd
-- keeps running in between, so that the page comes back at once.

--- The first line to show in `win` for `line` to have `above` screen rows
--- above it.
local function topline_for(win, line, above)
  local height = function(lnum)
    if not vim.api.nvim_win_text_height then
      return 1
    end
    local ok, size = pcall(vim.api.nvim_win_text_height, win, { start_row = lnum - 1, end_row = lnum - 1 })
    return ok and size.all or 1
  end
  local top = line
  while top > 1 and above > 0 do
    above = above - height(top - 1)
    if above < 0 then
      break
    end
    top = top - 1
  end
  return top
end

-- The terminal's cursor is in the source window, under the page: hidden
-- there while it is read (with a highlight that blends it away entirely).
local saved_guicursor

local function hide_cursor(hide)
  if hide and not saved_guicursor then
    saved_guicursor = vim.o.guicursor
    vim.api.nvim_set_hl(0, "TermdHiddenCursor", { blend = 100, nocombine = true })
    vim.o.guicursor = (saved_guicursor ~= "" and saved_guicursor .. "," or "") .. "n:TermdHiddenCursor"
  elseif not hide and saved_guicursor then
    vim.o.guicursor = saved_guicursor
    saved_guicursor = nil
  end
end

local function reading_in(win)
  for _, s in pairs(sessions) do
    if s.kind == "reader" and s.swin == win and valid_win(s.pwin) then
      return s
    end
  end
end

local function update_cursor()
  hide_cursor(reading_in(vim.api.nvim_get_current_win()) ~= nil)
end

--- Where the page goes over window `win`, which has the source: all of
--- it but the winbar.
local function overlay_config(win)
  local info = vim.fn.getwininfo(win)[1]
  return {
    relative = "win",
    win = win,
    row = 0,
    col = 0,
    width = math.max(1, info.width),
    height = math.max(1, info.height),
  }
end

--- Keys of the source buffer while reading, and what they were before.
local function reading_keys(s)
  local keys = { "q", "<CR>" }
  if M.config.read_key ~= "<Esc>" then
    table.insert(keys, "<Esc>")
  end
  return keys
end

local function map_reading_keys(s)
  s.saved_maps = {}
  vim.api.nvim_buf_call(s.buf, function()
    for _, lhs in ipairs(reading_keys(s)) do
      local old = vim.fn.maparg(lhs, "n", false, true)
      if old.buffer == 1 then
        s.saved_maps[#s.saved_maps + 1] = old
      end
      vim.keymap.set("n", lhs, function()
        M.unread(s.buf)
      end, { buffer = s.buf, nowait = true, desc = "Stop reading" })
    end
  end)
end

local function unmap_reading_keys(s)
  if not (s.saved_maps and vim.api.nvim_buf_is_valid(s.buf)) then
    return
  end
  vim.api.nvim_buf_call(s.buf, function()
    for _, lhs in ipairs(reading_keys(s)) do
      pcall(vim.keymap.del, "n", lhs, { buffer = s.buf })
    end
    for _, old in ipairs(s.saved_maps) do
      pcall(vim.fn.mapset, "n", false, old)
    end
  end)
  s.saved_maps = nil
end

--- Lay the page over window `win`.
local function show_page(s, win)
  if valid_win(s.pwin) then
    return
  end
  s.swin = win
  local config = overlay_config(win)
  config.focusable = false
  config.style = "minimal"
  config.zindex = 1 -- under every other float: completion, hover, messages
  config.border = "none" -- whatever 'winborder' says
  config.noautocmd = true
  local ok, pwin = pcall(vim.api.nvim_open_win, s.pbuf, false, config)
  if not ok then
    notify(tostring(pwin), vim.log.levels.ERROR)
    return
  end
  s.pwin = pwin
  page_options(pwin)
  vim.wo[pwin].winhighlight = "NormalFloat:Normal"
  map_reading_keys(s)
  update_cursor()
  send_size(s)
  send_text(s)
  s.view = nil
  follow(s, true)
end

--- Take the page away, and the source shows.
local function hide_page(s)
  if s.pwin == nil then
    return
  end
  stop_scroll(s)
  if valid_win(s.pwin) then
    pcall(vim.api.nvim_win_close, s.pwin, true)
  end
  s.pwin = nil
  unmap_reading_keys(s)
  update_cursor()
end

--- See whether the page still has the source under it.
local function check_page(s)
  if s.pwin == nil then
    return
  end
  if not valid_win(s.pwin) or not valid_win(s.swin) or vim.api.nvim_win_get_buf(s.swin) ~= s.buf then
    hide_page(s)
  else
    local config = overlay_config(s.swin)
    local now_config = vim.api.nvim_win_get_config(s.pwin)
    if now_config.width ~= config.width or now_config.height ~= config.height then
      pcall(vim.api.nvim_win_set_config, s.pwin, config)
    end
  end
  update_cursor()
end

local close_session

-- termd -------------------------------------------------------------------

--- A line from termd.
local function handle(s, line)
  local kind, rest = line:match("^(%S+) ?(.*)$")
  if kind == "G" then
    M._write(rest)
  elseif kind == "map" then
    local map, cell_h = {}, nil
    for number in rest:gmatch("%d+") do
      if cell_h then
        map[#map + 1] = tonumber(number) / cell_h
      else
        cell_h = math.max(1, tonumber(number))
      end
    end
    s.map = map
  elseif kind == "doc" then
    local rows, chunk, base = rest:match("^(%d+) (%d+) (%d+)")
    rows, chunk, base = tonumber(rows), tonumber(chunk), tonumber(base)
    if not (rows and vim.api.nvim_buf_is_valid(s.pbuf)) then
      return
    end
    if not s.map and not s.warned_old then
      s.warned_old = true
      notify(("`%s` is older than this plugin: update termd"):format(M.config.cmd), vim.log.levels.WARN)
    end
    s.doc = { rows = rows, chunk = chunk, base = base }
    fill(s, rows, chunk, base)
    s.view = nil
    if page_win(s) and (s.kind == "reader" or M.config.follow) then
      follow(s, true)
    end
    send_view(s)
  elseif kind == "at" then
    local tag, numbers = rest:match("^(%S+) ?(.*)$")
    local fn = tag and s.waiting[tag]
    if fn then
      s.waiting[tag] = nil
      local values = {}
      for number in numbers:gmatch("%S+") do
        values[#values + 1] = tonumber(number)
      end
      fn(unpack(values))
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

--- End a session. `leaving`: Neovim is exiting, so its windows can stay as
--- they are.
close_session = function(s, leaving)
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
  if s.kind == "reader" then
    hide_page(s)
  elseif valid_win(s.pwin) and #vim.api.nvim_list_wins() > 1 then
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
  page_options(win)
  vim.wo[win].winfixwidth = cfg.position == "right" or cfg.position == "left"
  return win
end

--- A session for `buf` with its page buffer, and nothing running yet.
local function new_session(buf, kind)
  local cfg = M.config
  if vim.fn.executable(cfg.cmd) == 0 then
    notify(("`%s` not found; install termd or set `cmd`"):format(cfg.cmd), vim.log.levels.ERROR)
    return
  end
  if not vim.o.termguicolors then
    notify("the preview needs 'termguicolors'", vim.log.levels.WARN)
  end
  local pbuf = vim.api.nvim_create_buf(false, true)
  vim.bo[pbuf].filetype = "termd"
  -- No name: a name is taken for a file by what looks for the current one
  -- (a file tree revealing it would ask to change to its directory).
  local s = {
    kind = kind,
    buf = buf,
    pbuf = pbuf,
    tag = 0,
    waiting = {},
    timers = {},
    scroller = uv.new_timer(),
    group = vim.api.nvim_create_augroup("termd_" .. buf, { clear = true }),
  }
  table.insert(s.timers, s.scroller)
  sessions[buf] = s
  return s
end

--- Start termd for a session and wire up what both kinds of session need.
--- Returns whether termd runs.
local function start(s)
  local cfg, buf, pbuf = M.config, s.buf, s.pbuf
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
          -- (Not to be opened again right away by `reader`.)
          pcall(vim.api.nvim_buf_set_var, buf, "termd_reader", false)
        end
        s.job = nil
        close_session(s)
      end
    end,
  })
  if not ok or job <= 0 then
    notify("could not start " .. cfg.cmd, vim.log.levels.ERROR)
    close_session(s)
    return false
  end
  s.job = job

  send_size(s)
  send_colors(s)
  send_text(s)

  local au = function(events, opts)
    opts.group = s.group
    vim.api.nvim_create_autocmd(events, opts)
  end
  s.au = au
  local update = function()
    send_text(s)
  end
  if cfg.live then
    au({ "TextChanged", "TextChangedI", "TextChangedP" }, { buffer = buf, callback = debounced(s, cfg.debounce, update) })
  else
    au("BufWritePost", { buffer = buf, callback = update })
  end
  au("WinScrolled", {
    callback = function()
      -- Which windows scrolled (or changed size), by their ids.
      local scrolled = vim.v.event
      local pwin = page_win(s)
      if pwin and scrolled[tostring(pwin)] then
        if s.set and vim.fn.line("w0", pwin) - 1 ~= s.set.top then
          stop_scroll(s) -- scrolled by the user: it stays where they put it
        end
        send_view(s)
      end
      if (s.kind == "split" and cfg.follow) or (s.kind == "reader" and pwin) then
        local win = source_win(s)
        if win and scrolled[tostring(win)] then
          follow(s)
        end
      end
    end,
  })
  au({ "WinResized", "VimResized" }, {
    callback = debounced(s, 50, function()
      if s.kind == "reader" then
        check_page(s)
      end
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
  -- The page's buffer went: finish once Neovim is done with it.
  au("BufWipeout", {
    buffer = pbuf,
    callback = function()
      vim.schedule(function()
        close_session(s)
      end)
    end,
  })
  return true
end

local function current_buf(buf)
  return (buf == nil or buf == 0) and vim.api.nvim_get_current_buf() or buf
end

--- The session previewing `buf`, or shown in it.
local function session_of(buf)
  buf = current_buf(buf)
  for _, s in pairs(sessions) do
    if s.buf == buf or s.pbuf == buf then
      return s
    end
  end
end

--- Preview `buf` (default: the current buffer) in a window next to it.
function M.open(buf)
  local s = session_of(buf)
  if s and s.kind == "split" then
    return
  elseif s then
    buf = s.buf
    M.close(buf)
  end
  buf = current_buf(buf)
  s = new_session(buf, "split")
  if not s then
    return
  end
  local cfg = M.config
  local pbuf = s.pbuf
  vim.bo[pbuf].bufhidden = "wipe"
  s.swin = vim.fn.bufwinid(buf) ~= -1 and vim.fn.bufwinid(buf) or vim.api.nvim_get_current_win()
  s.pwin = vim.api.nvim_win_call(s.swin, function()
    return open_window(pbuf)
  end)
  if not start(s) then
    return
  end

  if cfg.follow then
    s.au({ "CursorMoved", "CursorMovedI" }, {
      buffer = buf,
      callback = function()
        follow(s)
      end,
    })
  end
  s.au("WinClosed", {
    pattern = tostring(s.pwin),
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

  if not cfg.focus and valid_win(s.swin) then
    vim.api.nvim_set_current_win(s.swin)
  end
end

--- Read `buf` (default: the current buffer): lay the page over its window,
--- following the cursor, until the text changes, a mode other than Normal
--- and the command line is entered, or `unread`.
function M.read(buf)
  local s = session_of(buf)
  buf = s and s.buf or current_buf(buf)
  local win = vim.api.nvim_get_current_win()
  if vim.api.nvim_win_get_buf(win) ~= buf then
    win = vim.fn.bufwinid(buf)
  end
  if win == -1 or vim.api.nvim_win_get_config(win).relative ~= "" then
    return
  end
  vim.b[buf].termd_reader = nil
  if s and s.kind == "reader" then
    -- termd still runs from reading before.
    if valid_win(s.pwin) and s.swin ~= win then
      hide_page(s)
    end
    show_page(s, win)
    return
  elseif s then
    close_session(s)
  end
  s = new_session(buf, "reader")
  if not s then
    return
  end
  vim.bo[s.pbuf].bufhidden = "hide"
  show_page(s, win)
  if not s.pwin or not start(s) then
    close_session(s)
    return
  end

  local au = s.au
  au("CursorMoved", {
    buffer = buf,
    callback = function()
      if s.pwin then
        follow(s)
      end
    end,
  })
  -- Changing the text is editing: the source shows. (Not when writing
  -- formats it.)
  au("BufWritePost", {
    buffer = buf,
    callback = function()
      s.written = vim.api.nvim_buf_get_changedtick(buf)
    end,
  })
  au("TextChanged", {
    buffer = buf,
    callback = function()
      if s.pwin and vim.api.nvim_buf_get_changedtick(buf) ~= s.written then
        hide_page(s)
      end
    end,
  })
  -- So is Insert, Visual or Replace mode, or an operator; the command line
  -- (`:w`, a search) is not.
  au("ModeChanged", {
    pattern = "n:*",
    callback = function()
      local mode = vim.v.event.new_mode or ""
      if s.pwin and not mode:match("^c") and vim.api.nvim_get_current_win() == s.swin then
        hide_page(s)
      end
    end,
  })
  au("WinResized", {
    callback = function()
      check_page(s)
    end,
  })
  au({ "BufEnter", "WinEnter", "WinClosed" }, {
    callback = function()
      vim.schedule(function()
        if sessions[buf] == s then
          check_page(s)
        end
      end)
    end,
  })
end

--- Stop reading `buf` (default: the current buffer): the source shows, its
--- cursor where the page had it. termd keeps running, to read again soon.
function M.unread(buf)
  local s = session_of(buf)
  if s and s.kind == "reader" then
    hide_page(s)
  end
end

--- From the preview, move the cursor in the source window to what the
--- preview's cursor is on, at the same height in the window. (A reader's
--- cursor is in the source already.)
function M.jump(buf)
  local s = session_of(buf)
  if not s or s.kind == "reader" or not valid_win(s.pwin) then
    return
  end
  local cursor = vim.api.nvim_win_get_cursor(s.pwin)
  local cell, winline = 0, 1
  vim.api.nvim_win_call(s.pwin, function()
    cell, winline = vim.fn.virtcol(".") - 1, vim.fn.winline()
  end)
  local line, byte = spot(s, cursor[1] - 1, cell)
  local win = source_win(s)
  if sessions[s.buf] ~= s or not win then
    return
  end
  line = clamp(line, 1, vim.api.nvim_buf_line_count(s.buf))
  vim.api.nvim_set_current_win(win)
  vim.fn.winrestview({ topline = topline_for(win, line, winline - 1), lnum = line, col = byte })
end

--- Close the preview of `buf` (default: the current buffer, or the buffer
--- previewed in the current window). A reader stops reading, and termd.
function M.close(buf)
  local s = session_of(buf)
  if s then
    if s.kind == "reader" and vim.api.nvim_buf_is_valid(s.buf) then
      vim.b[s.buf].termd_reader = false
    end
    close_session(s)
  end
end

--- Whether the page is read over `win` (default: the current window).
function M.reading(win)
  win = (win == nil or win == 0) and vim.api.nvim_get_current_win() or win
  return reading_in(win) ~= nil
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

--- With `reader` set: read `buf` if it is a Markdown file just opened in the
--- current window. (Once: a file left for its source stays that way.)
local function auto_read(buf)
  if vim.v.vim_did_enter == 0 then
    -- No screen yet to know the size of.
    vim.api.nvim_create_autocmd("VimEnter", {
      once = true,
      callback = function()
        auto_read(buf)
      end,
    })
    return
  end
  -- (Later: this is called while the buffer is being set up.)
  vim.schedule(function()
    local win = vim.api.nvim_get_current_win()
    if
      M.config.reader
      and vim.api.nvim_buf_is_valid(buf)
      and vim.api.nvim_win_get_buf(win) == buf
      and vim.bo[buf].filetype == "markdown"
      and vim.bo[buf].buftype == ""
      and vim.b[buf].termd_reader ~= false
      and not vim.b[buf].termd_opened
      and not session_of(buf)
      and vim.api.nvim_win_get_config(win).relative == ""
      and not vim.wo[win].diff
      and vim.api.nvim_get_mode().mode == "n"
    then
      vim.b[buf].termd_opened = true
      M.read(buf)
    end
  end)
end

--- Map `read_key` in Normal mode in Markdown buffer `buf`, unless the
--- buffer has a mapping of its own for it.
local function map_read_key(buf)
  local key = M.config.read_key
  if not key or key == "" or not vim.api.nvim_buf_is_valid(buf) or vim.b[buf].termd_read_key then
    return
  end
  local own = vim.api.nvim_buf_call(buf, function()
    return vim.fn.maparg(key, "n", false, true)
  end)
  if own.buffer == 1 then
    return
  end
  vim.b[buf].termd_read_key = true
  local raw = vim.api.nvim_replace_termcodes(key, true, false, true)
  vim.keymap.set("n", key, function()
    local win = vim.api.nvim_get_current_win()
    local s = session_of(buf)
    if
      not (s and s.kind == "split")
      and vim.b[buf].termd_reader ~= false
      and vim.bo[buf].buftype == ""
      and vim.api.nvim_win_get_config(win).relative == ""
      and not vim.wo[win].diff
      and vim.fn.reg_executing() == ""
    then
      M.read(buf)
    else
      vim.api.nvim_feedkeys(raw, "n", false)
    end
  end, { buffer = buf, desc = "Read the page (termd)" })
end

local function define_highlights()
  vim.api.nvim_set_hl(0, "TermdCursor", { default = true, link = "Title" })
end

function M.setup(opts)
  M.config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), opts or {})
  local group = vim.api.nvim_create_augroup("termd_reader", { clear = true })
  if M.config.read_key then
    vim.api.nvim_create_autocmd("FileType", {
      group = group,
      pattern = "markdown",
      callback = function(ev)
        map_read_key(ev.buf)
      end,
    })
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].filetype == "markdown" then
        map_read_key(buf)
      end
    end
  end
  if M.config.reader then
    vim.api.nvim_create_autocmd({ "BufWinEnter", "FileType" }, {
      group = group,
      callback = function(ev)
        auto_read(ev.buf)
      end,
    })
    auto_read(vim.api.nvim_get_current_buf())
  end
end

M._cell_size = cell_size
M._sessions = sessions

define_highlights()
local group = vim.api.nvim_create_augroup("termd", { clear = true })
vim.api.nvim_create_autocmd("ColorScheme", { group = group, callback = define_highlights })
vim.api.nvim_create_autocmd("VimLeavePre", {
  group = group,
  callback = function()
    for _, s in pairs(sessions) do
      close_session(s, true)
    end
  end,
})

return M
