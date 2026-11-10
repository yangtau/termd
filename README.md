# termd

在终端里以「渲染后的页面」预览 Markdown：把文档按像素排版成图像，通过
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) 显示，并支持像素级滚动。

和 `glow`、`mdcat` 这类字符渲染不同，termd 画的是真正的排版结果：比例字体、不同字号的标题、
圆角代码块、表格、内嵌图片。

## 使用

```sh
cargo install --path .
termd README.md
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

- `--theme auto|dark|light`：默认跟随终端背景色。
- `--font NAME`、`--mono-font NAME`：指定正文和代码字体。
- `--zoom 1.2`：初始缩放。
- `--export out.png [--width 1200] [--font-size 16]`：不进入交互界面，直接把整页导出为 PNG。

## 支持的语法

标题、段落、粗体 / 斜体 / 删除线、行内代码、链接（仅着色）、有序 / 无序 / 任务列表、引用、
代码块（语法高亮）、表格（含对齐）、分隔线、本地图片（PNG / JPEG / GIF / WebP / BMP）。

暂未支持：远程图片、数学公式、脚注、链接点击、文内搜索。

## 实现

- `src/doc.rs`：`pulldown-cmark` 解析，`cosmic-text` 排版，`tiny-skia` 绘制，`syntect` 高亮。
  整篇文档排成一张固定宽度的长页面，可以按任意水平条带光栅化。
- `src/main.rs`：页面按 512px 高的分块上传给终端，每块只传一次；滚动时只发送
  「显示某块的某个区域」的放置指令，所以滚动不需要重新传图。视口附近的分块会在空闲时预传。
- `src/kitty.rs`：协议编码（zlib 压缩的 RGBA，分片 base64）。
- `src/term.rs`：启动时探测终端背景色、图像协议支持和窗口像素尺寸。

字号取自终端字符格的像素高度，因此在 HiDPI 屏幕上会自动保持清晰。
