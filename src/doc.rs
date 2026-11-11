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
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent,
    SwashImage, Weight, Wrap,
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
const META_LINK_SHIFT: u32 = 8;

const HEADING_SCALE: [f32; 6] = [1.75, 1.35, 1.15, 1.0, 0.9, 0.85];
const BODY_LINE: f32 = 1.65;
const HEADING_LINE: f32 = 1.3;
const CODE_SCALE: f32 = 0.88;
const CODE_LINE: f32 = 1.55;
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
}

pub struct Doc {
    /// Width of the page image in pixels.
    pub width: u32,
    /// Where the left edge of the image sits on the page.
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

/// What is at a point of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The text item nearest to the point.
    pub item: u32,
    /// Offset in the item's text of the character boundary nearest to it.
    pub offset: u32,
    /// Whether the point is on one of the item's characters, rather than
    /// beside or between its lines.
    pub inside: bool,
    /// The link the character under the point belongs to.
    pub link: Option<u32>,
}

struct Item {
    y: f32,
    h: f32,
    /// The part of the source this was made from.
    src: Range<u32>,
    kind: Kind,
}

/// What makes a text item part of the document's content (as opposed to
/// decoration such as list numbers and labels).
struct Content {
    map: TextMap,
    /// Offset in the text at which each line of the buffer starts.
    line_starts: Vec<u32>,
    /// What separates this text from the one before it when both are copied.
    join: Join,
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
    buf.set_size(width, None);

    // Every run names its family and a weight that family has, so
    // cosmic-text never needs to go looking for a fallback font.
    let attrs = |cjk: bool, face: Face, weight: u16| {
        let fam = fonts.family(face, cjk);
        Attrs::new().family(Family::Name(&fam.name)).weight(Weight(fam.weight(weight)))
    };
    let base = attrs(false, st.face, st.weight);
    let mut rich = Vec::new();
    for s in spans {
        let weight = if s.bold { BOLD.max(st.weight) } else { st.weight };
        let face = if s.code { Face::Mono } else { st.face };
        for (piece, cjk) in script_runs(&s.text) {
            let mut a = attrs(cjk, face, weight);
            if s.italic {
                a = a.style(Style::Italic);
            }
            let flag = |on: bool, flag: usize| if on { flag } else { 0 };
            let link = s.link.map_or(0, |id| (id as usize + 1) << META_LINK_SHIFT | META_LINK);
            a = a.metadata(flag(s.code, META_CODE) | flag(s.strike, META_STRIKE) | link);
            if s.code {
                a = a.metrics(Metrics::new(st.size * CODE_SCALE, st.line_h));
            }
            if let Some(c) = s.color {
                a = a.color(color(c));
            }
            rich.push((piece, a));
        }
    }
    buf.set_rich_text(rich, &base, Shaping::Advanced, None);
    buf.shape_until_scroll(&mut eng.fs, false);
    buf
}

/// What a content text item needs to know about its text: `map` says where
/// it comes from.
fn content(buf: &Buffer, map: TextMap, join: Join) -> Option<Box<Content>> {
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
    Some(Box::new(Content { map, line_starts, join }))
}

/// The map of text shaped from `spans`.
fn span_map(spans: &[Span]) -> TextMap {
    TextMap::from_pieces(spans.iter().map(|s| (s.text.len(), s.src.clone())))
}

