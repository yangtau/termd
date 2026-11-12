//! Markdown → laid-out page → pixels.
//!
//! A `Doc` is a page of fixed pixel width: a flat list of positioned draw
//! items (text buffers, rectangles, images). It is built incrementally:
//! `Doc::new` only parses the Markdown, and `Doc::advance` lays out as many
//! blocks as the caller asks for, so the first screen can be shown before the
//! rest of a long document has been shaped. `Doc::render` rasterizes any
//! horizontal strip of what has been laid out so far, which is what lets the
//! viewer upload the page tile by tile.
//!
//! The page image covers only the text column, not the full terminal width,
//! and its background is transparent unless the theme asks for a page color.

use crate::fonts::{Choice, Face, Fonts, Installed};
use crate::highlight::{Highlighter, Token};
use crate::source::{Lines, TextMap};
use crate::theme::{Rgb, Rgba, Theme};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping, Style,
    SwashCache, SwashContent, SwashImage, Weight, Wrap,
};
use image::RgbaImage;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Glyph metadata flags: inline code (gets a background), struck-through
/// text and links (underlined). cosmic-text has its own decorations, but it
/// loses them on runs that use a different font than the paragraph's first.
/// The bits above `META_LINK_SHIFT` hold the link's number plus one.
const META_CODE: usize = 1;
const META_STRIKE: usize = 2;
const META_LINK: usize = 4;
/// The glyph that ends a piece of inline code, with the room for its
/// background in its advance (see `make_buffer`).
const META_CODE_END: usize = 8;
/// Only on a buffer's default attributes, so that no span equals them.
const META_BASE: usize = 16;
const META_LINK_SHIFT: u32 = 8;

const HEADING_SCALE: [f32; 6] = [1.75, 1.35, 1.15, 1.0, 0.9, 0.85];
const BODY_LINE: f32 = 1.65;
const HEADING_LINE: f32 = 1.3;
const CODE_SCALE: f32 = 0.88;
const CODE_LINE: f32 = 1.55;
/// How far the background of inline code reaches beyond its text on each
/// side, in em of the text around it.
const CODE_PAD: f32 = 0.3;
/// Content column width limit, in em.
const MAX_WIDTH_EM: f32 = 46.0;
/// Indent of a list level, in em.
const LIST_INDENT: f32 = 2.1;
const REGULAR: u16 = 400;
/// Headings: set in the body face at a larger size, only slightly heavier.
const MEDIUM: u16 = 500;
/// Strong text and table headers.
const BOLD: u16 = 600;

/// Long-lived rendering state: fonts, glyph cache, highlighted code and
/// decoded images. Reused across re-layouts.
pub struct Engine {
    fs: FontSystem,
    fonts: Fonts,
    swash: SwashCache,
    highlighter: Highlighter,
    images: HashMap<PathBuf, Option<RgbaImage>>,
}

pub struct LayoutOpts {
    /// Page width in pixels (the terminal width, or `--width` for export).
    pub page_w: u32,
    /// Body font size in pixels.
    pub font_px: f32,
    /// Directory relative image paths are resolved against.
    pub base_dir: PathBuf,
    pub theme: Theme,
    /// Whether a table too wide for the page may keep its width and scroll
    /// sideways (the viewer), or has to wrap until it fits (a PNG).
    pub scroll_tables: bool,
}

pub struct Doc {
    /// Width in pixels of the strip of the page that holds the text column;
    /// only tables may reach outside of it.
    pub width: u32,
    /// Where on the page that strip starts.
    pub origin_x: u32,
    /// Height laid out so far; final once `done()`.
    height: u32,
    events: std::vec::IntoIter<(Event<'static>, Range<usize>)>,
    lines: Lines,
    lay: Layout,
    done: bool,
}

/// A reading position that survives a re-layout: a line of text, known by
/// where it comes from in the source.
#[derive(Clone, Copy)]
pub struct Anchor {
    /// Source offset of the first character of the line.
    src: u32,
    /// Position inside the line, as a fraction of its height.
    frac: f32,
}

impl Hit {
    /// The character boundary nearest to the point.
    pub fn pos(&self) -> Pos {
        Pos { item: self.item, offset: self.offset }
    }

    /// The start of the character at the point.
    pub fn character(&self) -> Pos {
        Pos { item: self.item, offset: self.character }
    }
}

impl Anchor {
    /// Source offset of the first character of the anchored line.
    pub fn src(self) -> u32 {
        self.src
    }

    /// The same position inside the line of text at source offset `src`.
    pub fn at(self, src: u32) -> Anchor {
        Anchor { src, ..self }
    }
}

/// A place in the document's text: a character boundary in a text item.
/// Ordered the way the document reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub item: u32,
    pub offset: u32,
}

/// What is at a point of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The text item nearest to the point.
    pub item: u32,
    /// Offset in the item's text of the character boundary nearest to it.
    pub offset: u32,
    /// Offset of the character it is on (or nearest to on its line).
    pub character: u32,
    /// Whether the point is on one of the item's characters, rather than
    /// beside or between its lines.
    pub inside: bool,
    /// The link the character under the point belongs to.
    pub link: Option<u32>,
}

struct Item {
    y: f32,
    h: f32,
    /// 0 for the page itself, else the number (from 1) of the sideways
    /// scrolling table it is part of, which is drawn as an image of its own.
    layer: u16,
    kind: Kind,
}

/// A table on the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableBox {
    /// Its place on the page: left edge, top, the width that shows, height.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Its full width; more than `w` if it scrolls sideways.
    pub full_w: f32,
    /// How far it is scrolled sideways.
    pub scroll: f32,
    /// Height of its header rows (0 without a header), which stay at the
    /// top of the viewport while the rest of the table scrolls through.
    pub head_h: f32,
    /// The layer its items are drawn on: not 0 if it scrolls sideways.
    pub layer: u16,
}

/// What makes a text item part of the document's content (as opposed to
/// decoration such as list numbers and labels).
struct Content {
    map: TextMap,
    /// Offset in the text at which each line of the buffer starts.
    line_starts: Vec<u32>,
    /// What separates this text from the one before it when both are copied.
    join: Join,
    /// What stands for its list marker in copied text ("- ", "2. ").
    prefix: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Join {
    /// A new block: an empty line in between.
    Block,
    /// The next line (list items, table rows).
    Line,
    /// The next cell of a table row.
    Cell,
}

enum Kind {
    Text { x: f32, buf: Buffer, color: Rgba, content: Option<Box<Content>> },
    /// Filled rectangle; `radii` are the corner radii clockwise from top-left.
    Rect { x: f32, w: f32, radii: [f32; 4], color: Rgba },
    /// Rounded rectangle outline drawn inside the box.
    Outline { x: f32, w: f32, radius: f32, line: f32, color: Rgba },
    Image { x: f32, img: RgbaImage },
    Check { x: f32 },
    /// The "copy" symbol in the corner of a code block.
    CopyIcon { x: f32, color: Rgba },
}

/// A code block, for copying it.
struct CodeBlock {
    /// Index of its text in `Layout::items`.
    item: usize,
    /// Its frame on the page: x, y, width, height.
    frame: [f32; 4],
    /// Where a click copies it: the area around its copy symbol.
    button: [f32; 4],
    /// The whole block in the source, fences included.
    src: Range<u32>,
}

#[derive(Clone, Default)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    /// Number of the link this is part of.
    link: Option<u32>,
    color: Option<Rgba>,
    /// Where the text is in the source.
    src: Range<u32>,
}

struct TextStyle {
    size: f32,
    line_h: f32,
    face: Face,
    weight: u16,
}

fn text_style(size: f32, line_scale: f32) -> TextStyle {
    TextStyle { size, line_h: (size * line_scale).round(), face: Face::Body, weight: REGULAR }
}

/// Text style of code blocks.
fn code_style(em: f32) -> TextStyle {
    TextStyle { face: Face::Mono, ..text_style((em * CODE_SCALE).round(), CODE_LINE) }
}

/// Small sans-serif labels: front matter keys, the language of a code block.
fn label_style(em: f32) -> TextStyle {
    TextStyle { face: Face::Ui, ..text_style((em * 0.8).round(), 1.5) }
}

/// Horizontal and vertical padding inside a code block.
fn code_pad(em: f32) -> (f32, f32) {
    ((1.2 * em).round(), (0.95 * em).round())
}

fn code_spans(tokens: &[Token], theme: &Theme) -> Vec<Span> {
    let variant = if theme.dark { 0 } else { 1 };
    tokens
        .iter()
        .map(|t| Span { text: t.text.clone(), color: t.colors[variant].map(Rgb::opaque), ..Span::default() })
        .collect()
}

fn color(c: Rgba) -> Color {
    Color::rgba(c.0, c.1, c.2, c.3)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF      // radicals, kana, CJK punctuation, ideographs
        | 0xAC00..=0xD7AF    // hangul
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0xFE30..=0xFE4F    // CJK compatibility forms
        | 0xFF00..=0xFFEF    // fullwidth forms
        | 0x20000..=0x3FFFF) // ideograph extensions
}

/// Split `text` into maximal runs of CJK and non-CJK characters. Whitespace
/// stays with the run it follows.
fn script_runs(text: &str) -> Vec<(&str, bool)> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            continue;
        }
        let cjk = is_cjk(c);
        match current {
            Some(cur) if cur != cjk => {
                runs.push((&text[start..i], cur));
                start = i;
                current = Some(cjk);
            }
            None => current = Some(cjk),
            _ => {}
        }
    }
    if start < text.len() {
        runs.push((&text[start..], current.unwrap_or(false)));
    }
    runs
}

fn make_buffer(eng: &mut Engine, spans: &[Span], st: &TextStyle, width: Option<f32>) -> Buffer {
    let fonts = &eng.fonts;
    let mut buf = Buffer::new(&mut eng.fs, Metrics::new(st.size, st.line_h));
    buf.set_wrap(Wrap::WordOrGlyph);

    // Every run names its family and a weight that family has, so
    // cosmic-text never needs to go looking for a fallback font.
    let attrs = |cjk: bool, face: Face, weight: u16| {
        let fam = fonts.family(face, cjk);
        Attrs::new().family(Family::Name(&fam.name)).weight(Weight(fam.weight(weight)))
    };
    // cosmic-text does not keep a span whose attributes equal the default
    // ones, and then shapes an ASCII word that starts in another span (the
    // "j /" of "`j` / `k`") entirely in that span's font. With a mark of
    // their own the defaults equal no span.
    let base = attrs(false, st.face, st.weight).metadata(META_BASE);
    let mut rich = Vec::new();
    for (i, s) in spans.iter().enumerate() {
        let weight = if s.bold { BOLD.max(st.weight) } else { st.weight };
        let face = if s.code { Face::Mono } else { st.face };
        let size = if s.code { st.size * CODE_SCALE } else { st.size };
        // The background of inline code is wider than its text. The room
        // for that is letter spacing: after the last character of the
        // code, and after the character before its first one. (A line that
        // starts with code has no such character: see `indent`.)
        let next_is_code = spans.get(i + 1).is_some_and(|next| next.code);
        let room = (s.code || next_is_code).then_some(CODE_PAD * st.size / size);
        let last = s.text.char_indices().next_back().map_or(0, |(at, _)| at);
        let mut at = 0;
        for (piece, cjk) in script_runs(&s.text) {
            let mut a = attrs(cjk, face, weight);
            if s.italic {
                a = a.style(Style::Italic);
            }
            let flag = |on: bool, flag: usize| if on { flag } else { 0 };
            let link = s.link.map_or(0, |id| (id as usize + 1) << META_LINK_SHIFT | META_LINK);
            let meta = flag(s.code, META_CODE) | flag(s.strike, META_STRIKE) | link;
            a = a.metadata(meta);
            if s.code {
                a = a.metrics(Metrics::new(size, st.line_h));
            }
            if let Some(c) = s.color {
                a = a.color(color(c));
            }
            match room {
                Some(room) if at + piece.len() == s.text.len() => {
                    let (head, tail) = piece.split_at(last - at);
                    if !head.is_empty() {
                        rich.push((head, a.clone()));
                    }
                    rich.push((tail, a.letter_spacing(room).metadata(meta | flag(s.code, META_CODE_END))));
                }
                _ => rich.push((piece, a)),
            }
            at += piece.len();
        }
    }
    buf.set_rich_text(rich, &base, Shaping::Advanced, None);
    wrap(&mut eng.fs, &mut buf, width);
    buf
}

/// How far to the right of where it was laid out a line of `buf` is: by
/// the padding of the background of inline code, if it starts with that, so
/// that the background and not the code begins where other lines begin.
/// (Only lines that begin at the left; the others have room there.)
fn indent(buf: &Buffer, run: &LayoutRun) -> f32 {
    let code = run.glyphs.first().is_some_and(|g| g.metadata & META_CODE != 0);
    let left = matches!(buf.lines[run.line_i].align(), None | Some(Align::Left));
    if code && left && !run.rtl { CODE_PAD * buf.metrics().font_size } else { 0.0 }
}

/// Break `buf` into lines no wider than `width`, their `indent` included.
fn wrap(fs: &mut FontSystem, buf: &mut Buffer, width: Option<f32>) {
    buf.set_size(width, None);
    buf.shape_until_scroll(fs, false);
    let Some(width) = width else { return };
    // Nothing says how wide a line may be before it is known how it
    // starts; narrower lines for all of them leave room for any indent.
    let most = buf.layout_runs().map(|run| indent(buf, &run)).fold(0.0, f32::max);
    if buf.layout_runs().any(|run| run.line_w + indent(buf, &run) > width) {
        buf.set_size(Some(width - most), None);
        buf.shape_until_scroll(fs, false);
    }
}

/// What a content text item needs to know about its text: `map` says where
/// it comes from.
fn content(buf: &Buffer, map: TextMap, join: Join, prefix: String) -> Option<Box<Content>> {
    let mut at = 0;
    let line_starts = buf
        .lines
        .iter()
        .map(|line| {
            let start = at;
            at += (line.text().len() + line.ending().as_str().len()) as u32;
            start
        })
        .collect();
    Some(Box::new(Content { map, line_starts, join, prefix }))
}

