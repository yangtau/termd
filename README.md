# termd

在终端里以「渲染后的页面」预览 Markdown：把文档按像素排版成图像，通过
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) 显示，并支持像素级滚动。

和 `glow`、`mdcat` 这类字符渲染不同，termd 画的是真正的排版结果：比例字体、不同字号的标题、
圆角代码块、表格、内嵌图片。

页面没有自己的底色：没有画东西的地方是透明的，直接透出终端的背景，文字颜色也取自终端，
所以不管用什么配色方案都不会出现一块突兀的白底或黑底。

## 使用

```sh
cargo install --path .
termd README.md
```

也可以用 Nix。CI 会把 Linux x86_64 和 macOS arm64 的构建结果推送到
[tau.cachix.org](https://tau.cachix.org)，所以通常是直接下载，不需要本地编译：

```sh
nix run github:yangtau/termd -- README.md            # main 上的最新版本
nix run github:yangtau/termd/v0.2.0 -- README.md     # 指定版本（git 标签）
nix profile install github:yangtau/termd/v0.2.0
```

flake 里已经声明了缓存地址和公钥（`nixConfig`），第一次使用时 Nix 会询问是否信任，
也可以加 `--accept-flake-config`。如果你的用户不在 Nix 的 `trusted-users` 里，
需要把缓存加到系统的 `nix.conf`：

```
extra-substituters = https://tau.cachix.org
extra-trusted-public-keys = tau.cachix.org-1:tqAThmpvqthNsfMUwRlljsDKPW8147I/+yzRVZnxKsA=
```

在自己的 flake 里使用：

```nix
inputs.termd.url = "github:yangtau/termd/v0.2.0";   # 不写标签就是 main
environment.systemPackages = [ inputs.termd.packages.${pkgs.system}.default ];
# 或者 overlay：nixpkgs.overlays = [ inputs.termd.overlays.default ]; 之后用 pkgs.termd
```

需要支持 Kitty 图像协议的终端（kitty、Ghostty、WezTerm 等）。暂不支持在 tmux 内运行。

| 按键                                           | 动作                                                                                       |
| :--------------------------------------------- | :----------------------------------------------------------------------------------------- |
| `j` / `k`、`↓` / `↑`、鼠标滚轮                 | 上下滚动                                                                                   |
| `space` / `b`、`PgDn` / `PgUp`                 | 翻页                                                                                       |
| `d` / `u`                                      | 半页                                                                                       |
| `g` / `G`                                      | 顶部 / 底部                                                                                |
| `+` / `-` / `0`                                | 放大 / 缩小 / 还原                                                                         |
| `e`                                            | 用 `$VISUAL` / `$EDITOR` 打开文件，光标在视口顶部对应的源码行；退出编辑器后回到原来的位置  |
| `y` / `Y`                                      | 复制视口里的代码块（`Y` 连同围栏一起）；连按会依次轮到视口里的下一个代码块                 |
| `/`                                            | 搜索：边输入边高亮匹配并跳过去，`Enter` 确定，`Esc` 取消；输入了大写字母才区分大小写       |
| `n` / `N`                                      | 下一个 / 上一个匹配，到头会绕回；状态栏显示是第几个                                        |
| 有搜索结果时 `Esc`                             | 清除搜索高亮（`n` 会把上一次的搜索找回来）                                                 |
| 点击链接                                       | 用系统默认方式打开；`#标题` 形式的链接跳到文内对应标题；鼠标悬停时状态栏显示目标地址       |
| 点击代码块右上角的复制图标                     | 复制这个代码块                                                                             |
| 鼠标拖拽                                       | 选中文字，松开即复制为纯文本（列表带标记，表格单元格用制表符分隔）；拖到上下边缘会自动滚动 |
| 双击 / 三击                                    | 选中一个词 / 整段                                                                          |
| `Shift` + 点击                                 | 把选区延伸到点击处                                                                         |
| 有选区时 `y` / `Y`                             | 再次复制纯文本 / 复制选区对应的 Markdown 源码（跨行时取整行）                              |
| 有选区时 `Esc`                                 | 取消选区                                                                                   |
| `h` / `l`、`←` / `→`、`Shift` + 滚轮、横向滚轮 | 横向滚动放不下的宽表格（鼠标所在的那张，否则是视口里的第一张）                             |
| `r`                                            | 重新加载（文件变化时也会自动重载）                                                         |
| `q` / `Esc`                                    | 退出                                                                                       |

复制走的是终端的 OSC 52，所以通过 SSH 使用时也能复制到本机剪贴板。

常用选项：

- `--theme auto|dark|light`：默认 `auto`，沿用终端的前景 / 背景色，页面背景透明。
  强制指定的明暗和终端实际背景相反时，才会画一块不透明的页面底色。
  文字边缘和代码块之类的浅色底是按终端报告的背景色预先混合好的，
  所以终端设置了半透明或背景图时，这些地方会是不透明的纯色。
- `--font NAME`、`--mono-font NAME`：指定正文和代码字体。正文默认用衬线字体
  （macOS 上是 Charter + 宋体，Linux 上问 fontconfig），中文字体按 `LANG` 选简体 / 繁体 / 日文 / 韩文字形。
- `--sans`：正文改用系统界面的无衬线字体（macOS 上是 SF + PingFang）。
- `--zoom 1.2`：初始缩放。不缩放时正文和终端自己的字差不多大，并随终端字号变化（HiDPI 屏幕上也清晰）；
  查看时按 `+` / `-` / `0` 放大、缩小、还原。
- `--no-smooth-scroll`：滚动时直接跳到目标位置，不做动画（比如通过很慢的 SSH 连接使用时）。
- `--export out.png [--width 1200] [--font-size 16]`：不进入交互界面，直接把整页导出为 PNG。

## Neovim 插件

仓库本身就是一个 Neovim 插件（Neovim 0.9+），提供阅读模式：渲染好的页面直接盖在源码窗口上。

**阅读模式**：比 Normal 再「退出一层」的状态。在 Markdown 缓冲区的 Normal 模式下按 `<Esc>`
（也就是从 Insert 模式按两下 `<Esc>`）或 `:TermdRead`，页面就盖在源码窗口上；打开 Markdown 文件时默认也直接进入
（`reader = false` 关闭）。

- 页面是一个盖在源码窗口上、不能进入的浮动窗口，窗口里的缓冲区和焦点仍是源码：
  像平常一样移动光标、滚动（`j`、`<C-d>`、`gg`、`/`、`n`、鼠标滚轮），页面跟着走，
  光标直接显示在页面上它所在的那个字上（在 `**`、链接地址、`# ` 这类页面上不显示的标记里时，
  显示成紧挨着同一行文字的细竖线；在空行上时，显示在前后两块内容之间）；再按 `<Esc>` 不会有反应；
- 打开 `hlsearch` 时，上一次搜索的匹配也在页面上高亮，光标所在的那个颜色更深，`:noh` 之后消失；
- 显示的是缓冲区当前的内容，包括没保存的修改；
- 因为当前缓冲区还是文件本身，neo-tree 能定位到文件，状态栏、`:w`、`<C-^>` 也都照常；
- `q` / `<CR>`：退出阅读；
- 编辑就会退出阅读：进入 Insert、Visual、操作符（`d`、`c`、`y`…）或者文本发生变化（`x`、`p`、`u`、`:s`）；
  `:w`、搜索这类命令行操作不会退出；
- 进出阅读时 termd 一直在后台运行，所以重新进入是即时的（`:TermdClose` 才会停掉它，
  之后 `<Esc>` 不再进入阅读，直到再次 `:TermdRead`）；
- diff、浮动窗口或正在执行宏时，`<Esc>` 保持原样；
  `read_key` 可以换成别的键，设为 `false` 则只用 `:TermdRead`；
- 自动进入只在文件第一次显示时发生：退出阅读后切走再切回来，仍然是源码；
- Neovim 不支持自定义模式，状态栏的模式仍显示 NORMAL；状态栏可以用 `require("termd").reading()`
  判断当前窗口是否在阅读。阅读时终端光标隐藏（Neovim 0.12 把浮动窗口下面的光标画成 Replace 模式的形状，所以 `guicursor` 的 `n` 和 `r` 都要隐藏）；
  光标、搜索匹配、当前匹配的颜色分别取自 `TermdCursor`（默认链接到 `Title`）、`TermdSearch`（`Search`）、
  `TermdCurSearch`（`CurSearch`）高亮组的背景色（没有就用文字颜色），再和背景混合。
  插件比 termd 新时（termd 不支持在页面上画光标），退回到页面左边标出光标所在行的竖条。

滚动：页面跟随的是源码窗口，而不只是光标所在的行。源码窗口里显示的那些行对应页面上的一段，
这一段通常比窗口高；光标在源码窗口第一行时页面和源码的开头对齐，在最后一行时结尾对齐，
中间按比例滑动。这样光标所在行在两边的高度大致相同，光标每走一行页面移动的量都差不多
（不管那一行在页面上是空行还是大标题），不会来回跳；滚动源码窗口（`<C-e>`、鼠标滚轮）页面也跟着滚。
页面要移动超过一行时用几帧滚过去（`smooth`，单位毫秒，设为 0 则直接跳）。

要求：

- 终端支持 Unicode 占位符：kitty（0.28+）、Ghostty。WezTerm 不支持。
- `termguicolors` 打开（图像是用前景色区分的）。
- tmux 里需要 `set -g allow-passthrough on`，外层终端同样得是 kitty 或 Ghostty。
- 图像在本机通过共享内存（kitty）或临时文件（其他终端）传给终端，写进终端的只是很短的转义序列。
  通过 SSH 时只能把像素内嵌在转义序列里，由于 Neovim 的界面进程同时也在往终端写，屏幕更新很频繁时
  偶尔会出现乱码或缺块，按 `<C-l>` 重绘即可。
- 插件和 termd 要用同一个版本。

用 lazy.nvim 安装：

```lua
{
  "yangtau/termd",
  ft = "markdown",       -- 阅读模式需要：打开 Markdown 文件时就加载插件
  cmd = { "TermdRead", "TermdClose" },
  opts = {
    -- 下面都是默认值
    cmd = "termd",        -- termd 可执行文件
    args = {},            -- 额外参数，比如 { "--sans", "--zoom", "0.9" }
    smooth = 120,         -- 页面滚到位用的毫秒数；0 表示直接跳
    cell_size = nil,      -- 终端没报告字符格像素尺寸时手动指定，如 { 10, 21 }
    reader = true,        -- 打开 Markdown 文件时进入阅读模式
    read_key = "<Esc>",   -- Normal 模式下进入阅读的键；false 表示不设
  },
}
```

完整说明见 `:help termd`，出问题时可以运行 `:checkhealth termd`。

## 支持的语法

标题、段落、粗体 / 斜体 / 删除线、行内代码、链接（着色加下划线）、有序 / 无序 / 任务列表、引用、
代码块（语法高亮）、表格（含对齐，见下）、分隔线、本地图片（PNG / JPEG / GIF / WebP / BMP）、
YAML front matter（显示为页首的属性表）。
想看效果可以打开 [`examples/demo.md`](examples/demo.md)，它把这些格式连同各种嵌套都用了一遍。

表格放不下时依次这样处理：先突破正文栏、最宽用到整个终端宽度；还放不下就让单元格折行
（短词的列保持原宽，长文本的列折行）；每列都缩到最窄仍然放不下，才保持舒适的列宽并横向滚动。
表格比一屏长时，表头会停在视口顶部。

暂未支持：远程图片、数学公式、脚注、文内搜索。

## 排查问题

启动慢、字体不对或图像显示异常时，可以设置 `TERMD_DEBUG=1`，退出后会在 stderr 打印启动各阶段的耗时、
选中的字体和使用的传输方式：

```sh
TERMD_DEBUG=1 termd README.md
```

开发相关的内容（实现、构建与发布）见 [DEVELOPMENT.md](DEVELOPMENT.md)。
