//! Markdown → laid-out page → pixels.
//!
//! `Engine::layout` walks the Markdown events once and produces a `Doc`: a
//! flat list of positioned draw items (text buffers, rectangles, images) on a
//! page of fixed pixel width. `Doc::render` rasterizes any horizontal strip
//! of that page, which is what lets the viewer upload the page tile by tile.

use crate::theme::{Rgb, Theme};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight,
    Wrap,
};
use image::RgbaImage;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Glyph metadata tag marking inline code, used to paint its background.
const META_CODE: usize = 1;

const HEADING_SCALE: [f32; 6] = [2.0, 1.5, 1.25, 1.0, 0.875, 0.85];
const BODY_LINE: f32 = 1.6;
const HEADING_LINE: f32 = 1.3;
const CODE_SCALE: f32 = 0.875;
const CODE_LINE: f32 = 1.5;
/// Content column width limit, in em.
const MAX_WIDTH_EM: f32 = 46.0;

/// Long-lived rendering state: fonts, glyph cache, syntax definitions and
/// decoded images. Expensive to create, reused across re-layouts.
pub struct Engine {
    fs: FontSystem,
    swash: SwashCache,
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    images: HashMap<PathBuf, Option<RgbaImage>>,
    sans: Option<String>,
    mono: Option<String>,
}

pub struct LayoutOpts<'a> {
    /// Page width in pixels.
    pub page_w: u32,
    /// Body font size in pixels.
    pub font_px: f32,
    /// Directory relative image paths are resolved against.
    pub base_dir: &'a Path,
    pub theme: &'a Theme,
}

pub struct Doc {
    pub width: u32,
    pub height: u32,
    bg: Rgb,
    link: Rgb,
    /// How far above/below its box an item may paint (glyph overhang).
    slack: f32,
    items: Vec<Item>,
}

struct Item {
    y: f32,
    h: f32,
    kind: Kind,
}

enum Kind {
    Text { x: f32, buf: Buffer, color: Rgb },
    Rect { x: f32, w: f32, radius: f32, color: Rgb },
    Image { x: f32, img: RgbaImage },
    Check { x: f32, checked: bool, border: Rgb },
}

#[derive(Clone, Default)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    color: Option<Rgb>,
}

struct TextStyle<'a> {
    size: f32,
    line_h: f32,
    mono: bool,
    bold: bool,
    sans_family: Family<'a>,
    mono_family: Family<'a>,
}

fn text_style<'a>(
    sans: &'a Option<String>,
    mono: &'a Option<String>,
    size: f32,
    line_scale: f32,
) -> TextStyle<'a> {
    TextStyle {
        size,
        line_h: (size * line_scale).round(),
        mono: false,
        bold: false,
        sans_family: sans.as_deref().map_or(Family::SansSerif, Family::Name),
        mono_family: mono.as_deref().map_or(Family::Monospace, Family::Name),
    }
}

fn color(c: Rgb) -> Color {
    Color::rgb(c.0, c.1, c.2)
}

fn make_buffer(fs: &mut FontSystem, spans: &[Span], st: &TextStyle, width: Option<f32>) -> Buffer {
    let mut buf = Buffer::new(fs, Metrics::new(st.size, st.line_h));
    buf.set_wrap(Wrap::WordOrGlyph);
    buf.set_size(width, None);
    let base = Attrs::new()
        .family(if st.mono { st.mono_family } else { st.sans_family })
        .weight(if st.bold { Weight::BOLD } else { Weight::NORMAL });
    let rich = spans.iter().map(|s| {
        let mut a = base.clone();
        if s.bold {
            a = a.weight(Weight::BOLD);
        }
        if s.italic {
            a = a.style(Style::Italic);
        }
        if s.strike {
            a = a.strikethrough();
        }
        if s.code {
            a = a
                .family(st.mono_family)
                .metadata(META_CODE)
                .metrics(Metrics::new(st.size * 0.88, st.line_h));
        }
        if let Some(c) = s.color {
            a = a.color(color(c));
        }
        (s.text.as_str(), a)
    });
    buf.set_rich_text(rich, &base, Shaping::Advanced, None);
    buf.shape_until_scroll(fs, false);
    buf
}