/// The map of text shaped from `spans`.
fn span_map(spans: &[Span]) -> TextMap {
    TextMap::from_pieces(spans.iter().map(|s| (s.text.len(), s.src.clone())))
}

/// (widest line, total height) of a shaped buffer.
fn measure(buf: &Buffer) -> (f32, f32) {
    buf.layout_runs().fold((0.0, 0.0), |(w, h): (f32, f32), run| {
        (w.max(run.line_w + indent(buf, &run)), h.max(run.line_top + run.line_height))
    })
}

/// Baseline of the first line, from the top of the buffer.
fn baseline(buf: &Buffer) -> Option<f32> {
    buf.layout_runs().next().map(|run| run.line_y)
}

fn is_blank(spans: &[Span]) -> bool {
    spans.iter().all(|s| s.text.trim().is_empty())
}

impl Engine {
    /// `installed` is the result of `fonts::scan`.
    pub fn new(installed: Installed, highlighter: Highlighter, choice: &Choice) -> Engine {
        let (fs, fonts) = Fonts::new(installed, choice);
        Engine { fs, fonts, swash: SwashCache::new(), highlighter, images: HashMap::new() }
    }

    /// Forget decoded images so edited files are picked up on reload.
    pub fn clear_image_cache(&mut self) {
        self.images.clear();
    }

    /// Forget rasterized glyphs; call when the font size changes.
    pub fn clear_glyph_cache(&mut self) {
        self.swash = SwashCache::new();
    }

    /// The families in use, for `TERMD_DEBUG`.
    pub fn font_names(&self) -> String {
        self.fonts.names()
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

/// Layout state carried between `Doc::advance` calls.
/// A code block laid out before its highlighting was ready.
struct Uncolored {
    /// Index of its text in `Layout::items`.
    item: usize,
    lang: String,
    code: String,
    /// Wrap width of its text.
    width: f32,
}

struct Layout {
    theme: Theme,
    base_dir: PathBuf,
    em: f32,
    /// Width of the whole page; tables may use it when the column is too
    /// narrow for them.
    page_w: f32,
    scroll_tables: bool,
    /// Left edge of the current block (moves right inside lists and quotes).
    x: f32,
    right: f32,
    /// Bottom of the last placed block.
    y: f32,
    /// Bottom margin of the last placed block (collapses with the next top margin).
    margin: f32,
    first: bool,
    /// Everything above this is final: no later item will be drawn there.
    stable: f32,
    /// The Markdown being laid out.
    source: String,
    /// Source range of the parser event being handled.
    at: Range<u32>,
    /// How far into the source the text items placed so far reach.
    covered: u32,
    items: Vec<Item>,
    /// Destinations of the links seen so far; text refers to them by index.
    links: Vec<String>,
    codes: Vec<CodeBlock>,
    tables: Vec<TableBox>,
    /// The layer new items go on (see `Item::layer`).
    layer: u16,
    /// Headings as (anchor name, page offset), for links into the document.
    headings: Vec<(String, f32)>,
    /// Code blocks waiting for their colors, in document order.
    uncolored: VecDeque<Uncolored>,

    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    strike: u32,
    /// Number of the open link.
    link: Option<u32>,
    heading: Option<usize>,
    /// Next ordinal for each open list (`None` = bullet list).
    lists: Vec<Option<u64>>,
    /// Marker waiting to be drawn next to the next placed block.
    marker: Option<Marker>,
    /// The marker just drawn, as text, for the text it belongs to.
    prefix: String,
    /// The last text placed was a tight list item, so the next one of the
    /// same kind follows on the next line rather than after an empty one.
    in_tight_list: bool,
    /// Baseline of the first line of the block about to be placed, so a
    /// list number can sit on the same baseline as its text.
    lead: Option<f32>,
    /// Open block quotes: bar x and the y where their content started.
    quotes: Vec<(f32, Option<f32>)>,
    /// Open code block: (language, text), and where its text is in the source.
    code: Option<(String, String)>,
    code_src: Range<u32>,
    html: Option<String>,
    /// Open image: (destination, alt text).
    image: Option<(String, String)>,
    table: Option<Table>,
    /// Open front matter block and its text so far.
    meta: Option<String>,
    /// A soft line break was seen and not yet turned into a space.
    soft_break: bool,
}

/// The parser events of `markdown`, each with its range in the source.
fn parse(markdown: &str) -> Vec<(Event<'static>, Range<usize>)> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    Parser::new_ext(markdown, options)
        .into_offset_iter()
        .map(|(event, range)| (event.into_static(), range))
        .collect()
}

/// Language of a fenced code block's info string.
fn code_lang(kind: &CodeBlockKind) -> String {
    match kind {
        CodeBlockKind::Fenced(info) => {
            info.split(|c: char| c == ',' || c.is_whitespace()).next().unwrap_or("").to_string()
        }
        CodeBlockKind::Indented => String::new(),
    }
}

/// What a highlight of glyph `i` of a line covers, from where to where:
/// inline code with the padding of its background (see `code_boxes`), the
/// text before it without the room that its advance holds for that padding.
fn lit_extent(glyphs: &[LayoutGlyph], i: usize, pad: f32) -> (f32, f32) {
    let code = |g: &LayoutGlyph| g.metadata & META_CODE != 0;
    let last = |g: &LayoutGlyph| g.metadata & META_CODE_END != 0;
    let (g, before, after) = (&glyphs[i], i.checked_sub(1).map(|i| &glyphs[i]), glyphs.get(i + 1));
    if code(g) {
        let opens = before.is_none_or(|b| !code(b) || last(b));
        // Only code that goes on in the next line ends without its room.
        let cut = !last(g) && after.is_none_or(|a| !code(a));
        (g.x - if opens { pad } else { 0.0 }, g.x + g.w + if cut { pad } else { 0.0 })
    } else {
        (g.x, g.x + g.w - if after.is_some_and(code) { pad } else { 0.0 })
    }
}

/// The text of a code block as it is displayed and highlighted.
fn code_text(raw: &str) -> &str {
    raw.strip_suffix('\n').unwrap_or(raw)
}

/// The (language, code) of every fenced code block, in document order.
fn code_blocks(events: &[(Event, Range<usize>)]) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(String, String)> = None;
    for (event, _) in events {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => open = Some((code_lang(kind), String::new())),
            Event::Text(t) => {
                if let Some((_, code)) = &mut open {
                    code.push_str(t);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, code)) = open.take() {
                    blocks.push((lang, code_text(&code).to_string()));
                }
            }
            _ => {}
        }
    }
    blocks
}

/// Start highlighting the code blocks of `markdown` in the background, so
/// the work overlaps with the rest of startup.
pub fn prepare_highlighting(highlighter: &mut Highlighter, markdown: &str) {
    highlighter.prepare(code_blocks(&parse(markdown)));
}

impl Doc {
    /// Parse `markdown`. Nothing is laid out until `advance` is called.
    pub fn new(eng: &mut Engine, markdown: &str, opts: LayoutOpts) -> Doc {
        eng.fonts.cover(&mut eng.fs, markdown);

        let em = opts.font_px;
        let page_w = opts.page_w as f32;
        let content_w = (page_w - 2.4 * em).min(MAX_WIDTH_EM * em).max(4.0 * em).round();
        // Room for things that hang slightly outside the column, such as the
        // padding of inline code at the start of a line. A page with its own
        // background color gets a proper margin instead.
        let bleed = (if opts.theme.page.is_some() { 1.2 } else { 0.5 } * em).ceil();
        let width = (content_w + 2.0 * bleed).min(page_w.max(1.0));
        let origin_x = ((page_w - width) / 2.0).max(0.0).floor();

        let events = parse(markdown);
        eng.highlighter.prepare(code_blocks(&events));

        let top = (1.5 * em).round();
        Doc {
            width: width as u32,
            origin_x: origin_x as u32,
            height: 0,
            events: events.into_iter(),
            lines: Lines::new(markdown),
            done: false,
            lay: Layout {
                theme: opts.theme,
                base_dir: opts.base_dir,
                em,
                page_w,
                scroll_tables: opts.scroll_tables,
                x: origin_x + bleed,
                right: origin_x + bleed + content_w,
                y: top,
                margin: 0.0,
                first: true,
                stable: 0.0,
                source: markdown.to_string(),
                at: 0..0,
                covered: 0,
                items: Vec::new(),
                links: Vec::new(),
                codes: Vec::new(),
                tables: Vec::new(),
                layer: 0,
                headings: Vec::new(),
                uncolored: VecDeque::new(),
                spans: Vec::new(),
                bold: 0,
                italic: 0,
                strike: 0,
                link: None,
                heading: None,
                lists: Vec::new(),
                marker: None,
                prefix: String::new(),
                in_tight_list: false,
                lead: None,
                quotes: Vec::new(),
                code: None,
                code_src: 0..0,
                html: None,
                image: None,
                table: None,
                meta: None,
                soft_break: false,
            },
        }
    }

    /// Lay out more of the document: until `until` pixels are final, until
    /// `deadline` passes, or to the end if neither is given.
    pub fn advance(&mut self, eng: &mut Engine, until: Option<u32>, deadline: Option<Instant>) {
        while !self.done {
            if until.is_some_and(|y| self.height >= y) || deadline.is_some_and(|d| Instant::now() >= d) {
                return;
            }
            match self.events.next() {
                Some((event, range)) => {
                    self.lay.at = range.start as u32..range.end as u32;
                    self.lay.event(eng, event);
                    self.height = self.lay.stable.floor() as u32;
                }
                None => {
                    self.lay.flush_inline(eng, false);
                    self.height = (self.lay.y + 2.0 * self.lay.em).ceil() as u32;
                    self.done = true;
                }
            }
        }
    }

    /// Whether some laid-out code block is still shown without colors.
    pub fn awaits_highlight(&self) -> bool {
        !self.lay.uncolored.is_empty()
    }

    /// Color the code blocks whose tokens have arrived since they were laid
    /// out, working until `deadline`; without one, wait for all of them.
    /// Returns the page ranges `(top, bottom)` that changed and must be
    /// rendered again.
    pub fn highlight(&mut self, eng: &mut Engine, deadline: Option<Instant>) -> Vec<(u32, u32)> {
        let lay = &mut self.lay;
        let mut changed = Vec::new();
        // Blocks are highlighted in document order, which is also the order
        // of this queue, so the first one that is not ready ends the pass.
        while let Some(block) = lay.uncolored.front() {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            let tokens = match deadline {
                None => eng.highlighter.get(&block.lang, &block.code),
                Some(_) => match eng.highlighter.try_get(&block.lang, &block.code) {
                    Some(tokens) => tokens,
                    None => break,
                },
            };
            let Some(block) = lay.uncolored.pop_front() else { break };
            let Some(tokens) = tokens else { continue }; // unknown language: stays plain
            let colored = make_buffer(eng, &code_spans(&tokens, &lay.theme), &code_style(lay.em), Some(block.width));
            let item = &mut lay.items[block.item];
            // Colors must not move anything; if they somehow would (the
            // text wraps differently), the block stays as it is.
            if let Kind::Text { buf, .. } = &mut item.kind
                && measure(&colored).1 == measure(buf).1
            {
                *buf = colored;
                changed.push((item.y.floor() as u32, (item.y + item.h).ceil() as u32));
            }
        }
        changed
    }

    /// Height that can be rendered: everything laid out so far.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Whether the whole document has been laid out.
    pub fn done(&self) -> bool {
        self.done
    }

    /// The text items that are document content, with their index.
    fn texts(&self) -> impl Iterator<Item = (usize, &Item, f32, &Buffer, &Content)> {
        self.lay.items.iter().enumerate().filter_map(|(i, item)| match &item.kind {
            Kind::Text { x, buf, content: Some(content), .. } => Some((i, item, *x, buf, &**content)),
            _ => None,
        })
    }

    /// The topmost line of text that ends below page offset `y`: its top,
    /// its height and the source offset of its first character.
    fn line_below(&self, y: f32) -> Option<(f32, f32, u32)> {
        let mut best: Option<(f32, f32, u32)> = None;
        for (_, item, _, buf, content) in self.texts() {
            if item.y + item.h <= y || best.is_some_and(|(top, ..)| item.y >= top) {
                continue;
            }
            for run in buf.layout_runs() {
                let top = item.y + run.line_top;
                if top + run.line_height > y {
                    if best.is_none_or(|(best_top, ..)| top < best_top) {
                        let first = run.glyphs.iter().map(|g| g.start).min().unwrap_or(0) as u32;
                        let offset = content.line_starts.get(run.line_i).copied().unwrap_or(0) + first;
                        best = Some((top, run.line_height, content.map.to_source(offset)));
                    }
                    break;
                }
            }
        }
        best
    }

    /// 1-based source line of the first text at or below page offset `y`.
    pub fn source_line(&self, y: u32) -> Option<u32> {
        self.line_below(y as f32).map(|(_, _, src)| self.lines.line_of(src))
    }

    /// The line of text at page offset `y`, to find the position again
    /// after a re-layout.
    pub fn anchor(&self, y: u32) -> Option<Anchor> {
        let (top, height, src) = self.line_below(y as f32)?;
        // Above the line (in a margin): keep the distance, within reason.
        Some(Anchor { src, frac: ((y as f32 - top) / height.max(1.0)).clamp(-4.0, 1.0) })
    }

    /// Page offset of `anchor` in this layout, laying out as far as needed.
    pub fn resolve(&mut self, eng: &mut Engine, anchor: Anchor) -> u32 {
        while !self.done && self.lay.covered <= anchor.src {
            let more = self.height + 1;
            self.advance(eng, Some(more), None);
        }
        // The text the offset is in, or else the first text after it.
        let found = self
            .texts()
            .filter(|(_, _, _, _, content)| content.map.range().end > anchor.src)
            .min_by_key(|(_, _, _, _, content)| content.map.range().start);
        let Some((_, item, _, buf, content)) = found else { return self.height };
        let offset = content.map.to_text(anchor.src.max(content.map.range().start));
        let line = content.line_starts.partition_point(|&start| start <= offset).saturating_sub(1);
        let in_line = offset - content.line_starts[line];
        let run = buf
            .layout_runs()
            .filter(|run| run.line_i == line)
            .find(|run| run.glyphs.iter().any(|g| g.end as u32 > in_line))
            .or_else(|| buf.layout_runs().filter(|run| run.line_i <= line).last());
        let (top, height) = run.map_or((0.0, 0.0), |run| (run.line_top, run.line_height));
        (item.y + top + anchor.frac * height).max(0.0) as u32
    }