/// (widest line, total height) of a shaped buffer.
fn measure(buf: &Buffer) -> (f32, f32) {
    buf.layout_runs().fold((0.0, 0.0), |(w, h): (f32, f32), run| {
        (w.max(run.line_w), h.max(run.line_top + run.line_height))
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
                x: bleed,
                right: bleed + content_w,
                y: top,
                margin: 0.0,
                first: true,
                stable: 0.0,
                source: markdown.to_string(),
                at: 0..0,
                covered: 0,
                items: Vec::new(),
                links: Vec::new(),
                uncolored: VecDeque::new(),
                spans: Vec::new(),
                bold: 0,
                italic: 0,
                strike: 0,
                link: None,
                heading: None,
                lists: Vec::new(),
                marker: None,
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
        let offset = content.map.from_source(anchor.src.max(content.map.range().start));
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
    #[cfg(test)]
    pub fn offset_of_line(&mut self, eng: &mut Engine, line: u32) -> u32 {
        self.resolve(eng, Anchor { src: self.start_of_line(line), frac: 0.0 })
    }

    /// Source offset at which 1-based `line` starts.
    pub fn start_of_line(&self, line: u32) -> u32 {
        self.lines.start_of(line)
    }

    /// What is at the point (`x`, `y`) of the page image.
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        // The nearest text, vertically first: text beside the point on its
        // line wins over text right above or below it.
        let distance = |lo: f32, hi: f32, v: f32| (lo - v).max(v - hi).max(0.0);
        let (index, item, item_x, buf, content) = self.texts().min_by(|a, b| {
            let key = |(_, item, left, buf, _): &(usize, &Item, f32, &Buffer, &Content)| {
                let width = buf.size().0.unwrap_or(0.0);
                (distance(item.y, item.y + item.h, y), distance(*left, left + width, x))
            };
            let (ka, kb) = (key(a), key(b));
            ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
        })?;
        let (lx, ly) = (x - item_x, y - item.y);
        // The line at that height, or the nearest one.
        let run = buf
            .layout_runs()
            .min_by(|a, b| {
                let d = |r: &cosmic_text::LayoutRun| distance(r.line_top, r.line_top + r.line_height, ly);
                d(a).total_cmp(&d(b))
            })?;
        let on_line = ly >= run.line_top && ly < run.line_top + run.line_height;
        let line_start = content.line_starts.get(run.line_i).copied().unwrap_or(0);
        let under = run.glyphs.iter().find(|g| lx >= g.x && lx < g.x + g.w);
        let (in_line, link) = match under {
            Some(g) => {
                let link = (g.metadata >> META_LINK_SHIFT).checked_sub(1).map(|id| id as u32);
                (if lx < g.x + g.w / 2.0 { g.start } else { g.end }, link)
            }
            // Beside the line: its nearer end.
            None => {
                let before = run.glyphs.first().is_none_or(|g| lx < g.x);
                let end = match before {
                    true => run.glyphs.iter().map(|g| g.start).min(),
                    false => run.glyphs.iter().map(|g| g.end).max(),
                };
                (end.unwrap_or(0), None)
            }
        };
        Some(Hit {
            item: index as u32,
            offset: line_start + in_line as u32,
            inside: on_line && under.is_some(),
            link,
        })
    }

    /// Destination of link number `id`.
    pub fn link(&self, id: u32) -> Option<&str> {
        self.lay.links.get(id as usize).map(String::as_str)
    }

    /// 1-based source line of the character at `offset` of text item `item`.
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

    /// Add a shape or a piece of decoration, made from the source the
    /// parser is at.
    fn push(&mut self, y: f32, h: f32, kind: Kind) {
        self.items.push(Item { y, h, src: self.at.clone(), kind });
    }

    /// Add text that is document content; `map` says where it comes from.
    fn push_text(&mut self, x: f32, y: f32, h: f32, buf: Buffer, color: Rgba, map: TextMap, join: Join) {
        let src = map.range();
        self.covered = self.covered.max(src.end);
        let content = content(&buf, map, join);
        self.items.push(Item { y, h, src, kind: Kind::Text { x, buf, color, content } });
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

        self.code_boxes(&buf, self.x, y);
        let color = if self.heading == Some(6) { self.theme.dim } else { self.text_color() };
        let join = if tight { Join::Line } else { Join::Block };
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
        let code_h = (size * 1.32).round();
        let pad = (size * 0.2).round();
        let mut boxes: Vec<(f32, f32, f32)> = Vec::new(); // x0, x1, top
        for run in buf.layout_runs() {
            let top = run.line_top + ((run.line_height - code_h) / 2.0).round();
            let mut open: Option<(f32, f32)> = None;
            for g in run.glyphs {
                if g.metadata & META_CODE != 0 {
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
            let (bx, w) = ((x + x0 - pad).round(), (x1 - x0 + 2.0 * pad).round());
            self.rect(bx, y + top, w, code_h, size * 0.28, self.theme.code_bg);
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
            Some(Some(tokens)) => self.mono_block(eng, &code_spans(&tokens, &self.theme), self.theme.fg, lang, map),
            Some(None) => self.mono_block(eng, &plain(), self.theme.fg, lang, map),
            None => {
                self.mono_block(eng, &plain(), self.theme.fg, lang, map);
                let width = self.right - self.x - 2.0 * code_pad(self.em).0;
                self.uncolored.push_back(Uncolored {
                    item: self.items.len() - 1,
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
        self.mono_block(eng, &[span], self.theme.dim, "", map);
    }

    /// A monospace block in a rounded frame (code blocks, raw HTML), with
    /// `label` (the language) in its top right corner. Its text is the last
    /// item pushed.
    fn mono_block(&mut self, eng: &mut Engine, spans: &[Span], fg: Rgba, label: &str, map: TextMap) {
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
        if !label.is_empty() {
            let span = Span { text: label.to_string(), ..Span::default() };
            let tag = make_buffer(eng, &[span], &label_style(em), None);
            let tag_w = measure(&tag).0;
            // Left out when the first line of code reaches under it.
            let first_line = buf.layout_runs().next().map_or(0.0, |run| run.line_w);
            if first_line + em <= width - 2.0 * pad_x - tag_w {
                let (x, y) = (self.right - pad_x - tag_w, y + (0.55 * em).round());
                self.push_label(x, y, label_style(em).line_h, tag, self.theme.dim);
            }
        }
        self.push_text(self.x + pad_x, y + pad_y, text_h, buf, fg, map, Join::Block);
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

        // Pass 1: natural (unwrapped) width of every column.
        let mut cells: Vec<Vec<(Buffer, TextMap)>> = Vec::new();
        let mut natural = vec![0.0f32; cols];
        for (r, row) in table.rows.iter().enumerate() {
            let style = if r < table.head_rows { &head_st } else { &st };
            let mut bufs = Vec::new();
            for (c, spans) in row.iter().enumerate() {
                let buf = make_buffer(eng, spans, style, None);
                natural[c] = natural[c].max(measure(&buf).0.ceil() + 1.0);
                bufs.push((buf, span_map(spans)));
            }
            cells.push(bufs);
        }

        let avail = self.right - self.x;
        let chrome = cols as f32 * 2.0 * pad_x;
        let room = (avail - chrome).max(cols as f32 * em);
        let widths = fit_columns(&natural, room, em);

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
                buf.set_size(Some(widths[c]), None);
                buf.shape_until_scroll(&mut eng.fs, false);
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
        if table.head_rows > 0 {
            let radii = [radius, radius, 0.0, 0.0];
            self.push(y, head_h, Kind::Rect { x: self.x, w: table_w, radii, color: self.theme.table_head });
        }
        let color = self.text_color();
        let mut cy = y + line;
        for (r, row) in cells.into_iter().enumerate() {
            if r > 0 {
                // The rule under the header is twice as heavy.
                let weight = if r == table.head_rows { 2.0 * line } else { line };
                self.rect(self.x, cy - line, table_w, weight, 0.0, self.theme.border);
            }
            let mut cx = self.x;
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
        let outline = Kind::Outline { x: self.x, w: table_w, radius, line, color: self.theme.border };
        self.push(y, table_h, outline);
    }
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

/// Column widths for a table whose columns would like `natural` widths but
/// only have `room` in total. Columns narrower than an equal share keep
/// their width, so a short column is never squeezed because of a long one;
/// the others share what is left in proportion to their content.
fn fit_columns(natural: &[f32], room: f32, min: f32) -> Vec<f32> {
    let mut widths = natural.to_vec();
    let mut flexible: Vec<usize> = (0..natural.len()).collect();
    let mut room = room;
    loop {
        let wanted: f32 = flexible.iter().map(|&c| natural[c]).sum();
        if wanted <= room || flexible.is_empty() {
            return widths;
        }
        let share = room / flexible.len() as f32;
        let (fits, wide): (Vec<usize>, Vec<usize>) = flexible.iter().partition(|&&c| natural[c] <= share);
        if fits.is_empty() {
            for c in wide {
                widths[c] = (natural[c] * room / wanted).floor().max(min);
            }
            return widths;
        }
        room -= fits.iter().map(|&c| natural[c]).sum::<f32>();
        flexible = wide;
    }
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
    /// Rasterize the strip `[y0, y0 + height)` of the page as straight RGBA.
    pub fn render(&self, eng: &mut Engine, y0: u32, height: u32) -> Vec<u8> {
        let (w, h) = (self.width.max(1), height.max(1));
        let mut pm = Pixmap::new(w, h).expect("non-zero pixmap size");
        let theme = &self.lay.theme;
        if let Some(bg) = theme.page {
            pm.fill(tiny_skia::Color::from_rgba8(bg.0, bg.1, bg.2, 255));
        }

        let ink = ink(theme.dark);
        let (top, bottom) = (y0 as f32, (y0 + height) as f32);
        // How far outside its line box a glyph may paint.
        let slack = self.lay.em;
        let shift = Transform::from_translate(0.0, -top);
        for item in &self.lay.items {
            if item.y - slack >= bottom || item.y + item.h + slack <= top {
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
                        pm.stroke_path(&path, &paint(theme.check_mark), &stroke, shift, None);
                    }
                }
                Kind::Image { x, img } => {
                    let mut canvas = Canvas { data: pm.data_mut(), w: w as i32, h: h as i32, ink: &ink };
                    canvas.image(x.round() as i32, (item.y - top).round() as i32, img);
                }
                Kind::Text { x, buf, color: default, .. } => {
                    let mut canvas = Canvas { data: pm.data_mut(), w: w as i32, h: h as i32, ink: &ink };
                    let (ox, oy) = (x.round() as i32, (item.y - top).round() as i32);
                    for run in buf.layout_runs() {
                        // Skip lines outside the strip: a long code block
                        // spans many tiles but each needs only a few lines.
                        let line_top = item.y + run.line_top;
                        if line_top - slack >= bottom || line_top + run.line_height + slack <= top {
                            continue;
                        }
                        for g in run.glyphs {
                            let pg = g.physical((0.0, run.line_y), 1.0);
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
                                let (x0, mut x1) = (g.x, g.x + g.w);
                                while let Some(next) = glyphs.next_if(|n| n.metadata & flag != 0) {
                                    x1 = next.x + next.w;
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

        let mut data = pm.take();
        if theme.page.is_none() {
            unpremultiply(&mut data);
        }
        data
    }

    /// Render the whole document onto an opaque page `page_w` pixels wide,
    /// for PNG export.
    pub fn render_page(&mut self, eng: &mut Engine, page_w: u32, bg: Rgb) -> RgbaImage {
        self.advance(eng, None, None);
        self.highlight(eng, None);
        let (w, h) = (self.width, self.height.max(1));
        let strip = self.render(eng, 0, h);
        let page_w = page_w.max(w);
        let mut page = RgbaImage::from_pixel(page_w, h, image::Rgba([bg.0, bg.1, bg.2, 255]));
        let x0 = ((page_w - w) / 2) as usize;
        for (y, row) in strip.chunks_exact(w as usize * 4).enumerate() {
            let start = (y * page_w as usize + x0) * 4;
            page.as_mut()[start..start + row.len()].copy_from_slice(row);
        }
        page
    }
}

/// Premultiplied → straight alpha, which is what the Kitty protocol takes.
fn unpremultiply(data: &mut [u8]) {
    // Only translucent pixels change, and those are just glyph and shape
    // edges; whole blocks of transparent or opaque pixels are skipped with a
    // check the compiler can vectorize.
    for block in data.chunks_mut(64 * 4) {
        let translucent = |px: &[u8]| px[3].wrapping_sub(1) < 254;
        if !block.chunks_exact(4).fold(false, |any, px| any | translucent(px)) {
            continue;
        }
        for px in block.chunks_exact_mut(4).filter(|px| translucent(px)) {
            let a = px[3] as u32;
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
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
        let opts = LayoutOpts { page_w, font_px: 16.0, base_dir: PathBuf::from("."), theme };
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
                    return (x + g.x + g.w / 2.0, item.y + run.line_top + run.line_height / 2.0);
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
    fn reads_front_matter() {
        let rows = front_matter_rows("title: \"Hello: world\"\ntags:\n  - a\n  - b\n\n作者: 某人\n# comment\n");
        let expect = [("Title", "Hello: world"), ("Tags", "- a\n- b"), ("作者", "某人\n# comment")];
        assert_eq!(rows, expect.map(|(k, v)| (k.to_string(), v.to_string())));
        assert!(front_matter_rows("just some text\n").is_empty());
    }

    #[test]
    fn short_columns_keep_their_width() {
        // Everything fits: untouched.
        assert_eq!(fit_columns(&[40.0, 60.0], 200.0, 10.0), vec![40.0, 60.0]);
        // One long column: only it shrinks.
        assert_eq!(fit_columns(&[40.0, 50.0, 900.0], 400.0, 10.0), vec![40.0, 50.0, 310.0]);
        // Two long columns share the rest in proportion.
        assert_eq!(fit_columns(&[40.0, 600.0, 300.0], 400.0, 10.0), vec![40.0, 240.0, 120.0]);
        // Nothing fits an equal share: all shrink in proportion, down to the minimum.
        assert_eq!(fit_columns(&[300.0, 100.0], 40.0, 16.0), vec![30.0, 16.0]);
    }

    #[test]
    fn unpremultiplies() {
        let mut px = [50, 25, 0, 100, 9, 9, 9, 255, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 100, 9, 9, 9, 255, 0, 0, 0, 0]);
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