/// (widest line, total height) of a shaped buffer.
fn measure(buf: &Buffer) -> (f32, f32) {
    buf.layout_runs().fold((0.0, 0.0), |(w, h): (f32, f32), run| {
        (w.max(run.line_w), h.max(run.line_top + run.line_height))
    })
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF      // radicals, kana, CJK punctuation, ideographs
        | 0xAC00..=0xD7AF    // hangul
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0xFF00..=0xFFEF    // fullwidth forms
        | 0x20000..=0x3FFFF) // ideograph extensions
}

fn is_blank(spans: &[Span]) -> bool {
    spans.iter().all(|s| s.text.trim().is_empty())
}

impl Engine {
    pub fn new(sans: Option<String>, mono: Option<String>) -> Engine {
        Engine {
            fs: FontSystem::new(),
            swash: SwashCache::new(),
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            images: HashMap::new(),
            sans,
            mono,
        }
    }

    /// Forget decoded images so edited files are picked up on reload.
    pub fn clear_image_cache(&mut self) {
        self.images.clear();
    }

    pub fn layout(&mut self, markdown: &str, opts: &LayoutOpts) -> Doc {
        let theme = opts.theme;
        let em = opts.font_px;
        let page_w = opts.page_w as f32;
        let content_w = (page_w - 2.4 * em).min(MAX_WIDTH_EM * em).max(4.0 * em);
        let left = ((page_w - content_w) / 2.0).max(0.0).round();
        let hl_name = if theme.dark { "base16-ocean.dark" } else { "InspiredGitHub" };

        let mut b = Builder {
            fs: &mut self.fs,
            syntaxes: &self.syntaxes,
            hl_theme: &self.themes.themes[hl_name],
            images: &mut self.images,
            sans: &self.sans,
            mono: &self.mono,
            theme,
            base_dir: opts.base_dir,
            em,
            x: left,
            right: left + content_w,
            y: (1.5 * em).round(),
            margin: 0.0,
            first: true,
            items: Vec::new(),
            spans: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            link: 0,
            heading: None,
            lists: Vec::new(),
            marker: None,
            quotes: Vec::new(),
            code: None,
            html: None,
            image: None,
            table: None,
            in_meta: false,
            soft_break: false,
        };

        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
        for event in Parser::new_ext(markdown, options) {
            b.event(event);
        }
        b.flush_inline(false);

        let height = (b.y + 2.0 * em).ceil() as u32;
        Doc {
            width: opts.page_w,
            height,
            bg: theme.bg,
            link: theme.link,
            slack: em * 2.0,
            items: b.items,
        }
    }
}

enum Marker {
    Bullet,
    Number(u64),
    Task(bool),
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    head_rows: usize,
}

struct Builder<'e> {
    fs: &'e mut FontSystem,
    syntaxes: &'e SyntaxSet,
    hl_theme: &'e syntect::highlighting::Theme,
    images: &'e mut HashMap<PathBuf, Option<RgbaImage>>,
    sans: &'e Option<String>,
    mono: &'e Option<String>,
    theme: &'e Theme,
    base_dir: &'e Path,

    em: f32,
    /// Left edge of the current block (moves right inside lists and quotes).
    x: f32,
    right: f32,
    /// Bottom of the last placed block.
    y: f32,
    /// Bottom margin of the last placed block (collapses with the next top margin).
    margin: f32,
    first: bool,
    items: Vec<Item>,

    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    heading: Option<usize>,
    /// Next ordinal for each open list (`None` = bullet list).
    lists: Vec<Option<u64>>,
    /// Marker waiting to be drawn next to the next placed block.
    marker: Option<Marker>,
    /// Open block quotes: bar x and the y where their content started.
    quotes: Vec<(f32, Option<f32>)>,
    /// Open code block: (language, text).
    code: Option<(String, String)>,
    html: Option<String>,
    /// Open image: (destination, alt text).
    image: Option<(String, String)>,
    table: Option<Table>,
    in_meta: bool,
    /// A soft line break was seen and not yet turned into a space.
    soft_break: bool,
}