    /// Page offset of the text that comes from 1-based source `line` (or the
    /// first text after it).
    pub fn offset_of_line(&mut self, eng: &mut Engine, line: u32) -> u32 {
        self.resolve(eng, Anchor { src: self.start_of_line(line), frac: 0.0 })
    }

    /// Source offset at which 1-based `line` starts.
    pub fn start_of_line(&self, line: u32) -> u32 {
        self.lines.start_of(line)
    }

    /// The tables laid out so far.
    pub fn tables(&self) -> &[TableBox] {
        &self.lay.tables
    }

    /// Scroll table number `table` sideways by `by` pixels, as far as it
    /// goes. Returns whether it moved.
    pub fn scroll_table(&mut self, table: usize, by: f32) -> bool {
        let Some(t) = self.lay.tables.get_mut(table) else { return false };
        let to = (t.scroll + by).clamp(0.0, (t.full_w - t.w).max(0.0)).round();
        let moved = to != t.scroll;
        t.scroll = to;
        moved
    }

    /// The strip of the page that has something to draw between page
    /// offsets `top` and `bottom`, as (left edge, width): the text column,
    /// widened by tables that reach out of it.
    pub fn extent(&self, top: u32, bottom: u32) -> (u32, u32) {
        let (mut left, mut right) = (self.origin_x as f32, (self.origin_x + self.width) as f32);
        for t in self.lay.tables.iter().filter(|t| t.layer == 0) {
            if t.y < bottom as f32 && t.y + t.h > top as f32 {
                (left, right) = (left.min(t.x.floor()), right.max((t.x + t.w).ceil()));
            }
        }
        let left = left.max(0.0);
        (left as u32, (right.min(self.lay.page_w) - left).max(1.0) as u32)
    }

    /// Where page coordinate `x` is for an item on `layer`: a sideways
    /// scrolled table shows a later part of itself. The flag tells whether
    /// `x` is in the part of the page where that layer shows at all.
    fn local_x(&self, layer: u16, x: f32) -> (f32, bool) {
        match layer.checked_sub(1).and_then(|i| self.lay.tables.get(i as usize)) {
            Some(t) => (x.clamp(t.x, t.x + t.w) + t.scroll, x >= t.x && x < t.x + t.w),
            None => (x, true),
        }
    }

    /// What is at the point (`x`, `y`) of the page.
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        // The nearest text, vertically first: text beside the point on its
        // line wins over text right above or below it.
        let distance = |lo: f32, hi: f32, v: f32| (lo - v).max(v - hi).max(0.0);
        let (index, item, item_x, buf, content) = self.texts().min_by(|a, b| {
            let key = |(_, item, left, buf, _): &(usize, &Item, f32, &Buffer, &Content)| {
                let width = buf.size().0.unwrap_or(0.0);
                let x = self.local_x(item.layer, x).0;
                (distance(item.y, item.y + item.h, y), distance(*left, left + width, x))
            };
            let (ka, kb) = (key(a), key(b));
            ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
        })?;
        let (x, in_view) = self.local_x(item.layer, x);
        let (lx, ly) = (x - item_x, y - item.y);
        // The line at that height, or the nearest one.
        let run = buf
            .layout_runs()
            .min_by(|a, b| {
                let d = |r: &cosmic_text::LayoutRun| distance(r.line_top, r.line_top + r.line_height, ly);
                d(a).total_cmp(&d(b))
            })?;
        let on_line = ly >= run.line_top && ly < run.line_top + run.line_height;
        let lx = lx - indent(buf, &run);
        let line_start = content.line_starts.get(run.line_i).copied().unwrap_or(0);
        let under = run.glyphs.iter().find(|g| lx >= g.x && lx < g.x + g.w);
        let (in_line, character, link) = match under {
            Some(g) => {
                let link = (g.metadata >> META_LINK_SHIFT).checked_sub(1).map(|id| id as u32);
                (if lx < g.x + g.w / 2.0 { g.start } else { g.end }, g.start, link)
            }
            // Beside the line: its nearer end.
            None => {
                let before = run.glyphs.first().is_none_or(|g| lx < g.x);
                let (boundary, character) = match before {
                    true => (run.glyphs.iter().map(|g| g.start).min(), run.glyphs.iter().map(|g| g.start).min()),
                    false => (run.glyphs.iter().map(|g| g.end).max(), run.glyphs.iter().map(|g| g.start).max()),
                };
                (boundary.unwrap_or(0), character.unwrap_or(0), None)
            }
        };
        Some(Hit {
            item: index as u32,
            offset: line_start + in_line as u32,
            character: line_start + character as u32,
            inside: on_line && in_view && under.is_some(),
            link,
        })
    }

    /// The highlight for the text between `from` and `to`, as rectangles
    /// (x, y, width, height) on the page, as far as they show between page
    /// offsets `top` and `bottom`, each with the layer its text is on.
    pub fn selection_rects(&self, from: Pos, to: Pos, top: f32, bottom: f32) -> Vec<(u16, [f32; 4])> {
        let mut rects = Vec::new();
        for (index, item, x, buf, content) in self.texts() {
            let index = index as u32;
            if index < from.item || index > to.item || item.y >= bottom || item.y + item.h <= top {
                continue;
            }
            let start = if index == from.item { from.offset } else { 0 };
            let end = if index == to.item { to.offset } else { u32::MAX };
            for run in buf.layout_runs() {
                let line_top = item.y + run.line_top;
                if line_top >= bottom || line_top + run.line_height <= top {
                    continue;
                }
                let line_start = content.line_starts.get(run.line_i).copied().unwrap_or(0);
                let glyphs = run.glyphs;
                let selected = (0..glyphs.len())
                    .filter(|&i| line_start + (glyphs[i].start as u32) < end && line_start + glyphs[i].end as u32 > start);
                let pad = CODE_PAD * buf.metrics().font_size;
                let extent = selected.fold(None, |extent: Option<(f32, f32)>, i| {
                    let (g_lo, g_hi) = lit_extent(glyphs, i, pad);
                    let (lo, hi) = extent.unwrap_or((g_lo, g_hi));
                    Some((lo.min(g_lo), hi.max(g_hi)))
                });
                let (lo, hi) = match extent {
                    Some((lo, hi)) => (x + indent(buf, &run) + lo, x + indent(buf, &run) + hi),
                    // An empty line inside the selection still shows.
                    None if run.glyphs.is_empty() && line_start >= start && line_start < end => {
                        (x, x + (0.4 * self.lay.em).round())
                    }
                    None => continue,
                };
                // What a sideways scrolled table shows of it.
                let (lo, hi) = match item.layer.checked_sub(1).and_then(|i| self.lay.tables.get(i as usize)) {
                    Some(t) => ((lo - t.scroll).max(t.x), (hi - t.scroll).min(t.x + t.w)),
                    None => (lo, hi),
                };
                if lo < hi {
                    rects.push((item.layer, [lo, line_top, hi - lo, run.line_height]));
                }
            }
        }
        rects
    }

    /// The text between `from` and `to` as plain text: blocks separated by
    /// empty lines, list items with their markers, table cells by tabs.
    pub fn selected_text(&self, from: Pos, to: Pos) -> String {
        let mut out = String::new();
        for (index, _, _, buf, content) in self.texts() {
            let index = index as u32;
            if index < from.item || index > to.item {
                continue;
            }
            let text = buffer_text(buf);
            let clamp = |offset: u32| {
                let mut at = (offset as usize).min(text.len());
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                at
            };
            let start = if index == from.item { clamp(from.offset) } else { 0 };
            let end = if index == to.item { clamp(to.offset) } else { text.len() };
            if !out.is_empty() {
                out.push_str(match content.join {
                    Join::Block => "\n\n",
                    Join::Line => "\n",
                    Join::Cell => "\t",
                });
            }
            // The marker belongs to the start of the item.
            if start == 0 && end > 0 {
                out.push_str(&content.prefix);
            }
            out.push_str(&text[start..end.max(start)]);
        }
        out
    }

    /// The Markdown behind the text between `from` and `to`. Within one
    /// source line that is exactly the stretch between the two characters;
    /// across lines it is the whole lines, so that list markers, fences and
    /// table rows come along intact.
    pub fn selected_source(&self, from: Pos, to: Pos) -> String {
        let src = |pos: Pos| match &self.lay.items.get(pos.item as usize)?.kind {
            Kind::Text { content: Some(content), .. } => Some(content.map.to_source(pos.offset)),
            _ => None,
        };
        let (Some(start), Some(end)) = (src(from), src(to)) else { return String::new() };
        let range = start..end.max(start);
        let range = match self.lines.line_of(range.start) == self.lines.line_of(range.end.saturating_sub(1)) {
            true => range,
            false => self.lines.whole_lines(range),
        };
        self.lay.source.get(range.start as usize..range.end as usize).unwrap_or("").to_string()
    }

    /// The word that the character at `pos` is part of, for selecting by
    /// double click: a run of letters and digits, or a single CJK or other
    /// character.
    pub fn word_at(&self, pos: Pos) -> (Pos, Pos) {
        let Some(Kind::Text { buf, .. }) = self.lay.items.get(pos.item as usize).map(|item| &item.kind) else {
            return (pos, pos);
        };
        let text = buffer_text(buf);
        let at = (pos.offset as usize).min(text.len());
        let is_word = |c: char| (c.is_alphanumeric() || c == '_') && !is_cjk(c);
        let Some(first) = text.get(at..).and_then(|rest| rest.chars().next()) else { return (pos, pos) };
        let start = at;
        let (mut lo, mut hi) = (start, start + first.len_utf8());
        if is_word(first) {
            lo = text[..start].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(start, |(i, _)| i);
            hi += text[hi..].chars().take_while(|c| is_word(*c)).map(char::len_utf8).sum::<usize>();
        }
        (Pos { item: pos.item, offset: lo as u32 }, Pos { item: pos.item, offset: hi as u32 })
    }

    /// The whole text item `pos` is in, for selecting by triple click.
    pub fn block_at(&self, pos: Pos) -> (Pos, Pos) {
        let len = match self.lay.items.get(pos.item as usize).map(|item| &item.kind) {
            Some(Kind::Text { buf, .. }) => buffer_text(buf).len() as u32,
            _ => 0,
        };
        (Pos { item: pos.item, offset: 0 }, Pos { item: pos.item, offset: len })
    }

    /// Destination of link number `id`.
    pub fn link(&self, id: u32) -> Option<&str> {
        self.lay.links.get(id as usize).map(String::as_str)
    }

    /// The code block whose copy symbol is at the point (`x`, `y`).
    pub fn copy_button(&self, x: f32, y: f32) -> Option<usize> {
        let inside = |[bx, by, bw, bh]: [f32; 4]| x >= bx && x < bx + bw && y >= by && y < by + bh;
        self.lay.codes.iter().position(|code| inside(code.button))
    }

    /// The code blocks that show between page offsets `top` and `bottom`,
    /// those that start in that range first.
    pub fn code_blocks(&self, top: f32, bottom: f32) -> Vec<usize> {
        let mut found: Vec<usize> = (0..self.lay.codes.len())
            .filter(|&i| {
                let [_, y, _, h] = self.lay.codes[i].frame;
                y < bottom && y + h > top
            })
            .collect();
        found.sort_by_key(|&i| self.lay.codes[i].frame[1] < top);
        found
    }

    /// Frame of code block `code` on the page: x, y, width, height.
    #[cfg(test)]
    pub fn code_frame(&self, code: usize) -> [f32; 4] {
        self.lay.codes[code].frame
    }

    /// Where the text of code block `code` starts and ends.
    pub fn code_span(&self, code: usize) -> (Pos, Pos) {
        let item = self.lay.codes[code].item as u32;
        (Pos { item, offset: 0 }, Pos { item, offset: u32::MAX })
    }

    /// The text of code block `code`, or with `fenced` the block as it is
    /// written in the source.
    pub fn code_text(&self, code: usize, fenced: bool) -> String {
        let block = &self.lay.codes[code];
        match &self.lay.items[block.item].kind {
            Kind::Text { buf, .. } if !fenced => buffer_text(buf),
            _ => self.lay.source[block.src.start as usize..block.src.end as usize].trim_end().to_string(),
        }
    }

    /// Page offset of the heading a link to `#fragment` means, laying out
    /// the rest of the document if it is not among the headings so far.
    pub fn heading(&mut self, eng: &mut Engine, fragment: &str) -> Option<u32> {
        let wanted = fragment.to_lowercase();
        let find = |doc: &Doc| doc.lay.headings.iter().find(|(name, _)| *name == wanted).map(|(_, y)| *y as u32);
        find(self).or_else(|| {
            self.advance(eng, None, None);
            find(self)
        })
    }

    /// 1-based source line of the character at `offset` of text item `item`.
    #[cfg(test)]
    pub fn line_of(&self, hit: &Hit) -> Option<u32> {
        match &self.lay.items.get(hit.item as usize)?.kind {
            Kind::Text { content: Some(content), .. } => Some(self.lines.line_of(content.map.to_source(hit.offset))),
            _ => None,
        }
    }
}

