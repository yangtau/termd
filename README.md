# termd

在终端里以「渲染后的页面」预览 Markdown：把文档按像素排版成图像，通过
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) 显示，并支持像素级滚动。

和 `glow`、`mdcat` 这类字符渲染不同，termd 画的是真正的排版结果：比例字体、不同字号的标题、
圆角代码块、表格、内嵌图片。

页面没有自己的底色：背景是透明的，直接透出终端的背景（包括终端设置的不透明度、模糊或背景图），
文字颜色也取自终端，所以不管用什么配色方案都不会出现一块突兀的白底或黑底。

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

| 按键 | 动作 |
|:-----|:-----|
| `j` / `k`、`↓` / `↑`、鼠标滚轮 | 上下滚动 |
| `space` / `b`、`PgDn` / `PgUp` | 翻页 |
| `d` / `u` | 半页 |
| `g` / `G` | 顶部 / 底部 |
| `+` / `-` / `0` | 放大 / 缩小 / 还原 |
| `r` | 重新加载（文件变化时也会自动重载） |
| `q` / `Esc` | 退出 |

常用选项：

- `--theme auto|dark|light`：默认 `auto`，沿用终端的前景 / 背景色，页面背景透明。
  强制指定的明暗和终端实际背景相反时，才会画一块不透明的页面底色。
- `--font NAME`、`--mono-font NAME`：指定正文和代码字体。默认用系统界面字体
  （macOS 上是 SF + PingFang，Linux 上问 fontconfig），中文字体按 `LANG` 选简体 / 繁体 / 日文 / 韩文字形。
- `--zoom 1.2`：初始缩放。
- `--no-smooth-scroll`：滚动时直接跳到目标位置，不做动画（比如通过很慢的 SSH 连接使用时）。
- `--export out.png [--width 1200] [--font-size 16]`：不进入交互界面，直接把整页导出为 PNG。

## 支持的语法

标题、段落、粗体 / 斜体 / 删除线、行内代码、链接（仅着色）、有序 / 无序 / 任务列表、引用、
代码块（语法高亮）、表格（含对齐）、分隔线、本地图片（PNG / JPEG / GIF / WebP / BMP）。

暂未支持：远程图片、数学公式、脚注、链接点击、文内搜索。

## 实现

文档先被排成一张固定宽度的长页面，再按 512px 高的水平分块光栅化成 RGBA 像素，
通过 Kitty 图像协议上传给终端。中间不经过 PNG 之类的图片编码。

- `src/doc.rs`：`pulldown-cmark` 解析，`cosmic-text` 排版，`tiny-skia` 画形状，字形直接贴到像素缓冲上。
  排版是增量的：只排到需要显示的位置就可以出图，剩下的在空闲时继续，所以打开一个很长的文档和打开一个短文档一样快。
  页面图像只覆盖正文那一栏而不是整个终端宽度，两侧的空白不需要传输。
- `src/fonts.rs`：字体选择。从系统字体索引里只把选中的西文、等宽、中文字体交给排版引擎，
  并且每段文字都指定一个该字体确实存在的字重。这样既保证中文粗体不会被替换成别的字体，
  也避免了排版引擎为了找后备字体反复打开所有已安装的字体文件。文档里出现这些字体没有的字符（emoji、其他文字）时才按需补充。
- `src/font_cache.rs`：系统字体索引的磁盘缓存。建立索引要打开机器上的每一个字体文件，字体多的时候（比如 macOS 上
  一千多个字体）要几百毫秒，重启后第一次甚至要几秒，是启动过程中最慢的一步。索引会保存到
  `~/Library/Caches/termd/fonts`（Linux 上是 `~/.cache/termd/fonts`），同时记下各字体目录的修改时间；
  安装或删除字体后目录的修改时间会变，下次启动就自动重新扫描。删掉这个文件也可以强制重新扫描。
- `src/highlight.rs`：`syntect` 语法高亮，在后台线程进行。某种语言第一次高亮要编译语法定义（shell 大约 45ms），
  比显示首屏需要的其他所有工作都慢，所以代码块先以纯色显示，高亮结果出来后再把颜色补上。
- `src/main.rs`：查看器。每个分块只上传一次；滚动时只发送「显示某块的某个区域」的放置指令，不需要重新传图，
  所以可以每 8ms 发一帧来做平滑滚动。每帧最多光栅化一个新分块，视口附近的分块在空闲时预先上传。
- `src/kitty.rs`：协议编码。终端和 termd 在同一台机器上并且支持时，像素通过 POSIX 共享内存传给终端
  （启动时探测，kitty 支持）；否则用 zlib 压缩 + base64 内嵌在转义序列里（比如通过 SSH）。
- `src/term.rs`：启动时探测终端的前景 / 背景色、图像协议支持、共享内存支持和窗口像素尺寸。
- `src/theme.rs`：配色，全部由终端的前景色和背景色混合得到。

字号取自终端字符格的像素高度，因此在 HiDPI 屏幕上会自动保持清晰。

排查启动慢的问题时可以设置 `TERMD_DEBUG=1`，退出后会在 stderr 打印启动各阶段的耗时、
选中的字体和使用的传输方式：

```sh
TERMD_DEBUG=1 termd README.md
```

## 开发与发布

`nix develop` 进入带 Rust 工具链的开发环境。

`.github/workflows/nix.yml` 在每次推送到 main、推送 `v*` 标签和 PR 时，在 Linux x86_64 和
macOS arm64 上执行 `nix build`，并把结果推送到 Cachix（需要仓库 secret `CACHIX_AUTH_TOKEN`）。

发布新版本：改 `Cargo.toml` 里的 `version`，提交到 main，然后打同名标签：

```sh
git tag v0.2.0 && git push origin v0.2.0
```

Nix 的构建结果只取决于源码，所以标签指向的提交只要在 main 上构建过，就已经在缓存里了。
