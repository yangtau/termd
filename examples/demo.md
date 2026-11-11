---
title: termd 演示文档
author: 某人 <someone@example.com>
tags: 排版, typography, 测试
note: YAML front matter 显示为页首的属性表；这一行特意写得很长，用来看属性值在一行放不下的时候怎样折行、和左边的键怎样对齐。
---

# termd 演示文档

这是一段**中文**与 English 混排的正文，包含 _斜体 italic_、**_粗斜体 bold italic_**、~~删除线~~、`inline code` 和[链接](https://example.com)。
终端里看到的不是字符画，而是一张按像素排版的页面。

这份文档把常见的格式都用了一遍，改排版的时候拿它对照：正文、标题、列表、代码、引用、表格、图片，以及它们互相嵌套的样子。

## 正文

### 长段落

排版的好坏，多半要在长段落里才看得出来。字号、行距和每行的字数互相牵制：行太长，眼睛换行时容易找不到下一行的开头；行太短，又要不停地折返。中文的一行大约三十到四十个字比较舒服，西文则是六十到七十五个字符。行距也不是越大越好，正文一般取字号的一点五到一点七倍，标题要更紧一些，否则几行标题会散开，看起来不像一个整体。

Typography is the craft of endowing human language with a durable visual form. A page of running text should have an even colour: no rivers of white, no words crowded together, no lines so long that the eye loses its place on the way back. The quick brown fox jumps over the lazy dog — 1234567890, “quotes”, ‘single’, it’s, e.g., i.e., fi fl ffi.

中英文混排时 English words 和数字 2026 夹在汉字中间，基线要对齐，粗细要相称。标点也要看：逗号，句号。顿号、分号；冒号：问号？感叹号！「直角引号」『双直角引号』“弯引号”（括号）《书名号》——破折号……省略号。

### 行内样式

- 行内代码出现在各种位置：`开头的 code` 在句子里，句子中间的 `git rebase -i HEAD~3`，紧挨着标点的`code`，以及结尾的 `code`
- 一行里有好几个：`j` / `k`、`↓` / `↑`、`space` / `b`、`PgDn` / `PgUp`、`g` / `G`
- 和别的样式叠在一起：**粗体里的 `code`**、_斜体里的 `code`_、~~删除线里的 `code`~~、[链接里的 `code`](https://example.com)
- 很长的行内代码会折行：`cargo run --release -- examples/demo.md --theme light --zoom 1.2 --mono-font "JetBrains Mono" --no-smooth-scroll`
- 链接：[普通链接](https://example.com)、[**粗体链接**](https://example.com)、<https://example.com/autolink>、[跳到「表格」一节](#表格)
- 符号和 emoji：→ ← ↑ ↓ ⇒ ≠ ≤ ≥ ± × ÷ ∞ ✓ ✗ ⌘ ⌥ ⇧ ⌃ ⏎ 😀 🎉 🚀 ❤️ 👍🏽
- 其他文字：日本語のテキスト、한국어 텍스트、Ελληνικά、кириллица

硬换行（行尾两个空格）：  
第二行紧跟在第一行下面，  
第三行也是。

### 各级标题

# 一级标题 Heading 1

## 二级标题 Heading 2

### 三级标题 Heading 3

#### 四级标题 Heading 4

##### 五级标题 Heading 5

###### 六级标题 Heading 6

## 一个很长很长的二级标题，长到一行放不下，用来看标题折行之后的行距和它与下面正文之间的距离

### 标题里的 `inline code`、**粗体**和 _italic_

标题后面紧跟着的正文。

## 列表

### 无序列表

- 第一项
- 第二项，带有嵌套：
  - 嵌套 A
  - 嵌套 B with `code`
    - 第三层 α
    - 第三层 β，这一项写得长一些，长到需要折行，看看折行之后第二行是不是和第一行的文字对齐，而不是和项目符号对齐
  - 嵌套 C
- 第三项

### 有序列表

1. 有序列表
2. Second item
3. Third item
   1. 嵌套的有序项
   2. 再一项
      - 有序里面套无序
      - 再套一层
4. 第四项

从别的数字开始，并且超过两位数：

98. 第九十八项
99. 第九十九项
100. 第一百项
101. 第一百零一项

### 任务列表

- [x] 已完成的任务
- [x] 已完成的任务，带 `code` 和 [链接](https://example.com)
- [ ] 未完成的任务
- [ ] 未完成的任务，下面有子任务
  - [x] 子任务一
  - [ ] 子任务二

### 松散的列表

每一项之间有空行，项里面可以有好几段，也可以有代码块和引用：

1. 第一步：安装。

   这一段属于第一步，和上面的句子之间隔着一个段落间距。

   ```sh
   cargo install --path .
   ```

2. 第二步：打开一份文档。

   > 引用也可以放在列表项里面。

3. 第三步：没有第三步。

## 代码

行内的代码是 `let x = 42;` 这样；下面是代码块。

```rust
use std::collections::HashMap;

/// 统计每个单词出现的次数。
fn count_words<'a>(text: &'a str) -> HashMap<&'a str, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}

fn main() {
    // 语法高亮
    let greeting = "hello, 世界";
    let counts = count_words("the quick brown fox jumps over the lazy dog the end");
    println!("{greeting} {} {:?}", 42, counts.get("the"));
}
```

```python
from dataclasses import dataclass


@dataclass
class Point:
    x: float
    y: float

    def distance(self, other: "Point") -> float:
        """两点之间的距离。"""
        return ((self.x - other.x) ** 2 + (self.y - other.y) ** 2) ** 0.5


if __name__ == "__main__":
    print(f"{Point(0, 0).distance(Point(3, 4)):.1f}")  # 5.0
```

```json
{
  "name": "termd",
  "version": "0.1.0",
  "features": ["tables", "code", "images"],
  "nested": { "enabled": true, "ratio": 0.8, "missing": null }
}
```

```sh
# 一行很长的命令，看代码块里怎么折行
nix run github:yangtau/termd -- README.md --theme auto --zoom 1.2 --mono-font "JetBrains Mono" --no-smooth-scroll --width 1200 --font-size 16
```

```diff
- let font_px = cell_h * 0.8;
+ let font_px = cell_h;
```

没有标注语言的代码块：

```
$ termd examples/demo.md
j/k scroll · space/b page · q quit
```

缩进四格的代码块：

    plain text,
        缩进保持原样。

## 引用

> 引用块里的文字会变淡。
>
> — 某人

> 引用可以有好几段，也可以嵌套。
>
> > 这是嵌套的引用，里面还有 `code` 和**粗体**。
> >
> > > 第三层。
>
> 回到第一层。里面也可以有列表：
>
> - 列表项一
> - 列表项二
>
> 和代码块：
>
> ```rust
> let quoted = true;
> ```

## 表格

三种对齐方式：

| 协议           |       滚动方式       |   评价 |
| :------------- | :------------------: | -----: |
| Kitty graphics | 只传一次，按区域显示 | 最适合 |
| iTerm2 inline  |       每帧重发       |   一般 |
| Sixel          |   每帧重发，体积大   |   兜底 |

单元格里的行内样式：

| 按键                           | 动作                 | 备注                            |
| ------------------------------ | -------------------- | ------------------------------- |
| `j` / `k`、`↓` / `↑`、鼠标滚轮 | 上下滚动             | **平滑**滚动，可以关掉          |
| `space` / `b`                  | 翻页                 | 也可以用 `PgDn` / `PgUp`        |
| `+` / `-` / `0`                | 放大 / 缩小 / 还原   | 见 [`--zoom`](#表格)            |
| `e`                            | 在 `$EDITOR` 里打开  | ~~只读~~ 现在可以直接跳到那一行 |
| `q`                            | 退出                 |                                 |

数字右对齐：

| 文件          |   行数 | 占比 |     大小 |
| :------------ | -----: | ---: | -------: |
| `doc.rs`      |  2,650 |  41% |  118 KiB |
| `viewer.rs`   |  1,210 |  19% |   52 KiB |
| `fonts.rs`    |    640 |  10% |   27 KiB |
| `kitty.rs`    |    343 |   5% |   13 KiB |
| 其余 8 个文件 |  1,560 |  25% |   61 KiB |
| **合计**      |  6,403 | 100% |  271 KiB |

单元格里有很长的文字，需要折行：

| 方案   | 说明                                                                                                                   | 结论 |
| ------ | ---------------------------------------------------------------------------------------------------------------------- | ---- |
| 字符画 | 把 Markdown 转成带颜色的字符，标题只能靠加粗和颜色区分，表格用制表符画线；任何终端都能用，但中英文混排时很难对齐。     | 够用 |
| 图像   | 把整页按像素排版成图像再交给终端显示，比例字体、字号、圆角都可以有；需要终端支持图像协议，也需要自己处理滚动和选择。 | 好看 |

列很多的宽表格（放不下时先伸出正文栏，再折行，最后横向滚动）：

| 编号 | 名称     | 状态   | 负责人 | 优先级 | 创建日期   | 截止日期   | 标签           | 预计工时 | 实际工时 | 完成度 | 备注                     |
| ---: | :------- | :----- | :----- | :----: | :--------- | :--------- | :------------- | -------: | -------: | -----: | :----------------------- |
|    1 | 字体选择 | 已完成 | Alice  |   高   | 2026-10-01 | 2026-10-08 | fonts, macOS   |     16 h |     20 h |   100% | Core Text 按需查找       |
|    2 | 行内代码 | 已完成 | Bob    |   中   | 2026-10-03 | 2026-10-10 | layout         |      8 h |      6 h |   100% | 背景两侧留出真实的空间   |
|    3 | 宽表格   | 进行中 | Carol  |   高   | 2026-10-12 | 2026-11-20 | tables, scroll |     24 h |     18 h |    75% | 表头固定，横向滚动       |
|    4 | 数学公式 | 未开始 | Dave   |   低   | 2026-11-01 | 2026-12-31 | math           |     40 h |      0 h |     0% | 暂未支持                 |
|    5 | 文内搜索 | 未开始 | Erin   |   中   | 2026-11-05 | 2026-12-15 | search         |     12 h |      0 h |     0% | 需要先有选择和高亮的基础 |

## 图片

本地图片按原始尺寸显示（按字号缩放），放不下时缩小：

![logo](logo.png)

找不到的图片和远程图片显示为占位文字：![不存在的图片](missing.png)、![远程图片](https://example.com/remote.png)。

## 其他

分隔线：

---

原始 HTML 块原样显示：

<details>
<summary>点开看看</summary>
这里的内容不会被渲染成 HTML。
</details>

转义与实体：\*不是斜体\*、\`不是代码\`、&lt;tag&gt;、&amp;、&copy;、1 &times; 2。

###### 结束

> 结束。