impl Layout {
    fn event(&mut self, eng: &mut Engine, event: Event) {
        match event {
            Event::Start(tag) => self.start(eng, tag),
            Event::End(tag) => self.end(eng, tag),
            Event::Text(t) => self.text(&t, false),
            Event::Code(t) => self.text(&t, true),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, true),
            Event::SoftBreak => self.soft_break = true,
            Event::HardBreak => self.text("\n", false),
            Event::Html(t) => match &mut self.html {
                Some(html) => html.push_str(&t),
                None => self.html_block(eng, &t),
            },
            Event::InlineHtml(t) => {
                if t.to_ascii_lowercase().starts_with("<br") {
                    self.text("\n", false);
                }
            }
            Event::Rule => {
                self.flush_inline(eng, true);
                let h = self.hairline();
                let y = self.place(eng, h, 1.6 * self.em, 1.6 * self.em, h);
                self.rect(self.x, y, self.right - self.x, h, 0.0, self.theme.border);
            }
            Event::TaskListMarker(checked) => self.marker = Some(Marker::Task(checked)),
            Event::FootnoteReference(name) => self.text(&format!("[{name}]"), false),
        }
        // Only a block quote draws above the current position after the
        // fact (its bar), so outside of quotes everything placed is final.
        if self.quotes.is_empty() {
            self.stable = self.y;
        }
    }

    /// Thickness of rules and borders: one pixel, two on HiDPI screens.
    fn hairline(&self) -> f32 {
        (self.em / 14.0).round().max(1.0)
    }

    fn start(&mut self, eng: &mut Engine, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush_inline(eng, true),
            Tag::Heading { level, .. } => {
                self.flush_inline(eng, true);
                self.heading = Some(level as usize);
            }
            Tag::BlockQuote(_) => {
                self.flush_inline(eng, true);
                self.quotes.push((self.x, None));
                self.x += 1.25 * self.em;
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline(eng, true);
                self.code = Some((code_lang(&kind), String::new()));
            }
            Tag::HtmlBlock => {
                self.flush_inline(eng, true);
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.flush_inline(eng, true);
                // A list starts a new block; a nested one continues its item.
                self.in_tight_list &= !self.lists.is_empty();
                self.lists.push(start);
                self.x += LIST_INDENT * self.em;
            }
            Tag::Item => {
                self.flush_inline(eng, true);
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        Marker::Number(*n - 1)
                    }
                    _ => Marker::Bullet,
                });
            }
            Tag::Table(aligns) => {
                self.flush_inline(eng, true);
                self.table = Some(Table { aligns, rows: Vec::new(), row: Vec::new(), head_rows: 0 });
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some(self.links.len() as u32);
                self.links.push(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            Tag::MetadataBlock(_) => self.meta = Some(String::new()),
            _ => {}
        }
    }

    fn end(&mut self, eng: &mut Engine, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(eng, false),
            TagEnd::Heading(_) => {
                self.flush_inline(eng, false);
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline(eng, true);
                self.x -= 1.25 * self.em;
                if let Some((bar_x, Some(top))) = self.quotes.pop() {
                    let w = (0.17 * self.em).max(2.0).round();
                    self.rect(bar_x, top, w, self.y - top, w / 2.0, self.theme.quote_bar);
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    self.code_block(eng, &lang, &text);
                }
            }
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.html_block(eng, &html);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline(eng, true);
                self.lists.pop();
                self.x -= LIST_INDENT * self.em;
                if self.lists.is_empty() {
                    self.margin = self.margin.max(self.em);
                }
            }
            TagEnd::Item => {
                self.flush_inline(eng, true);
                // An item with no content still gets its marker.
                if self.marker.is_some() {
                    let line = (self.em * BODY_LINE).round();
                    self.place(eng, line, 0.32 * self.em, 0.32 * self.em, line);
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
                    self.table_block(eng, t);
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = None,
            TagEnd::Image => {
                if let Some((dest, alt)) = self.image.take() {
                    self.image_block(eng, &dest, &alt);
                }
            }
            TagEnd::MetadataBlock(_) => {
                if let Some(meta) = self.meta.take() {
                    self.front_matter(eng, &meta);
                }
            }
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
        if let Some(meta) = &mut self.meta {
            meta.push_str(t);
        } else if let Some((_, buf)) = &mut self.code {
            // The block's text is the source from its first to its last line.
            if buf.is_empty() {
                self.code_src.start = self.at.start;
            }
            self.code_src.end = self.at.end;
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
                link: self.link,
                // Struck-through text steps back, like the reference
                // renderers do it.
                color: match (self.link.is_some(), self.strike > 0) {
                    (true, _) => Some(self.theme.link),
                    (false, true) => Some(self.theme.dim),
                    _ => None,
                },
                src: self.at.clone(),
            });
        }
    }

    fn text_color(&self) -> Rgba {
        if self.quotes.is_empty() { self.theme.fg } else { self.theme.quote }
    }

    /// Reserve vertical space for a block of height `h` and return its top.
    /// `first_line_h` positions a pending list marker next to the first line.
    fn place(&mut self, eng: &mut Engine, h: f32, margin_top: f32, margin_bottom: f32, first_line_h: f32) -> f32 {
        let gap = if self.first { 0.0 } else { self.margin.max(margin_top) };
        let y = (self.y + gap).round();
        self.first = false;
        self.y = y + h;
        self.margin = margin_bottom;
        for quote in &mut self.quotes {
            quote.1.get_or_insert(y);
        }
        let lead = self.lead.take();
        if let Some(marker) = self.marker.take() {
            self.draw_marker(eng, marker, y, first_line_h, lead);
        }
        y
    }

    /// Add a shape or a piece of decoration.
    fn push(&mut self, y: f32, h: f32, kind: Kind) {
        self.items.push(Item { y, h, layer: self.layer, kind });
    }

    /// Add text that is document content; `map` says where it comes from.
    #[allow(clippy::too_many_arguments)]
    fn push_text(&mut self, x: f32, y: f32, h: f32, buf: Buffer, color: Rgba, map: TextMap, join: Join) {
        self.covered = self.covered.max(map.range().end);
        let content = content(&buf, map, join, std::mem::take(&mut self.prefix));
        self.items.push(Item { y, h, layer: self.layer, kind: Kind::Text { x, buf, color, content } });
    }

    /// Add text that is decoration: a list number, a label.
    fn push_label(&mut self, x: f32, y: f32, h: f32, buf: Buffer, color: Rgba) {
        self.push(y, h, Kind::Text { x, buf, color, content: None });
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgba) {
        self.push(y, h, Kind::Rect { x, w, radii: [radius; 4], color });
    }

    fn draw_marker(&mut self, eng: &mut Engine, marker: Marker, y: f32, line_h: f32, lead: Option<f32>) {
        let em = self.em;
        let color = self.text_color();
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        self.prefix = match marker {
            Marker::Bullet => format!("{indent}- "),
            Marker::Number(n) => format!("{indent}{n}. "),
            Marker::Task(checked) => format!("{indent}[{}] ", if checked { 'x' } else { ' ' }),
        };
        match marker {
            // Disc, ring, square by nesting depth, like a browser.
            Marker::Bullet => {
                let d = (0.3 * em).round().max(3.0);
                let (x, y) = ((self.x - 1.15 * em).round(), y + ((line_h - d) / 2.0).round());
                match self.lists.len() {
                    0 | 1 => self.rect(x, y, d, d, d / 2.0, color),
                    2 => {
                        let line = (d * 0.2).max(1.0);
                        self.push(y, d, Kind::Outline { x, w: d, radius: d / 2.0, line, color });
                    }
                    _ => self.rect(x, y, d, d, 0.0, color),
                }
            }
            Marker::Number(n) => {
                let st = text_style(em, BODY_LINE);
                let span = Span { text: format!("{n}."), ..Span::default() };
                let buf = make_buffer(eng, &[span], &st, None);
                let (w, h) = measure(&buf);
                let x = self.x - 0.6 * em - w;
                // Lines with CJK glyphs have a slightly lower baseline than
                // Latin-only ones; follow the text's.
                let shift = lead.zip(baseline(&buf)).map_or(0.0, |(text, number)| (text - number).round());
                self.push_label(x, y + shift, h, buf, color);
            }
            // A quiet gray box: it reports a state, it is not a control.
            Marker::Task(checked) => {
                let s = (0.9 * em).round();
                let (x, y) = ((self.x - 1.5 * em).round(), y + ((line_h - s) / 2.0).round());
                if checked {
                    self.rect(x, y, s, s, s * 0.22, self.theme.check);
                    self.push(y, s, Kind::Check { x });
                } else {
                    let (line, color) = ((s * 0.08).max(1.0).round(), self.theme.check);
                    self.push(y, s, Kind::Outline { x, w: s, radius: s * 0.22, line, color });
                }
            }
        }
    }

    /// Turn the accumulated inline spans into a paragraph or heading.
    /// `tight` is used for text that is not wrapped in a paragraph (tight
    /// list items), which gets smaller margins.
    fn flush_inline(&mut self, eng: &mut Engine, tight: bool) {
        if self.table.is_some() {
            return; // table cells collect their own spans
        }
        let spans = std::mem::take(&mut self.spans);
        if is_blank(&spans) {
            return;
        }
        let em = self.em;
        let (st, mt, mb) = match self.heading {
            // Headings stand out by size and the space above them, not by
            // weight or rules; they sit close to the text they introduce.
            Some(level) => {
                let level = level.clamp(1, 6);
                let mut st = text_style((em * HEADING_SCALE[level - 1]).round(), HEADING_LINE);
                st.weight = if level <= 3 { MEDIUM } else { BOLD };
                let top = if level <= 2 { 2.2 } else { 1.7 };
                (st, top * em, 0.55 * em)
            }
            None if tight => (text_style(em, BODY_LINE), 0.32 * em, 0.32 * em),
            None => (text_style(em, BODY_LINE), em, em),
        };
        let buf = make_buffer(eng, &spans, &st, Some(self.right - self.x));
        let (_, text_h) = measure(&buf);
        self.lead = baseline(&buf);
        let y = self.place(eng, text_h, mt, mb, st.line_h);
        if self.heading.is_some() {
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            let name = heading_anchor(&text, self.headings.iter().map(|(name, _)| name.as_str()));
            self.headings.push((name, y));
        }

        self.code_boxes(&buf, self.x, y);
        let color = if self.heading == Some(6) { self.theme.dim } else { self.text_color() };
        let join = if tight && self.in_tight_list { Join::Line } else { Join::Block };
        self.in_tight_list = tight && self.heading.is_none() && !self.lists.is_empty();
        self.push_text(self.x, y, text_h, buf, color, span_map(&spans), join);
    }

    /// YAML front matter, shown as a quiet key/value header above the
    /// document.
    fn front_matter(&mut self, eng: &mut Engine, text: &str) {
        let rows = front_matter_rows(text);
        if rows.is_empty() {
            return;
        }

        let em = self.em;
        let key_st = label_style(em);
        let value_st = TextStyle { face: Face::Ui, ..text_style((em * 0.95).round(), 1.5) };
        let keys: Vec<Buffer> = rows
            .iter()
            .map(|(key, _)| make_buffer(eng, &[Span { text: key.clone(), ..Span::default() }], &key_st, None))
            .collect();
        let widest = keys.iter().map(|k| measure(k).0).fold(0.0, f32::max);
        let key_col = (widest + 2.2 * em).max(4.5 * em).min((self.right - self.x) / 2.0).round();
        for ((_, value), key) in rows.into_iter().zip(keys) {
            // Each value is found again in the block by its text, which is
            // good enough to tell which line it is on.
            let block = &self.source[self.at.start as usize..self.at.end as usize];
            let start = self.at.start + block.find(value.lines().next().unwrap_or("")).unwrap_or(0) as u32;
            let map = TextMap::from_lines(&value, &self.source, start..self.at.end);
            let span = Span { text: value, ..Span::default() };
            let value = make_buffer(eng, &[span], &value_st, Some(self.right - self.x - key_col));
            let h = measure(&value).1.max(value_st.line_h);
            let y = self.place(eng, h, 0.3 * em, 0.3 * em, value_st.line_h);
            let shift = baseline(&value).zip(baseline(&key)).map_or(0.0, |(v, k)| (v - k).round());
            self.push_label(self.x, y + shift, key_st.line_h, key, self.theme.dim);
            self.push_text(self.x + key_col, y, h, value, self.theme.fg, map, Join::Line);
        }
        let line = self.hairline();
        let y = self.place(eng, line, 1.1 * em, 1.2 * em, line);
        self.rect(self.x, y, self.right - self.x, line, 0.0, self.theme.border);
    }

    /// Backgrounds behind the inline code of `buf`, whose top-left is at
    /// (`x`, `y`). Pushed before the text so they end up underneath.
    fn code_boxes(&mut self, buf: &Buffer, x: f32, y: f32) {
        let size = buf.metrics().font_size;
        let code_h = (size * 1.38).round();
        let pad = CODE_PAD * size;
        let mut boxes: Vec<(f32, f32, f32)> = Vec::new(); // x0, x1, top
        for run in buf.layout_runs() {
            let top = run.line_top + ((run.line_height - code_h) / 2.0).round();
            let dx = indent(buf, &run);
            let mut open: Option<(f32, f32)> = None;
            for g in run.glyphs {
                if g.metadata & META_CODE != 0 {
                    let gx = dx + g.x;
                    let (x0, _) = open.unwrap_or((gx - pad, 0.0));
                    // The last glyph has the room after it in its advance;
                    // code that goes on in the next line ends without it.
                    let room = if g.metadata & META_CODE_END != 0 { 0.0 } else { pad };
                    open = Some((x0.min(gx - pad), gx + g.w + room));
                    if room == 0.0 {
                        boxes.extend(open.take().map(|(x0, x1)| (x0, x1, top)));
                    }
                } else if let Some((x0, x1)) = open.take() {
                    boxes.push((x0, x1, top));
                }
            }
            if let Some((x0, x1)) = open {
                boxes.push((x0, x1, top));
            }
        }
        for (x0, x1, top) in boxes {
            let (left, right) = ((x + x0).round(), (x + x1).round());
            self.rect(left, y + top, right - left, code_h, size * 0.26, self.theme.code_bg);
        }
    }

    fn code_block(&mut self, eng: &mut Engine, lang: &str, text: &str) {
        let text = code_text(text);
        let plain = || vec![Span { text: text.to_string(), ..Span::default() }];
        let src = std::mem::replace(&mut self.code_src, 0..0);
        // An empty block has no text events; it is at the block itself.
        let src = if text.is_empty() { self.at.start..self.at.start } else { src };
        let map = TextMap::from_lines(text, &self.source, src);
        // Never wait for the highlighter: show the block now and let
        // `Doc::highlight` color it once the tokens are there.
        match eng.highlighter.try_get(lang, text) {
            Some(Some(tokens)) => {
                self.mono_block(eng, &code_spans(&tokens, &self.theme), self.theme.fg, lang, map, true)
            }
            Some(None) => self.mono_block(eng, &plain(), self.theme.fg, lang, map, true),
            None => {
                self.mono_block(eng, &plain(), self.theme.fg, lang, map, true);
                let width = self.right - self.x - 2.0 * code_pad(self.em).0;
                self.uncolored.push_back(Uncolored {
                    item: self.codes.last().map_or(0, |code| code.item),
                    lang: lang.to_string(),
                    code: text.to_string(),
                    width,
                });
            }
        }
    }

    fn html_block(&mut self, eng: &mut Engine, html: &str) {
        let html = html.trim();
        if html.is_empty() || (html.starts_with("<!--") && html.ends_with("-->")) {
            return;
        }
        // Shown as written, so it maps to the source line by line.
        let block = &self.source[self.at.start as usize..self.at.end as usize];
        let start = self.at.start + (block.len() - block.trim_start().len()) as u32;
        let map = TextMap::from_lines(html, &self.source, start..self.at.end);
        let span = Span { text: html.to_string(), ..Span::default() };
        self.mono_block(eng, &[span], self.theme.dim, "", map, false);
    }

    /// A monospace block in a rounded frame (code blocks, raw HTML). A code
    /// block (`code`) gets its language and a copy symbol in the top right
    /// corner. Its text is the last item pushed.
    fn mono_block(&mut self, eng: &mut Engine, spans: &[Span], fg: Rgba, label: &str, map: TextMap, code: bool) {
        let em = self.em;
        let st = code_style(em);
        let (pad_x, pad_y) = code_pad(em);
        let width = self.right - self.x;
        let buf = make_buffer(eng, spans, &st, Some(width - 2.0 * pad_x));
        let (_, text_h) = measure(&buf);
        let text_h = text_h.max(st.line_h);
        let h = text_h + 2.0 * pad_y;
        let y = self.place(eng, h, em, em, (em * BODY_LINE).round());
        let (line, radius) = (self.hairline(), (0.55 * em).round());
        self.push(y, h, Kind::Outline { x: self.x, w: width, radius, line, color: self.theme.border });

        // The corner: [language] [copy symbol]. Left out when the first line
        // of code reaches under it.
        let first_line = buf.layout_runs().next().map_or(0.0, |run| run.line_w);
        let label_st = label_style(em);
        let icon = (0.8 * em).round();
        let mut corner = self.right - (0.75 * em).round();
        let corner_y = y + (0.55 * em).round();
        let mut button = [0.0; 4];
        if code && first_line + em <= corner - icon - self.x - pad_x {
            corner -= icon;
            let icon_y = corner_y + ((label_st.line_h - icon) / 2.0).round();
            self.push(icon_y, icon, Kind::CopyIcon { x: corner, color: self.theme.dim });
            // Generous: the whole corner around the symbol.
            let slop = (0.5 * em).round();
            button = [corner - slop, y, self.right - corner + slop, icon_y - y + icon + slop];
            corner -= (0.5 * em).round();
        }
        if !label.is_empty() {
            let span = Span { text: label.to_string(), ..Span::default() };
            let tag = make_buffer(eng, &[span], &label_st, None);
            let tag_w = measure(&tag).0;
            if first_line + em <= corner - tag_w - self.x - pad_x {
                self.push_label(corner - tag_w, corner_y, label_st.line_h, tag, self.theme.dim);
            }
        }
        self.push_text(self.x + pad_x, y + pad_y, text_h, buf, fg, map, Join::Block);
        if code {
            let src = self.at.clone();
            self.codes.push(CodeBlock { item: self.items.len() - 1, frame: [self.x, y, width, h], button, src });
        }
    }

    fn image_block(&mut self, eng: &mut Engine, dest: &str, alt: &str) {
        let avail = self.right - self.x;
        // Images are authored in CSS pixels (16px body text); scale to ours.
        let scale = self.em / 16.0;
        let scaled = load_image(&mut eng.images, &self.base_dir, dest).map(|src| {
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
                self.flush_inline(eng, true);
                let h = img.height() as f32;
                let y = self.place(eng, h, 0.9 * self.em, 0.9 * self.em, (self.em * BODY_LINE).round());
                self.push(y, h, Kind::Image { x: self.x, img });
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

    fn table_block(&mut self, eng: &mut Engine, table: Table) {
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let em = self.em;
        let (pad_x, pad_y, line) = ((0.85 * em).round(), (0.55 * em).round(), self.hairline());
        let st = text_style(em, 1.5);
        let head_st = TextStyle { weight: BOLD, ..text_style(em, 1.5) };

        // Pass 1: for every column, the width its content would like
        // (nothing wrapped) and the least it can do with (its longest word).
        let mut cells: Vec<Vec<(Buffer, TextMap)>> = Vec::new();
        let mut natural = vec![0.0f32; cols];
        let mut least = vec![0.0f32; cols];
        for (r, row) in table.rows.iter().enumerate() {
            let style = if r < table.head_rows { &head_st } else { &st };
            let mut bufs = Vec::new();
            for (c, spans) in row.iter().enumerate() {
                let buf = make_buffer(eng, spans, style, None);
                natural[c] = natural[c].max(measure(&buf).0.ceil() + 1.0);
                least[c] = least[c].max(longest_word(&buf).ceil() + 1.0);
                bufs.push((buf, span_map(spans)));
            }
            cells.push(bufs);
        }
        // A column is not squeezed below a few characters even if its
        // words are shorter, nor of course made wider than it wants.
        for (least, natural) in least.iter_mut().zip(&natural) {
            *least = least.max(5.0 * em).min(*natural);
        }

        // How wide may it be? A table that does not fit the text column
        // first grows out of it, up to the page; then its cells wrap; and
        // if it would still not fit with every column at its least, it
        // keeps a comfortable width and scrolls sideways. Inside a list or
        // a quote it stays in its place.
        let chrome = cols as f32 * 2.0 * pad_x;
        let column = self.right - self.x;
        let nested = !self.lists.is_empty() || !self.quotes.is_empty();
        let margin = (1.2 * em).round();
        let page = if nested { column } else { (self.page_w - 2.0 * margin).max(column) };
        let wanted = natural.iter().sum::<f32>() + chrome;
        let scrolls = self.scroll_tables && least.iter().sum::<f32>() + chrome > page;
        let widths = if wanted <= page {
            natural
        } else if scrolls {
            let comfortable = 14.0 * em;
            natural.iter().zip(&least).map(|(n, l)| n.min(l.max(comfortable))).collect()
        } else {
            fit_columns(&natural, &least, page - chrome)
        };

        // Pass 2: wrap to the final widths and measure row heights.
        let mut heights = Vec::new();
        for row in &mut cells {
            let mut row_h = st.line_h;
            for (c, (buf, _)) in row.iter_mut().enumerate() {
                let align = match table.aligns.get(c) {
                    Some(Alignment::Center) => Some(Align::Center),
                    Some(Alignment::Right) => Some(Align::Right),
                    _ => None,
                };
                for l in &mut buf.lines {
                    l.set_align(align);
                }
                wrap(&mut eng.fs, buf, Some(widths[c]));
                row_h = row_h.max(measure(buf).1);
            }
            heights.push(row_h + 2.0 * pad_y);
        }

        // Rows separated by hairlines inside a rounded frame; the header
        // row is tinted.
        let rows = heights.len() as f32;
        let table_w = widths.iter().sum::<f32>() + chrome;
        let table_h = heights.iter().sum::<f32>() + (rows + 1.0) * line;
        let radius = (0.5 * em).round();
        let y = self.place(eng, table_h, em, em, st.line_h);
        let head_h: f32 = heights[..table.head_rows].iter().sum::<f32>() + line;
        // In the column if it fits there, else centered on the page. (Where
        // tables cannot scroll, one that is still too wide is cut off.)
        let shown_w = if scrolls { table_w.min(page) } else { table_w };
        let x = match nested || shown_w <= column {
            true => self.x,
            false => ((self.page_w - shown_w.min(page)) / 2.0).round(),
        };
        let number = self.tables.len() as u16 + 1;
        self.layer = if table_w > shown_w { number } else { 0 };
        self.tables.push(TableBox {
            x,
            y,
            w: shown_w,
            h: table_h,
            full_w: table_w,
            scroll: 0.0,
            head_h: if table.head_rows > 0 { head_h + line } else { 0.0 },
            layer: self.layer,
        });

        // Tints are translucent, so the shapes of a table keep clear of
        // each other: the header's fill and the rules stay inside the frame.
        let inner_w = table_w - 2.0 * line;
        if table.head_rows > 0 {
            let radii = [radius - line, radius - line, 0.0, 0.0];
            let fill = Kind::Rect { x: x + line, w: inner_w, radii, color: self.theme.table_head };
            self.push(y + line, head_h - line, fill);
        }
        let color = self.text_color();
        let mut cy = y + line;
        for (r, row) in cells.into_iter().enumerate() {
            if r > 0 {
                // The rule under the header is twice as heavy.
                let weight = if r == table.head_rows { 2.0 * line } else { line };
                self.rect(x + line, cy - line, inner_w, weight, 0.0, self.theme.border);
            }
            let mut cx = x;
            for (c, ((buf, map), width)) in row.into_iter().zip(&widths).enumerate() {
                self.code_boxes(&buf, cx + pad_x, cy + pad_y);
                let join = match (r, c) {
                    (0, 0) => Join::Block,
                    (_, 0) => Join::Line,
                    _ => Join::Cell,
                };
                self.push_text(cx + pad_x, cy + pad_y, heights[r] - 2.0 * pad_y, buf, color, map, join);
                cx += width + 2.0 * pad_x;
            }
            cy += heights[r] + line;
        }
        let outline = Kind::Outline { x, w: table_w, radius, line, color: self.theme.border };
        self.push(y, table_h, outline);
        self.layer = 0;
    }
}

/// Width of the longest stretch of an unwrapped buffer that cannot be
/// broken: a word, or a single CJK character.
fn longest_word(buf: &Buffer) -> f32 {
    let mut longest = 0.0f32;
    for run in buf.layout_runs() {
        let mut word = 0.0;
        for g in run.glyphs {
            let c = run.text[g.start..].chars().next().unwrap_or(' ');
            if c.is_whitespace() {
                word = 0.0;
            } else if is_cjk(c) {
                longest = longest.max(g.w);
                word = 0.0;
            } else {
                // Code may come to start a line, which takes an indent.
                if word == 0.0 && g.metadata & META_CODE != 0 {
                    word = CODE_PAD * buf.metrics().font_size;
                }
                word += g.w;
                longest = longest.max(word);
            }
        }
    }
    longest
}

/// The text in a buffer, as it would be typed.
fn buffer_text(buf: &Buffer) -> String {
    let mut text = String::new();
    for (i, line) in buf.lines.iter().enumerate() {
        text.push_str(line.text());
        // The last line has no line break after it, whatever it reports.
        if i + 1 < buf.lines.len() {
            text.push_str(line.ending().as_str());
        }
    }
    text
}

/// The name a heading can be linked to with `#name`, the way GitHub derives
/// it: lower case, punctuation dropped, spaces as hyphens, and a number
/// appended if `taken` already has it.
fn heading_anchor<'a>(text: &str, taken: impl Iterator<Item = &'a str> + Clone) -> String {
    let base: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();
    let free = |name: &str| !taken.clone().any(|t| t == name);
    if free(&base) {
        return base;
    }
    (1..).map(|n| format!("{base}-{n}")).find(|name| free(name)).unwrap_or(base)
}

/// The (label, value) rows of a YAML front matter block. Only top-level
/// `key: value` lines are understood; anything else (nested mappings, list
/// items) is kept as further lines of the value before it.
fn front_matter_rows(text: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let nested = line.starts_with([' ', '\t', '-', '#']);
        match line.split_once(':') {
            Some((key, value)) if !nested && !key.trim().is_empty() => {
                let value = value.trim();
                let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
                let value = if quoted('"') || quoted('\'') { &value[1..value.len() - 1] } else { value };
                // "title" reads better as "Title" in a label.
                let mut key: Vec<char> = key.trim().chars().collect();
                key[0] = key[0].to_ascii_uppercase();
                rows.push((key.into_iter().collect(), value.to_string()));
            }
            _ => {
                if let Some((_, value)) = rows.last_mut() {
                    if !value.is_empty() {
                        value.push('\n');
                    }
                    value.push_str(line.trim());
                }
            }
        }
    }
    rows
}

