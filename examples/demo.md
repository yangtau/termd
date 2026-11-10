---
title: front matter is skipped
---

# termd 演示文档

这是一段**中文**与 English 混排的正文，包含 *斜体 italic*、~~删除线~~、`inline code` 和[链接](https://example.com)。
终端里看到的不是字符画，而是一张按像素排版的页面。

## 列表

- 第一项
- 第二项，带有嵌套：
  - 嵌套 A
  - 嵌套 B with `code`
- 第三项

1. 有序列表
2. Second item
3. Third item

- [x] 已完成的任务
- [ ] 未完成的任务

## 代码

```rust
fn main() {
    // 语法高亮
    let greeting = "hello, 世界";
    println!("{greeting} {}", 42);
}
```

## 引用与表格

> 引用块里的文字会变淡。
>
> — 某人

| 协议 | 滚动方式 | 评价 |
|:-----|:--------:|-----:|
| Kitty graphics | 只传一次，按区域显示 | 最适合 |
| iTerm2 inline | 每帧重发 | 一般 |
| Sixel | 每帧重发，体积大 | 兜底 |

---

### 图片

![logo](logo.png)

###### 六级标题

结束。