impl Builder<'_> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t, false),
            Event::Code(t) => self.text(&t, true),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, true),
            Event::SoftBreak => self.soft_break = true,
            Event::HardBreak => self.text("\n", false),
            Event::Html(t) => match &mut self.html {
                Some(html) => html.push_str(&t),
                None => self.html_block(&t),
            },
            Event::InlineHtml(t) => {
                if t.to_ascii_lowercase().starts_with("<br") {
                    self.text("\n", false);
                }
            }
            Event::Rule => {
                self.flush_inline(true);
                let h = (self.em * 0.12).max(2.0).round();
                let y = self.place(h, 1.5 * self.em, 1.5 * self.em, h);
                self.rect(self.x, y, self.right - self.x, h, 0.0, self.theme.border);
            }
            Event::TaskListMarker(checked) => self.marker = Some(Marker::Task(checked)),
            Event::FootnoteReference(name) => self.text(&format!("[{name}]"), false),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush_inline(true),
            Tag::Heading { level, .. } => {
                self.flush_inline(true);
                self.heading = Some(level as usize);
            }
            Tag::BlockQuote(_) => {
                self.flush_inline(true);
                self.quotes.push((self.x, None));
                self.x += 1.2 * self.em;
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline(true);
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .next()
                        .unwrap_or("")
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush_inline(true);
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.flush_inline(true);
                self.lists.push(start);
                self.x += 1.8 * self.em;
            }
            Tag::Item => {
                self.flush_inline(true);
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        Marker::Number(*n - 1)
                    }
                    _ => Marker::Bullet,
                });
            }
            Tag::Table(aligns) => {
                self.flush_inline(true);
                self.table = Some(Table { aligns, rows: Vec::new(), row: Vec::new(), head_rows: 0 });
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            Tag::MetadataBlock(_) => self.in_meta = true,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(false),
            TagEnd::Heading(_) => {
                self.flush_inline(false);
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline(true);
                self.x -= 1.2 * self.em;
                if let Some((bar_x, Some(top))) = self.quotes.pop() {
                    let w = (0.22 * self.em).max(2.0).round();
                    self.rect(bar_x, top, w, self.y - top, 0.0, self.theme.quote_bar);
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    self.code_block(&lang, &text);
                }
            }
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.html_block(&html);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline(true);
                self.lists.pop();
                self.x -= 1.8 * self.em;
                if self.lists.is_empty() {
                    self.margin = self.margin.max(0.8 * self.em);
                }
            }
            TagEnd::Item => {
                self.flush_inline(true);
                // An item with no content still gets its marker.
                if self.marker.is_some() {
                    let line = (self.em * BODY_LINE).round();
                    self.place(line, 0.25 * self.em, 0.25 * self.em, line);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                    if tag == TagEnd::TableHead {
                        t.head_rows = t.rows.len();
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_block(t);
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            TagEnd::Image => {
                if let Some((dest, alt)) = self.image.take() {
                    self.image_block(&dest, &alt);
                }
            }
            TagEnd::MetadataBlock(_) => self.in_meta = false,
            _ => {}
        }
    }

    fn text(&mut self, t: &str, code: bool) {
        // A source line break is a space, except between two CJK characters,
        // where a space would be a visible gap.
        if std::mem::take(&mut self.soft_break) && self.image.is_none() {
            let prev = self.spans.last().and_then(|s| s.text.chars().next_back());
            let cjk_join = prev.is_some_and(is_cjk) && t.chars().next().is_some_and(is_cjk);
            if !cjk_join && let Some(last) = self.spans.last_mut() {
                last.text.push(' ');
            }
        }
        if self.in_meta {
        } else if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
        } else if let Some(html) = &mut self.html {
            html.push_str(t);
        } else if let Some((_, alt)) = &mut self.image {
            alt.push_str(t);
        } else {
            self.spans.push(Span {
                text: t.to_string(),
                bold: self.bold > 0,
                italic: self.italic > 0,
                strike: self.strike > 0,
                code,
                color: (self.link > 0).then_some(self.theme.link),
            });
        }
    }

    fn text_color(&self) -> Rgb {
        if self.quotes.is_empty() { self.theme.fg } else { self.theme.dim }
    }

    /// Reserve vertical space for a block of height `h` and return its top.
    /// `first_line_h` positions a pending list marker next to the first line.
    fn place(&mut self, h: f32, margin_top: f32, margin_bottom: f32, first_line_h: f32) -> f32 {
        let gap = if self.first { 0.0 } else { self.margin.max(margin_top) };
        let y = (self.y + gap).round();
        self.first = false;
        self.y = y + h;
        self.margin = margin_bottom;
        for quote in &mut self.quotes {
            quote.1.get_or_insert(y);
        }
        if let Some(marker) = self.marker.take() {
            self.draw_marker(marker, y, first_line_h);
        }
        y
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgb) {
        self.items.push(Item { y, h, kind: Kind::Rect { x, w, radius, color } });
    }

    fn draw_marker(&mut self, marker: Marker, y: f32, line_h: f32) {
        let em = self.em;
        match marker {
            Marker::Bullet => {
                let d = (0.34 * em).round().max(3.0);
                self.rect(self.x - em, y + (line_h - d) / 2.0, d, d, d / 2.0, self.text_color());
            }
            Marker::Number(n) => {
                let st = text_style(self.sans, self.mono, em, BODY_LINE);
                let span = Span { text: format!("{n}."), ..Span::default() };
                let buf = make_buffer(self.fs, &[span], &st, None);
                let (w, h) = measure(&buf);
                let x = self.x - 0.45 * em - w;
                self.items.push(Item { y, h, kind: Kind::Text { x, buf, color: self.text_color() } });
            }
            Marker::Task(checked) => {
                let s = (0.95 * em).round();
                self.items.push(Item {
                    y: y + ((line_h - s) / 2.0).round(),
                    h: s,
                    kind: Kind::Check { x: self.x - 1.45 * em, checked, border: self.theme.border },
                });
            }
        }
    }

    /// Turn the accumulated inline spans into a paragraph or heading.
    /// `tight` is used for text that is not wrapped in a paragraph (tight
    /// list items), which gets smaller margins.
    fn flush_inline(&mut self, tight: bool) {
        if self.table.is_some() {
            return; // table cells collect their own spans
        }
        let spans = std::mem::take(&mut self.spans);
        if is_blank(&spans) {
            return;
        }
        let em = self.em;
        let (mut st, mt, mb) = match self.heading {
            Some(level) => {
                let scale = HEADING_SCALE[level.clamp(1, 6) - 1];
                let mut st = text_style(self.sans, self.mono, (em * scale).round(), HEADING_LINE);
                st.bold = true;
                (st, 1.5 * em, 0.9 * em)
            }
            None if tight => (text_style(self.sans, self.mono, em, BODY_LINE), 0.25 * em, 0.25 * em),
            None => (text_style(self.sans, self.mono, em, BODY_LINE), 0.8 * em, 0.8 * em),
        };
        st.mono = false;
        let buf = make_buffer(self.fs, &spans, &st, Some(self.right - self.x));
        let (_, text_h) = measure(&buf);

        // h1/h2 get a rule underneath, like GitHub.
        let ruled = matches!(self.heading, Some(1 | 2));
        let rule_gap = if ruled { (0.3 * em).round() } else { 0.0 };
        let rule_h = if ruled { (em * 0.06).max(1.0).round() } else { 0.0 };
        let y = self.place(text_h + rule_gap + rule_h, mt, mb, st.line_h);

        // Backgrounds behind inline code.
        let code_h = (st.size * 1.3).round();
        let pad = (st.size * 0.18).round();
        let mut boxes: Vec<(f32, f32, f32)> = Vec::new(); // x0, x1, top
        for run in buf.layout_runs() {
            let top = run.line_top + ((run.line_height - code_h) / 2.0).round();
            let mut open: Option<(f32, f32)> = None;
            for g in run.glyphs {
                if g.metadata == META_CODE {
                    let (x0, _) = open.unwrap_or((g.x, 0.0));
                    open = Some((x0.min(g.x), g.x + g.w));
                } else if let Some((x0, x1)) = open.take() {
                    boxes.push((x0, x1, top));
                }
            }
            if let Some((x0, x1)) = open {
                boxes.push((x0, x1, top));
            }
        }
        for (x0, x1, top) in boxes {
            let (x, w) = (self.x + x0 - pad, x1 - x0 + 2.0 * pad);
            self.rect(x, y + top, w, code_h, st.size * 0.22, self.theme.code_bg);
        }

        let color = if self.heading == Some(6) { self.theme.dim } else { self.text_color() };
        self.items.push(Item { y, h: text_h, kind: Kind::Text { x: self.x, buf, color } });
        if ruled {
            self.rect(self.x, y + text_h + rule_gap, self.right - self.x, rule_h, 0.0, self.theme.border);
        }
    }

    fn code_block(&mut self, lang: &str, text: &str) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        let syntax = self
            .syntaxes
            .find_syntax_by_token(lang)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, self.hl_theme);
        let mut spans = Vec::new();
        for line in LinesWithEndings::from(text) {
            match hl.highlight_line(line, self.syntaxes) {
                Ok(parts) => spans.extend(parts.into_iter().map(|(style, s)| Span {
                    text: s.to_string(),
                    color: Some(Rgb(style.foreground.r, style.foreground.g, style.foreground.b)),
                    ..Span::default()
                })),
                Err(_) => spans.push(Span { text: line.to_string(), ..Span::default() }),
            }
        }
        self.mono_block(&spans, self.theme.fg);
    }

    fn html_block(&mut self, html: &str) {
        let html = html.trim();
        if html.is_empty() || (html.starts_with("<!--") && html.ends_with("-->")) {
            return;
        }
        let span = Span { text: html.to_string(), ..Span::default() };
        self.mono_block(&[span], self.theme.dim);
    }

    /// A monospace block on a rounded background (code blocks, raw HTML).
    fn mono_block(&mut self, spans: &[Span], fg: Rgb) {
        let em = self.em;
        let mut st = text_style(self.sans, self.mono, (em * CODE_SCALE).round(), CODE_LINE);
        st.mono = true;
        let (pad_x, pad_y) = (em.round(), (0.8 * em).round());
        let width = self.right - self.x;
        let buf = make_buffer(self.fs, spans, &st, Some(width - 2.0 * pad_x));
        let (_, text_h) = measure(&buf);
        let text_h = text_h.max(st.line_h);
        let h = text_h + 2.0 * pad_y;
        let y = self.place(h, 0.8 * em, 0.8 * em, (em * BODY_LINE).round());
        self.rect(self.x, y, width, h, 0.4 * em, self.theme.code_bg);
        self.items.push(Item {
            y: y + pad_y,
            h: text_h,
            kind: Kind::Text { x: self.x + pad_x, buf, color: fg },
        });
    }

    fn image_block(&mut self, dest: &str, alt: &str) {
        let avail = self.right - self.x;
        // Images are authored in CSS pixels (16px body text); scale to ours.
        let scale = self.em / 16.0;
        let scaled = self.load_image(dest).map(|src| {
            let w = (src.width() as f32 * scale).min(avail).max(1.0);
            let h = (src.height() as f32 * w / src.width() as f32).max(1.0);
            let (w, h) = (w.round() as u32, h.round() as u32);
            if (w, h) == src.dimensions() {
                src.clone()
            } else {
                image::imageops::resize(src, w, h, image::imageops::FilterType::CatmullRom)
            }
        });
        match scaled {
            Some(img) if self.table.is_none() => {
                self.flush_inline(true);
                let h = img.height() as f32;
                let y = self.place(h, 0.8 * self.em, 0.8 * self.em, (self.em * BODY_LINE).round());
                self.items.push(Item { y, h, kind: Kind::Image { x: self.x, img } });
            }
            _ => {
                // Remote, missing or undecodable: show a placeholder inline.
                let label = if alt.is_empty() { dest } else { alt };
                self.spans.push(Span {
                    text: format!("[image: {label}]"),
                    italic: true,
                    color: Some(self.theme.dim),
                    ..Span::default()
                });
            }
        }
    }

    fn load_image(&mut self, dest: &str) -> Option<&RgbaImage> {
        if dest.contains("://") && !dest.starts_with("file://") {
            return None;
        }
        let path = self.base_dir.join(dest.strip_prefix("file://").unwrap_or(dest));
        self.images
            .entry(path)
            .or_insert_with_key(|p| image::open(p).ok().map(|i| i.to_rgba8()))
            .as_ref()
    }

    fn table_block(&mut self, table: Table) {
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let em = self.em;
        let (pad_x, pad_y, line) = ((0.8 * em).round(), (0.4 * em).round(), 1.0f32);
        let st = text_style(self.sans, self.mono, em, 1.5);
        let head_st = TextStyle { bold: true, ..text_style(self.sans, self.mono, em, 1.5) };

        // Pass 1: natural (unwrapped) width of every column.
        let mut cells: Vec<Vec<Buffer>> = Vec::new();
        let mut natural = vec![0.0f32; cols];
        for (r, row) in table.rows.iter().enumerate() {
            let style = if r < table.head_rows { &head_st } else { &st };
            let mut bufs = Vec::new();
            for (c, spans) in row.iter().enumerate() {
                let buf = make_buffer(self.fs, spans, style, None);
                natural[c] = natural[c].max(measure(&buf).0.ceil() + 1.0);
                bufs.push(buf);
            }
            cells.push(bufs);
        }

        // Shrink columns proportionally if the table is wider than the page.
        let avail = self.right - self.x;
        let chrome = cols as f32 * 2.0 * pad_x + (cols as f32 + 1.0) * line;
        let total: f32 = natural.iter().sum();
        let room = (avail - chrome).max(cols as f32 * em);
        let widths: Vec<f32> = if total <= room {
            natural
        } else {
            natural.iter().map(|w| (w * room / total).floor().max(em)).collect()
        };

        // Pass 2: wrap to the final widths and measure row heights.
        let mut heights = Vec::new();
        for row in &mut cells {
            let mut row_h = st.line_h;
            for (c, buf) in row.iter_mut().enumerate() {
                let align = match table.aligns.get(c) {
                    Some(Alignment::Center) => Some(Align::Center),
                    Some(Alignment::Right) => Some(Align::Right),
                    _ => None,
                };
                for l in &mut buf.lines {
                    l.set_align(align);
                }
                buf.set_size(Some(widths[c]), None);
                buf.shape_until_scroll(self.fs, false);
                row_h = row_h.max(measure(buf).1);
            }
            heights.push(row_h);
        }

        let table_w = widths.iter().sum::<f32>() + chrome;
        let table_h = heights.iter().sum::<f32>()
            + heights.len() as f32 * 2.0 * pad_y
            + (heights.len() as f32 + 1.0) * line;
        let y = self.place(table_h, 0.8 * em, 0.8 * em, st.line_h);

        // The grid is the border-colored backdrop showing between cell fills.
        self.rect(self.x, y, table_w, table_h, 0.0, self.theme.border);
        let color = self.text_color();
        let mut cy = y + line;
        for (r, row) in cells.into_iter().enumerate() {
            let cell_h = heights[r] + 2.0 * pad_y;
            let fill = if r < table.head_rows { self.theme.table_head } else { self.theme.bg };
            let mut cx = self.x + line;
            let mut row = row.into_iter();
            for width in &widths {
                let cell_w = width + 2.0 * pad_x;
                self.rect(cx, cy, cell_w, cell_h, 0.0, fill);
                if let Some(buf) = row.next() {
                    self.items.push(Item {
                        y: cy + pad_y,
                        h: heights[r],
                        kind: Kind::Text { x: cx + pad_x, buf, color },
                    });
                }
                cx += cell_w + line;
            }
            cy += cell_h + line;
        }
    }
}