/// Column widths for a table whose columns would like `natural` widths,
/// need at least `least`, and have `room` in total. If everything fits,
/// every column gets what it wants; otherwise each gives up the same share
/// of what it can spare, so a column of short words keeps its width and
/// the long-winded ones wrap.
fn fit_columns(natural: &[f32], least: &[f32], room: f32) -> Vec<f32> {
    let (wanted, needed): (f32, f32) = (natural.iter().sum(), least.iter().sum());
    if wanted <= room {
        return natural.to_vec();
    }
    let spare = ((room - needed) / (wanted - needed).max(1.0)).clamp(0.0, 1.0);
    natural.iter().zip(least).map(|(n, l)| (l + (n - l) * spare).floor()).collect()
}

fn load_image<'a>(
    images: &'a mut HashMap<PathBuf, Option<RgbaImage>>,
    base_dir: &Path,
    dest: &str,
) -> Option<&'a RgbaImage> {
    if dest.contains("://") && !dest.starts_with("file://") {
        return None;
    }
    let path = base_dir.join(dest.strip_prefix("file://").unwrap_or(dest));
    images
        .entry(path)
        .or_insert_with_key(|p| image::open(p).ok().map(|i| i.to_rgba8()))
        .as_ref()
}

// ---------------------------------------------------------------------------
// Rasterization
// ---------------------------------------------------------------------------

/// A strip of the page being drawn: premultiplied RGBA, `w` × `h` pixels.
struct Canvas<'a> {
    data: &'a mut [u8],
    w: i32,
    h: i32,
    /// Maps glyph coverage to the opacity it is drawn with.
    ink: &'a [u8; 256],
}

/// How much opacity a glyph pixel gets for its coverage. Dark text on a
/// light background reads thinner than the same outlines in light on dark,
/// and thin faces (a Song typeface at text size) nearly fall apart; text
/// renderers compensate by thickening dark text a little, and so does this.
fn ink(dark_page: bool) -> [u8; 256] {
    let gamma = if dark_page { 1.0 } else { 0.72 };
    std::array::from_fn(|coverage| ((coverage as f32 / 255.0).powf(gamma) * 255.0).round() as u8)
}

/// `x / 255`, rounded, for `x` up to 255 * 255.
#[inline]
fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

impl Canvas<'_> {
    /// Source-over `c` at `coverage` (0..=255) onto one pixel.
    #[inline]
    fn blend(px: &mut [u8], c: Rgba, coverage: u32) {
        let a = div255(c.3 as u32 * coverage);
        if a == 0 {
            return;
        }
        let inv = 255 - a;
        px[0] = div255(c.0 as u32 * a + px[0] as u32 * inv) as u8;
        px[1] = div255(c.1 as u32 * a + px[1] as u32 * inv) as u8;
        px[2] = div255(c.2 as u32 * a + px[2] as u32 * inv) as u8;
        px[3] = (a + div255(px[3] as u32 * inv)) as u8;
    }

    /// Clip the box at (`x`, `y`) of size `w` × `h` to the canvas. Returns
    /// the visible part as (x0, y0, x1, y1), or `None` if there is none.
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
        let (x0, y0, x1, y1) = (x.max(0), y.max(0), (x + w).min(self.w), (y + h).min(self.h));
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }

    fn row(&mut self, y: i32, x0: i32, x1: i32) -> &mut [u8] {
        let start = (y * self.w + x0) as usize * 4;
        &mut self.data[start..start + (x1 - x0) as usize * 4]
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgba) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, w, h) else { return };
        for py in y0..y1 {
            for px in self.row(py, x0, x1).chunks_exact_mut(4) {
                Self::blend(px, c, 255);
            }
        }
    }

    /// Draw a rasterized glyph with its top-left at (`x`, `y`).
    fn glyph(&mut self, x: i32, y: i32, img: &SwashImage, c: Rgba) {
        let (gw, gh) = (img.placement.width as i32, img.placement.height as i32);
        let Some((x0, y0, x1, y1)) = self.clip(x, y, gw, gh) else { return };
        let ink = self.ink;
        for py in y0..y1 {
            let src = ((py - y) * gw + (x0 - x)) as usize;
            let dst = self.row(py, x0, x1).chunks_exact_mut(4);
            match img.content {
                SwashContent::Mask => {
                    for (px, &cov) in dst.zip(&img.data[src..]) {
                        if cov != 0 {
                            Self::blend(px, c, ink[cov as usize] as u32);
                        }
                    }
                }
                // Color glyphs (emoji) carry their own straight-alpha color.
                SwashContent::Color => {
                    for (px, s) in dst.zip(img.data[src * 4..].chunks_exact(4)) {
                        Self::blend(px, Rgba(s[0], s[1], s[2], s[3]), c.3 as u32);
                    }
                }
                SwashContent::SubpixelMask => {}
            }
        }
    }

    fn image(&mut self, x: i32, y: i32, img: &RgbaImage) {
        let (iw, ih) = (img.width() as i32, img.height() as i32);
        let Some((x0, y0, x1, y1)) = self.clip(x, y, iw, ih) else { return };
        let pixels = img.as_raw();
        for py in y0..y1 {
            let src = ((py - y) * iw + (x0 - x)) as usize * 4;
            for (px, s) in self.row(py, x0, x1).chunks_exact_mut(4).zip(pixels[src..].chunks_exact(4)) {
                Self::blend(px, Rgba(s[0], s[1], s[2], s[3]), 255);
            }
        }
    }
}

impl Doc {
    /// Rasterize a rectangle of the page as straight RGBA: `width` pixels
    /// from `x0`, `height` from `y0`. `layer` 0 is the page; layer `n` is
    /// sideways scrolling table number `n`, whose image starts at the
    /// table's left edge and is as wide as the whole table. With `lit` the
    /// page stands on the selection color: what the viewer shows in place
    /// of the selected parts.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        eng: &mut Engine,
        layer: u16,
        x0: u32,
        y0: u32,
        width: u32,
        height: u32,
        lit: bool,
    ) -> Vec<u8> {
        let (w, h) = (width.max(1), height.max(1));
        let mut pm = Pixmap::new(w, h).expect("non-zero pixmap size");
        let theme = &self.lay.theme;
        let ground = if lit { Some(theme.select) } else { theme.page };
        if let Some(bg) = ground {
            pm.fill(tiny_skia::Color::from_rgba8(bg.0, bg.1, bg.2, 255));
        }

        let ink = ink(theme.dark);
        let (top, bottom) = (y0 as f32, (y0 + height) as f32);
        // How far outside its line box a glyph may paint.
        let slack = self.lay.em;
        let shift = Transform::from_translate(-(x0 as f32), -top);
        let left = x0 as i32;
        for item in &self.lay.items {
            if item.layer != layer || item.y - slack >= bottom || item.y + item.h + slack <= top {
                continue;
            }
            match &item.kind {
                Kind::Rect { x, w, radii, color } => fill_rrect(&mut pm, *x, item.y, *w, item.h, *radii, *color, shift),
                Kind::Outline { x, w, radius, line, color } => {
                    // Stroke centered half a line inside the box, so the
                    // outline is exactly `line` thick and stays in bounds.
                    let half = line / 2.0;
                    let radii = [(radius - half).max(0.0); 4];
                    if let Some(path) = rrect(x + half, item.y + half, w - line, item.h - line, radii) {
                        let stroke = Stroke { width: *line, ..Stroke::default() };
                        pm.stroke_path(&path, &paint(*color), &stroke, shift, None);
                    }
                }
                Kind::Check { x } => {
                    let (x, y, s) = (*x, item.y, item.h);
                    let mut pb = PathBuilder::new();
                    pb.move_to(x + s * 0.25, y + s * 0.52);
                    pb.line_to(x + s * 0.43, y + s * 0.69);
                    pb.line_to(x + s * 0.75, y + s * 0.33);
                    if let Some(path) = pb.finish() {
                        let stroke = Stroke {
                            width: (s * 0.12).max(1.5),
                            line_cap: tiny_skia::LineCap::Round,
                            line_join: tiny_skia::LineJoin::Round,
                            ..Stroke::default()
                        };
                        pm.stroke_path(&path, &paint(theme.paper.opaque()), &stroke, shift, None);
                    }
                }
                Kind::CopyIcon { x, color } => {
                    // Two sheets: the front one whole, the back one where
                    // it shows behind it.
                    let (x, y, s) = (*x, item.y, item.h);
                    let (side, r) = (s * 0.66, s * 0.16);
                    let stroke = Stroke {
                        width: (s * 0.085).max(1.0),
                        line_cap: tiny_skia::LineCap::Round,
                        line_join: tiny_skia::LineJoin::Round,
                        ..Stroke::default()
                    };
                    let half = stroke.width / 2.0;
                    if let Some(front) = rrect(x + half, y + s - side - half, side, side, [r; 4]) {
                        pm.stroke_path(&front, &paint(*color), &stroke, shift, None);
                    }
                    let (left, right, top, bottom) = (x + s - side - half, x + s - half, y + half, y + side + half);
                    let k = r * 0.5523;
                    let mut pb = PathBuilder::new();
                    pb.move_to(left, y + s - side - half - s * 0.14);
                    pb.line_to(left, top + r);
                    pb.cubic_to(left, top + r - k, left + r - k, top, left + r, top);
                    pb.line_to(right - r, top);
                    pb.cubic_to(right - r + k, top, right, top + r - k, right, top + r);
                    pb.line_to(right, bottom - r);
                    pb.cubic_to(right, bottom - r + k, right - r + k, bottom, right - r, bottom);
                    pb.line_to(x + half + side + s * 0.14, bottom);
                    if let Some(back) = pb.finish() {
                        pm.stroke_path(&back, &paint(*color), &stroke, shift, None);
                    }
                }
                Kind::Image { x, img } => {
                    let mut canvas = Canvas { data: pm.data_mut(), w: w as i32, h: h as i32, ink: &ink };
                    canvas.image(x.round() as i32 - left, (item.y - top).round() as i32, img);
                }
                Kind::Text { x, buf, color: default, .. } => {
                    let mut canvas = Canvas { data: pm.data_mut(), w: w as i32, h: h as i32, ink: &ink };
                    let (ox, oy) = (x.round() as i32 - left, (item.y - top).round() as i32);
                    for run in buf.layout_runs() {
                        // Skip lines outside the strip: a long code block
                        // spans many tiles but each needs only a few lines.
                        let line_top = item.y + run.line_top;
                        if line_top - slack >= bottom || line_top + run.line_height + slack <= top {
                            continue;
                        }
                        let dx = indent(buf, &run);
                        for g in run.glyphs {
                            let pg = g.physical((dx, run.line_y), 1.0);
                            let c = g.color_opt.map_or(*default, |c| Rgba(c.r(), c.g(), c.b(), c.a()));
                            if let Some(img) = eng.swash.get_image(&mut eng.fs, pg.cache_key) {
                                let (gx, gy) = (ox + pg.x + img.placement.left, oy + pg.y - img.placement.top);
                                canvas.glyph(gx, gy, img, c);
                            }
                        }
                        // Lines through or under runs of glyphs marked for it:
                        // (flag, position above the baseline, thickness), in em.
                        for (flag, rise, weight) in [(META_STRIKE, 0.3, 0.065), (META_LINK, -0.17, 0.05)] {
                            let mut glyphs = run.glyphs.iter().peekable();
                            while let Some(g) = glyphs.next() {
                                if g.metadata & flag == 0 {
                                    continue;
                                }
                                let (x0, mut x1) = (dx + g.x, dx + g.x + g.w);
                                while let Some(next) = glyphs.next_if(|n| n.metadata & flag != 0) {
                                    x1 = dx + next.x + next.w;
                                }
                                let c = g.color_opt.map_or(*default, |c| Rgba(c.r(), c.g(), c.b(), c.a()));
                                let thick = (g.font_size * weight).round().max(1.0);
                                let y = (run.line_y - g.font_size * rise).round();
                                let (x, w) = (ox + x0.round() as i32, (x1 - x0).round() as i32);
                                canvas.fill(x, oy + y as i32, w, thick as i32, c);
                            }
                        }
                    }
                }
            }
        }

        // On its own ground every pixel is opaque already. A transparent
        // page is blended with the terminal's background here where that is
        // known, and leaves it to the terminal otherwise.
        let mut data = pm.take();
        match (ground, theme.behind) {
            (Some(_), _) => {}
            (None, Some(bg)) => flatten(&mut data, bg),
            (None, None) => unpremultiply(&mut data),
        }
        data
    }

    /// Render the whole document as one image of the page, for PNG export.
    pub fn render_page(&mut self, eng: &mut Engine) -> Option<RgbaImage> {
        self.advance(eng, None, None);
        self.highlight(eng, None);
        let (w, h) = (self.lay.page_w as u32, self.height.max(1));
        RgbaImage::from_raw(w, h, self.render(eng, 0, 0, 0, w, h, false))
    }
}

/// Call `change` on the translucent pixels of premultiplied `data`. Those
/// are few (the edges of glyphs and shapes, and tinted fills), so whole
/// blocks of transparent or opaque pixels are skipped with a check the
/// compiler can vectorize.
fn for_translucent(data: &mut [u8], change: impl Fn(&mut [u8])) {
    for block in data.chunks_mut(64 * 4) {
        let translucent = |px: &[u8]| px[3].wrapping_sub(1) < 254;
        if !block.chunks_exact(4).fold(false, |any, px| any | translucent(px)) {
            continue;
        }
        for px in block.chunks_exact_mut(4).filter(|px| translucent(px)) {
            change(px);
        }
    }
}

/// Premultiplied → straight alpha, which is what the Kitty protocol takes.
fn unpremultiply(data: &mut [u8]) {
    for_translucent(data, |px| {
        let a = px[3] as u32;
        for c in &mut px[..3] {
            *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
        }
    });
}

/// Put what has been drawn on the background color `bg`: every pixel that
/// was touched becomes opaque, the rest stays transparent.
fn flatten(data: &mut [u8], bg: Rgb) {
    for_translucent(data, |px| {
        let inv = 255 - px[3] as u32;
        for (c, bg) in px.iter_mut().zip([bg.0, bg.1, bg.2]) {
            *c = (*c as u32 + div255(bg as u32 * inv)).min(255) as u8;
        }
        px[3] = 255;
    });
}

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.0, c.1, c.2, c.3);
    p.anti_alias = true;
    p
}

/// Rounded rectangle path; `radii` are clockwise from top-left.
fn rrect(x: f32, y: f32, w: f32, h: f32, radii: [f32; 4]) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let [tl, tr, br, bl] = radii.map(|r| r.min(w / 2.0).min(h / 2.0).max(0.0));
    const K: f32 = 0.5523; // cubic approximation of a quarter circle
    let mut pb = PathBuilder::new();
    pb.move_to(x + tl, y);
    pb.line_to(x + w - tr, y);
    pb.cubic_to(x + w - tr + tr * K, y, x + w, y + tr - tr * K, x + w, y + tr);
    pb.line_to(x + w, y + h - br);
    pb.cubic_to(x + w, y + h - br + br * K, x + w - br + br * K, y + h, x + w - br, y + h);
    pb.line_to(x + bl, y + h);
    pb.cubic_to(x + bl - bl * K, y + h, x, y + h - bl + bl * K, x, y + h - bl);
    pb.line_to(x, y + tl);
    pb.cubic_to(x, y + tl - tl * K, x + tl - tl * K, y, x + tl, y);
    pb.close();
    pb.finish()
}