impl Doc {
    /// Rasterize the strip `[y0, y0 + height)` of the page as straight RGBA.
    pub fn render(&mut self, engine: &mut Engine, y0: u32, height: u32) -> Vec<u8> {
        let (w, h) = (self.width.max(1), height.max(1));
        let mut pm = Pixmap::new(w, h).expect("non-zero pixmap size");
        pm.fill(tiny_skia::Color::from_rgba8(self.bg.0, self.bg.1, self.bg.2, 255));

        let (top, bottom) = (y0 as f32, (y0 + height) as f32);
        let shift = Transform::from_translate(0.0, -top);
        for item in &mut self.items {
            if item.y - self.slack >= bottom || item.y + item.h + self.slack <= top {
                continue;
            }
            match &mut item.kind {
                Kind::Rect { x, w, radius, color } => {
                    fill_rrect(&mut pm, *x, item.y, *w, item.h, *radius, *color, shift);
                }
                Kind::Text { x, buf, color: c } => {
                    let (ox, oy) = (x.round() as i32, (item.y - top).round() as i32);
                    let (pw, ph) = (w as i32, h as i32);
                    let data = pm.data_mut();
                    buf.draw(&mut engine.fs, &mut engine.swash, color(*c), |gx, gy, gw, gh, col| {
                        let a = col.a() as u32;
                        if a == 0 {
                            return;
                        }
                        for py in (oy + gy).max(0)..(oy + gy + gh as i32).min(ph) {
                            for px in (ox + gx).max(0)..(ox + gx + gw as i32).min(pw) {
                                let i = (py * pw + px) as usize * 4;
                                blend(&mut data[i..i + 3], [col.r(), col.g(), col.b()], a);
                            }
                        }
                    });
                }
                Kind::Image { x, img } => {
                    let (ox, oy) = (x.round() as i64, (item.y - top).round() as i64);
                    let data = pm.data_mut();
                    for (ix, iy, p) in img.enumerate_pixels() {
                        let (px, py) = (ox + ix as i64, oy + iy as i64);
                        if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 || p[3] == 0 {
                            continue;
                        }
                        let i = (py as usize * w as usize + px as usize) * 4;
                        blend(&mut data[i..i + 3], [p[0], p[1], p[2]], p[3] as u32);
                    }
                }
                Kind::Check { x, checked, border } => {
                    let (x, y, s) = (x.round(), item.y, item.h);
                    let r = s * 0.2;
                    if *checked {
                        fill_rrect(&mut pm, x, y, s, s, r, self.link, shift);
                        let mut pb = PathBuilder::new();
                        pb.move_to(x + s * 0.24, y + s * 0.52);
                        pb.line_to(x + s * 0.43, y + s * 0.70);
                        pb.line_to(x + s * 0.76, y + s * 0.32);
                        if let Some(path) = pb.finish() {
                            let stroke = Stroke { width: (s * 0.13).max(1.5), ..Stroke::default() };
                            pm.stroke_path(&path, &paint(Rgb(255, 255, 255)), &stroke, shift, None);
                        }
                    } else {
                        let t = (s * 0.09).max(1.0).round();
                        fill_rrect(&mut pm, x, y, s, s, r, *border, shift);
                        fill_rrect(&mut pm, x + t, y + t, s - 2.0 * t, s - 2.0 * t, r - t, self.bg, shift);
                    }
                }
            }
        }
        // The background is opaque, so premultiplied == straight alpha.
        pm.take()
    }
}

fn blend(dst: &mut [u8], src: [u8; 3], alpha: u32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = ((s as u32 * alpha + *d as u32 * (255 - alpha) + 127) / 255) as u8;
    }
}

fn paint(c: Rgb) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.0, c.1, c.2, 255);
    p.anti_alias = true;
    p
}

#[allow(clippy::too_many_arguments)]
fn fill_rrect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgb, ts: Transform) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = r.min(w / 2.0).min(h / 2.0);
    if r <= 0.0 {
        // Snap square corners to the pixel grid so edges stay crisp.
        let (x0, y0) = (x.round(), y.round());
        if let Some(rect) = tiny_skia::Rect::from_xywh(x0, y0, w.round().max(1.0), h.round().max(1.0)) {
            pm.fill_rect(rect, &paint(c), ts, None);
        }
        return;
    }
    let k = r * 0.5523; // cubic approximation of a quarter circle
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, &paint(c), FillRule::Winding, ts, None);
    }
}