#[allow(clippy::too_many_arguments)]
fn fill_rrect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, radii: [f32; 4], c: Rgba, ts: Transform) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if radii.iter().all(|&r| r <= 0.0) {
        // Snap square corners to the pixel grid so edges stay crisp.
        let (x0, y0) = (x.round(), y.round());
        if let Some(rect) = tiny_skia::Rect::from_xywh(x0, y0, w.round().max(1.0), h.round().max(1.0)) {
            pm.fill_rect(rect, &paint(c), ts, None);
        }
    } else if let Some(path) = rrect(x, y, w, h, radii) {
        pm.fill_path(&path, &paint(c), FillRule::Winding, ts, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with the machine's fonts; `None` where there are none.
    fn engine() -> Option<Engine> {
        let installed = crate::fonts::installed_for_tests()?;
        Some(Engine::new(installed, Highlighter::new(), &Choice::default()))
    }

    fn layout(eng: &mut Engine, markdown: &str, page_w: u32) -> Doc {
        let theme = Theme::new(Some(Rgb(255, 255, 255)), Some(Rgb(0, 0, 0)), None);
        let opts = LayoutOpts { page_w, font_px: 16.0, base_dir: PathBuf::from("."), theme, scroll_tables: true };
        let mut doc = Doc::new(eng, markdown, opts);
        doc.advance(eng, None, None);
        doc
    }

    /// The middle of the first character of `needle` on the page.
    fn find(doc: &Doc, needle: &str) -> (f32, f32) {
        for (_, item, x, buf, _) in doc.texts() {
            for run in buf.layout_runs() {
                let Some(at) = run.text.find(needle) else { continue };
                if let Some(g) = run.glyphs.iter().find(|g| g.start == at) {
                    return (x + indent(buf, &run) + g.x + g.w / 2.0, item.y + run.line_top + run.line_height / 2.0);
                }
            }
        }
        panic!("{needle:?} is not on the page");
    }

    const SAMPLE: &str = "\
# Title

First paragraph
second line with [a link](https://example.com/x) in it.

- item one
- item **two**

> quoted
>
> ```rust
> let a = 1;
> let b = 2;
> ```

| Key | Value |
|---|---|
| zeta | omega |

Last &amp; final paragraph.
";

    fn line_at(doc: &Doc, needle: &str) -> u32 {
        let (x, y) = find(doc, needle);
        let hit = doc.hit(x, y).expect("there is text");
        assert!(hit.inside, "{needle:?} should be under the point");
        doc.line_of(&hit).unwrap()
    }

    #[test]
    fn points_map_to_source_lines() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        let expect = [
            ("Title", 1),
            ("First", 3),
            ("second", 4),
            ("a link", 4),
            ("in it", 4),
            ("item one", 6),
            ("two", 7),
            ("quoted", 9),
            ("let a", 12),
            ("b = 2", 13),
            ("Key", 16),
            ("zeta", 18),
            ("omega", 18),
            ("Last", 20),
            ("final", 20),
        ];
        for (needle, line) in expect {
            assert_eq!(line_at(&doc, needle), line, "line of {needle:?}");
        }
    }

    #[test]
    fn finds_links_under_a_point() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        let (x, y) = find(&doc, "a link");
        let link = doc.hit(x, y).unwrap().link.expect("a link is there");
        assert_eq!(doc.link(link), Some("https://example.com/x"));
        let (x, y) = find(&doc, "second");
        assert_eq!(doc.hit(x, y).unwrap().link, None);
    }

    #[test]
    fn points_beside_text_snap_to_it() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        let (x, y) = find(&doc, "Title");
        // Far to the right of the heading: its end, and not "inside".
        let hit = doc.hit(x + 800.0, y).unwrap();
        assert_eq!((hit.offset, hit.inside), ("Title".len() as u32, false));
        // Above everything: the start of the first text.
        let hit = doc.hit(0.0, 0.0).unwrap();
        assert_eq!((hit.offset, doc.line_of(&hit)), (0, Some(1)));
    }

    #[test]
    fn viewport_top_maps_to_a_source_line() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        assert_eq!(doc.source_line(0), Some(1));
        let (_, y) = find(&doc, "let b");
        assert_eq!(doc.source_line(y as u32), Some(13));
        // Monotonic down the page.
        let lines: Vec<u32> = (0..doc.height()).step_by(7).filter_map(|y| doc.source_line(y)).collect();
        assert!(lines.windows(2).all(|w| w[0] <= w[1]), "{lines:?}");
        assert_eq!(lines.last(), Some(&20));
    }

    #[test]
    fn positions_survive_a_relayout() {
        let Some(mut eng) = engine() else { return };
        let wide = layout(&mut eng, SAMPLE, 900);
        // In the same layout an anchor leads back to where it was taken.
        for y in (0..wide.height().saturating_sub(40)).step_by(13) {
            let anchor = wide.anchor(y).unwrap();
            let mut same = layout(&mut eng, SAMPLE, 900);
            let back = same.resolve(&mut eng, anchor);
            assert!(back.abs_diff(y) <= 1, "{y} came back as {back}");
        }
        // At another width the same source line is at the top again.
        for needle in ["second", "item one", "let b", "omega", "Last"] {
            let (_, y) = find(&wide, needle);
            let line = wide.source_line(y as u32).unwrap();
            let anchor = wide.anchor(y as u32).unwrap();
            let mut narrow = layout(&mut eng, SAMPLE, 300);
            let y = narrow.resolve(&mut eng, anchor);
            assert_eq!(narrow.source_line(y), Some(line), "at {needle:?}");
        }
    }

    #[test]
    fn source_lines_are_found_on_the_page() {
        let Some(mut eng) = engine() else { return };
        let mut doc = layout(&mut eng, SAMPLE, 900);
        for line in [1, 3, 6, 7, 9, 12, 13, 18, 20] {
            let y = doc.offset_of_line(&mut eng, line);
            assert_eq!(doc.source_line(y), Some(line));
        }
        // A line with nothing on the page (a table's delimiter row, a blank
        // line) leads to the next text.
        let y = doc.offset_of_line(&mut eng, 19);
        assert_eq!(doc.source_line(y), Some(20));
    }

    #[test]
    fn splits_scripts() {
        assert_eq!(script_runs("用 Nix 构建"), vec![("用 ", true), ("Nix ", false), ("构建", true)]);
        assert_eq!(script_runs("plain text"), vec![("plain text", false)]);
        assert_eq!(script_runs("  中文"), vec![("  中文", true)]);
        assert_eq!(script_runs(""), vec![]);
    }

    #[test]
    fn names_headings_like_github() {
        let none = std::iter::empty::<&str>();
        assert_eq!(heading_anchor("Getting Started", none.clone()), "getting-started");
        assert_eq!(heading_anchor("  What's new in v0.2.0?  ", none.clone()), "whats-new-in-v020");
        assert_eq!(heading_anchor("开发与发布", none.clone()), "开发与发布");
        assert_eq!(heading_anchor("snake_case & kebab-case", none), "snake_case--kebab-case");
        let taken = ["usage", "usage-1"];
        assert_eq!(heading_anchor("Usage", taken.iter().copied()), "usage-2");
    }

    /// The place in the text right before the first character of `needle`.
    fn before(doc: &Doc, needle: &str) -> Pos {
        let (x, y) = find(doc, needle);
        doc.hit(x - 1.0, y).unwrap().pos()
    }

    #[test]
    fn selections_copy_as_plain_text() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        let text = |from: &str, to: &str| doc.selected_text(before(&doc, from), before(&doc, to));
        // Inside one paragraph; the source line break is a space.
        assert_eq!(text("paragraph", "with"), "paragraph second line ");
        // Across blocks: an empty line between them, markers on list items.
        assert_eq!(text("in it", "two"), "in it.\n\n- item one\n- item ");
        // Starting inside an item leaves its marker out.
        assert_eq!(text("one", "two"), "one\n- item ");
        // Code keeps its lines; a table is tab-separated rows.
        assert_eq!(text("quoted", "Value"), "quoted\n\nlet a = 1;\nlet b = 2;\n\nKey\t");
        assert_eq!(text("Key", "Last"), "Key\tValue\nzeta\tomega\n\n");
        assert_eq!(text("Last", "final"), "Last & ");
    }

    #[test]
    fn selections_copy_as_markdown() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, SAMPLE, 900);
        let source = |from: &str, to: &str| doc.selected_source(before(&doc, from), before(&doc, to));
        // Within a line: exactly what is between the two characters.
        assert_eq!(source("line with", "in it"), "line with [a link](https://example.com/x) ");
        assert_eq!(source("Last", "final"), "Last &amp; ");
        // Across lines: the whole lines, markup and all.
        assert_eq!(source("one", "two"), "- item one\n- item **two**");
        assert_eq!(source("let a", "b = 2"), "> let a = 1;\n> let b = 2;");
        assert_eq!(source("Key", "omega"), "| Key | Value |\n|---|---|\n| zeta | omega |");
    }

    #[test]
    fn selection_highlight_follows_the_lines() {
        let Some(mut eng) = engine() else { return };
        // So narrow that the text column is at its minimum (4 em, less than
        // the longest word), whatever font the machine has: the paragraph
        // breaks into well over three lines.
        let doc = layout(&mut eng, SAMPLE, 100);
        let (from, to) = (before(&doc, "First"), before(&doc, "item one"));
        let rects: Vec<[f32; 4]> =
            doc.selection_rects(from, to, 0.0, doc.height() as f32).into_iter().map(|(_, rect)| rect).collect();
        // One rectangle per line, each right under the one before.
        assert!(rects.len() >= 3, "{rects:?}");
        for pair in rects.windows(2) {
            assert!(pair[1][1] >= pair[0][1] + pair[0][3] - 0.5, "{pair:?}");
        }
        assert!(rects.iter().all(|r| r[2] > 0.0 && r[3] > 0.0));
        // Nothing outside the requested range of the page.
        let (_, y) = find(&doc, "First");
        assert!(doc.selection_rects(from, to, 0.0, y - 30.0).is_empty());
        // Nothing selected, nothing highlighted.
        assert!(doc.selection_rects(from, from, 0.0, doc.height() as f32).is_empty());
    }

    #[test]
    fn double_click_selects_a_word() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, "Hello wide_world, 中文 end.\n", 900);
        let word = |needle: &str| {
            let (x, y) = find(&doc, needle);
            let (from, to) = doc.word_at(doc.hit(x, y).unwrap().character());
            doc.selected_text(from, to)
        };
        assert_eq!(word("llo"), "Hello");
        assert_eq!(word("world"), "wide_world");
        assert_eq!(word(","), ",");
        assert_eq!(word("文"), "文");
        let (from, to) = doc.block_at(before(&doc, "wide"));
        assert_eq!(doc.selected_text(from, to), "Hello wide_world, 中文 end.");
    }

    /// A table with `cols` columns of `words` words each.
    fn table(cols: usize, words: usize) -> String {
        let cell = |c: usize| (0..words).map(|w| format!("word{c}x{w}")).collect::<Vec<_>>().join(" ");
        let row = |_| format!("| {} |", (0..cols).map(cell).collect::<Vec<_>>().join(" | "));
        let rule = format!("|{}", "---|".repeat(cols));
        format!("intro\n\n{}\n{rule}\n{}\n{}\n\noutro\n", row(0), row(1), row(2))
    }

    #[test]
    fn tables_grow_out_of_the_column_then_wrap_then_scroll() {
        let Some(mut eng) = engine() else { return };
        // The column is 46 em = 736 px of a 1600 px page.
        let column = |doc: &Doc| (doc.origin_x as f32, (doc.origin_x + doc.width) as f32);

        // Small: stays in the column, at its left edge.
        let doc = layout(&mut eng, &table(3, 1), 1600);
        let (left, right) = column(&doc);
        let t = doc.tables()[0];
        assert!(t.x > left && t.x + t.w < right && t.layer == 0 && t.full_w == t.w, "{t:?}");
        assert_eq!(doc.extent(0, doc.height()), (doc.origin_x, doc.width));

        // Wider than the column, narrower than the page: keeps its natural
        // width (one line per row) and is centered on the page.
        let doc = layout(&mut eng, &table(4, 4), 1600);
        let (left, right) = column(&doc);
        let t = doc.tables()[0];
        assert!(t.x < left && t.x + t.w > right && t.w < 1600.0 && t.layer == 0, "{t:?}");
        assert!(((t.x + t.w / 2.0) - 800.0).abs() <= 1.0, "centered: {t:?}");
        assert!(t.h < 150.0, "rows of one line: {t:?}");
        // The page is drawn wider where the table is, and only there.
        let (x, w) = doc.extent(t.y as u32, (t.y + t.h) as u32);
        assert!(x as f32 <= t.x && (x + w) as f32 >= t.x + t.w);
        assert_eq!(doc.extent(0, t.y as u32 - 1), (doc.origin_x, doc.width));

        // Too wide for the page: fills it, and the cells wrap.
        let doc = layout(&mut eng, &table(4, 14), 1600);
        let t = doc.tables()[0];
        assert!(t.w > 1400.0 && t.w <= 1600.0 && t.layer == 0 && t.full_w == t.w, "{t:?}");
        assert!(t.h > 150.0, "wrapped rows: {t:?}");

        // Too many columns even when wrapped: scrolls sideways.
        let doc = layout(&mut eng, &table(30, 3), 1600);
        let t = doc.tables()[0];
        assert!(t.layer == 1 && t.full_w > t.w && t.w <= 1600.0, "{t:?}");

        // Inside a list a table stays where it is: it wraps or scrolls
        // within the column.
        let nested = format!("- item\n\n  {}", table(4, 4).replace('\n', "\n  "));
        let doc = layout(&mut eng, &nested, 1600);
        let (left, right) = column(&doc);
        let t = doc.tables()[0];
        assert!(t.x > left && t.x + t.w <= right + 1.0, "{t:?}");
    }

    #[test]
    fn a_png_has_no_scrolling_tables() {
        let Some(mut eng) = engine() else { return };
        let theme = Theme::opaque(false);
        let opts = LayoutOpts { page_w: 800, font_px: 16.0, base_dir: PathBuf::from("."), theme, scroll_tables: false };
        let mut doc = Doc::new(&mut eng, &table(30, 3), opts);
        let page = doc.render_page(&mut eng).unwrap();
        assert_eq!((page.width(), page.height()), (800, doc.height()));
        assert_eq!(doc.tables()[0].layer, 0);
    }

    #[test]
    fn scrolled_tables_are_hit_where_they_show() {
        let Some(mut eng) = engine() else { return };
        let mut doc = layout(&mut eng, &table(30, 3), 1600);
        let t = doc.tables()[0];
        // The last column is out of sight until the table is scrolled.
        let (x, y) = find(&doc, "word29x0");
        assert!(x > t.x + t.w);
        assert!(doc.scroll_table(0, 1e9));
        let scroll = doc.tables()[0].scroll;
        assert_eq!(scroll, (t.full_w - t.w).round());
        assert!(!doc.scroll_table(0, 10.0), "already at the end");
        // On the page it is now `scroll` further left.
        let hit = doc.hit(x - scroll, y).unwrap();
        assert!(hit.inside);
        let (from, to) = doc.word_at(hit.character());
        assert_eq!(doc.selected_text(from, to), "word29x0");
        // Its highlight is drawn where it shows, cut off at the table's edge.
        let rects = doc.selection_rects(from, to, 0.0, doc.height() as f32);
        assert_eq!(rects.len(), 1);
        let (layer, rect) = rects[0];
        assert_eq!(layer, t.layer);
        assert!((rect[0] - (x - scroll)).abs() < 12.0 && rect[0] + rect[2] <= t.x + t.w + 0.5);
        // The first column is out of sight now: nothing of it is highlighted.
        let (from, to) = doc.block_at(before(&doc, "word0x0"));
        assert!(doc.selection_rects(from, to, 0.0, doc.height() as f32).is_empty());
        // Beside the table, at the height of its rows, nothing is "inside".
        assert!(!doc.hit(t.x - 5.0, y).unwrap().inside);
    }

    #[test]
    fn code_blocks_can_be_copied() {
        let Some(mut eng) = engine() else { return };
        let mut doc = layout(&mut eng, SAMPLE, 900);
        doc.highlight(&mut eng, None);
        assert_eq!(doc.code_blocks(0.0, doc.height() as f32), vec![0]);
        assert_eq!(doc.code_text(0, false), "let a = 1;\nlet b = 2;");
        assert_eq!(doc.code_text(0, true), "```rust\n> let a = 1;\n> let b = 2;\n> ```");
        // The copy symbol is in the block's top right corner.
        let [x, y, w, _] = doc.code_frame(0);
        assert_eq!(doc.copy_button(x + w - 20.0, y + 16.0), Some(0));
        assert_eq!(doc.copy_button(x + 20.0, y + 16.0), None);
        assert_eq!(doc.code_blocks(0.0, y - 1.0), Vec::<usize>::new());
        // What lights up when it is copied is its text, a rectangle for each
        // line, well inside the frame.
        let [.., h] = doc.code_frame(0);
        let (from, to) = doc.code_span(0);
        let lit = doc.selection_rects(from, to, 0.0, doc.height() as f32);
        assert_eq!(lit.len(), 2);
        for (_, [lx, ly, lw, lh]) in lit {
            assert!(lx > x + 4.0 && lx + lw < x + w - 4.0 && ly > y + 4.0 && ly + lh < y + h - 4.0);
        }
    }

    #[test]
    fn headings_can_be_linked_to() {
        let Some(mut eng) = engine() else { return };
        let theme = Theme::new(None, None, None);
        let opts =
            LayoutOpts { page_w: 900, font_px: 16.0, base_dir: PathBuf::from("."), theme, scroll_tables: true };
        let text = format!("# Top\n\n{}\n## Far Below\n\ntext\n\n## Far Below\n", "paragraph\n\n".repeat(80));
        let mut doc = Doc::new(&mut eng, &text, opts);
        doc.advance(&mut eng, Some(100), None);
        assert!(!doc.done());
        let first = doc.heading(&mut eng, "far-below").expect("found by laying out further");
        let second = doc.heading(&mut eng, "far-below-1").unwrap();
        assert!(first > 1000 && second > first);
        assert_eq!(doc.heading(&mut eng, "top"), Some(doc.heading(&mut eng, "Top").unwrap()));
        assert_eq!(doc.heading(&mut eng, "missing"), None);
    }

    #[test]
    fn reads_front_matter() {
        let rows = front_matter_rows("title: \"Hello: world\"\ntags:\n  - a\n  - b\n\n作者: 某人\n# comment\n");
        let expect = [("Title", "Hello: world"), ("Tags", "- a\n- b"), ("作者", "某人\n# comment")];
        assert_eq!(rows, expect.map(|(k, v)| (k.to_string(), v.to_string())));
        assert!(front_matter_rows("just some text\n").is_empty());
    }

    #[test]
    fn short_columns_keep_their_width() {
        // Everything fits: untouched.
        assert_eq!(fit_columns(&[40.0, 60.0], &[40.0, 30.0], 200.0), vec![40.0, 60.0]);
        // One long column: only it shrinks.
        assert_eq!(fit_columns(&[40.0, 50.0, 900.0], &[40.0, 50.0, 80.0], 400.0), vec![40.0, 50.0, 310.0]);
        // Two long columns give up the same share of what they can spare.
        assert_eq!(fit_columns(&[40.0, 600.0, 300.0], &[40.0, 100.0, 100.0], 440.0), vec![40.0, 242.0, 157.0]);
        assert_eq!(fit_columns(&[40.0, 600.0, 300.0], &[40.0, 100.0, 100.0], 590.0), vec![40.0, 350.0, 200.0]);
        // Not even the least fits: the least it is (and the table scrolls or is cut).
        assert_eq!(fit_columns(&[300.0, 100.0], &[80.0, 60.0], 40.0), vec![80.0, 60.0]);
    }

    #[test]
    fn unpremultiplies() {
        let mut px = [50, 25, 0, 100, 9, 9, 9, 255, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 100, 9, 9, 9, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn flattens_onto_the_background() {
        // Premultiplied: half of (200, 100, 0), an opaque pixel, an untouched one.
        let mut px = [100, 50, 0, 128, 9, 9, 9, 255, 0, 0, 0, 0];
        flatten(&mut px, Rgb(255, 255, 255));
        assert_eq!(px, [227, 177, 127, 255, 9, 9, 9, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn a_known_background_leaves_nothing_to_blend() {
        let Some(mut eng) = engine() else { return };
        let markdown = "Text with `code`, a [link](x) and ~~more~~.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n";
        // The terminal told its background: only opaque and untouched pixels.
        let doc = layout(&mut eng, markdown, 600);
        let pixels = doc.render(&mut eng, 0, 0, 0, 600, doc.height(), false);
        let alphas = |pixels: &[u8]| pixels.chunks_exact(4).map(|px| px[3]).collect::<std::collections::BTreeSet<u8>>();
        assert_eq!(alphas(&pixels).into_iter().collect::<Vec<_>>(), [0, 255]);
        // The highlighted variant stands on the selection color throughout.
        let lit = doc.render(&mut eng, 0, 0, 0, 600, doc.height(), true);
        assert_eq!(alphas(&lit).into_iter().collect::<Vec<_>>(), [255]);
        let select = doc.lay.theme.select;
        assert_eq!(lit[..3], [select.0, select.1, select.2]);
        // It did not: the terminal has to blend, so the edges stay translucent.
        let theme = Theme::new(None, None, None);
        let opts = LayoutOpts { page_w: 600, font_px: 16.0, base_dir: PathBuf::from("."), theme, scroll_tables: true };
        let mut doc = Doc::new(&mut eng, markdown, opts);
        doc.advance(&mut eng, None, None);
        assert!(alphas(&doc.render(&mut eng, 0, 0, 0, 600, doc.height(), false)).len() > 2);
    }

    #[test]
    fn inline_code_has_room_for_its_background() {
        let Some(mut eng) = engine() else { return };
        let doc = layout(&mut eng, "press `j` / `k` now\n", 800);
        let Some(Kind::Text { buf, .. }) = doc.lay.items.iter().map(|item| &item.kind).find(|k| matches!(k, Kind::Text { .. }))
        else {
            panic!("no text")
        };
        let run = buf.layout_runs().next().unwrap();
        let glyph = |text: &str| run.glyphs.iter().find(|g| &run.text[g.start..g.end] == text).unwrap();
        let (j, slash, k) = (glyph("j"), glyph("/"), glyph("k"));
        // The text between two pieces of code is set in the body face, not
        // in the font of the code before it.
        assert_ne!(slash.font_id, j.font_id);
        assert_eq!(k.font_id, j.font_id);
        // Each background keeps clear of the slash, by the same distance on
        // either side.
        let boxes: Vec<(f32, f32)> = doc
            .lay
            .items
            .iter()
            .filter_map(|item| match item.kind {
                Kind::Rect { x, w, .. } => Some((x, x + w)),
                _ => None,
            })
            .collect();
        assert_eq!(boxes.len(), 2);
        let origin = doc.lay.items.iter().find_map(|item| match item.kind {
            Kind::Text { x, .. } => Some(x),
            _ => None,
        });
        let slash_x = origin.unwrap() + slash.x;
        let (before, after) = (slash_x - boxes[0].1, boxes[1].0 - (slash_x + slash.w));
        assert!(before > 2.0 && (before - after).abs() <= 1.0, "{before} before, {after} after");
    }

    #[test]
    fn a_selection_covers_the_background_of_inline_code() {
        let Some(mut eng) = engine() else { return };
        let boxes = |doc: &Doc| -> Vec<[f32; 2]> {
            let rects = doc.lay.items.iter().filter_map(|item| match item.kind {
                Kind::Rect { x, w, .. } => Some([x, x + w]),
                _ => None,
            });
            rects.collect()
        };
        let lit = |doc: &Doc, from: u32, to: u32| -> Vec<[f32; 2]> {
            let item = doc.lay.items.iter().position(|item| matches!(item.kind, Kind::Text { .. })).unwrap() as u32;
            let (from, to) = (Pos { item, offset: from }, Pos { item, offset: to });
            let rects = doc.selection_rects(from, to, 0.0, doc.height() as f32);
            rects.into_iter().map(|(_, [x, _, w, _])| [x, x + w]).collect()
        };
        let close = |a: f32, b: f32| (a - b).abs() <= 1.0;

        // "press j / k now": the code alone lights up all of its background,
        // the text before it none of it.
        let doc = layout(&mut eng, "press `j` / `k` now\n", 800);
        let (boxes_, j, press) = (boxes(&doc), lit(&doc, 6, 7), lit(&doc, 0, 6));
        assert!(close(j[0][0], boxes_[0][0]) && close(j[0][1], boxes_[0][1]), "{j:?} for {boxes_:?}");
        assert!(close(press[0][1], boxes_[0][0]), "{press:?} before {boxes_:?}");

        // Code that takes several lines: on each, the highlight reaches as
        // far as the background, which is wider than the code.
        let doc = layout(&mut eng, "see `aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm` end\n", 420);
        let (boxes_, all) = (boxes(&doc), lit(&doc, 0, u32::MAX));
        assert!(boxes_.len() >= 2 && boxes_.len() == all.len(), "{boxes_:?} {all:?}");
        for (b, l) in boxes_.iter().zip(&all) {
            assert!(l[0] <= b[0] + 1.0 && l[1] >= b[1] - 1.0, "{l:?} does not cover {b:?}");
        }
    }

    #[test]
    fn a_line_that_starts_with_inline_code_starts_with_its_background() {
        let Some(mut eng) = engine() else { return };
        let long = "`aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm nnnn oooo pppp`";
        let doc = layout(&mut eng, &format!("{long} and then\nsome more text that fills the lines to their end.\n"), 420);
        let (left, right) = (doc.lay.x, doc.lay.right);
        let boxes: Vec<[f32; 2]> = doc
            .lay
            .items
            .iter()
            .filter_map(|item| match item.kind {
                Kind::Rect { x, w, .. } => Some([x, x + w]),
                _ => None,
            })
            .collect();
        // The code takes several lines, and each begins with it.
        assert!(boxes.len() >= 2, "{boxes:?}");
        for b in &boxes {
            assert!((b[0] - left).abs() <= 1.0 && b[1] <= right + 1.0, "{b:?} in {left}..{right}");
        }
        // The code itself is further in, and found there by the pointer.
        let (x, y) = find(&doc, "aaaa");
        let item = doc.texts().next().unwrap().0 as u32;
        let first = doc.selection_rects(Pos { item, offset: 0 }, Pos { item, offset: 1 }, 0.0, 1e6);
        let [lx, _, lw, _] = first[0].1;
        assert!((lx - left).abs() <= 1.0 && x - lx > lw / 2.0 && x < lx + lw, "{x} in {lx}+{lw}");
        let hit = doc.hit(x, y).unwrap();
        assert_eq!((hit.item, hit.character, hit.inside), (item, 0, true));
        // Lines of plain text start at the edge as before, and no line is
        // longer than the column.
        for (_, _, x, buf, _) in doc.texts() {
            assert_eq!(x, left);
            for run in buf.layout_runs() {
                assert!(x + indent(buf, &run) + run.line_w <= right + 0.5, "{}", run.text);
            }
        }
    }

    #[test]
    fn blends_over_transparent() {
        let mut px = [0u8; 4];
        Canvas::blend(&mut px, Rgba(200, 100, 0, 255), 128);
        assert_eq!(px, [100, 50, 0, 128]);
        // Opaque coverage replaces the pixel.
        Canvas::blend(&mut px, Rgba(10, 20, 30, 255), 255);
        assert_eq!(px, [10, 20, 30, 255]);
    }
}
