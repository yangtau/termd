//! Rendering for an editor (`--serve`): the page is drawn inside one of the
//! editor's own windows instead of on a screen of termd's.
//!
//! The editor fills its window with Kitty Unicode placeholders, text cells
//! that the terminal replaces with part of an image, chosen by the cell's
//! foreground color (the image id) and two combining marks (the row and
//! column inside the image). Scrolling and redrawing are then the editor's
//! business; termd only renders the page and uploads it, in chunks of rows
//! that are images of their own.
//!
//! Commands arrive on stdin, one per line:
//!
//! ```text
//! size COLS CELL_W CELL_H     the window's width in cells, a cell in pixels
//! colors BG FG                the editor's colors, #rrggbb or -
//! text BYTES                  followed by that many bytes of Markdown
//! show FIRST LAST             the rows (0-based) the window shows, or is
//!                             about to
//! spot TAG ROW COL            what in the source the cell at ROW, COL shows
//! quit
//! ```
//!
//! and replies go to stdout, one per line:
//!
//! ```text
//! map CELL_H Y1 Y2 ... END    for a page about to be announced: the pixel
//!                             offset of the top of each source line, then
//!                             the page's height; a row is CELL_H pixels
//! doc ROWS CHUNK BASE         the page is ROWS rows long; rows from
//!                             i*CHUNK on are image BASE+i
//! G BYTES                     escape sequences to write to the terminal
//! at TAG LINE BYTE            answers `spot`: the source line (from 1; 0
//!                             for none) and the byte in it (from 0)
//! ```
//!
//! With the map the editor scrolls the page along with the source on its
//! own, without asking for every move.
//!
//! Nothing is written to the terminal directly: the editor owns it and
//! passes the `G` lines on.

use crate::doc::{Doc, Engine, LayoutOpts};
use crate::theme::{Rgb, Theme};
use crate::{base_dir, kitty};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Rows of a chunk: about this many pixels' worth.
const CHUNK_PX: u32 = 512;
/// There are 297 combining marks for the row number.
const MAX_CHUNK_ROWS: u32 = 297;
/// Image ids per process; chunk `i` is image `base + i`.
const IDS: u32 = 4096;
/// Chunks this far either side of the window are uploaded ahead of it.
const AHEAD: u32 = 2;
/// Chunks further than this from the window are freed in the terminal.
const KEEP: u32 = 8;
/// Coloring code runs in slices this long, so commands are not held up.
const WORK_SLICE: Duration = Duration::from_millis(6);

#[derive(Debug, PartialEq)]
enum Cmd {
    Size { cols: u32, cell_w: u32, cell_h: u32 },
    Colors(Option<Rgb>, Option<Rgb>),
    Text(String),
    Show(u32, u32),
    Spot { tag: String, row: u32, col: u32 },
    Quit,
}

/// One command line, except `text`, whose body follows it.
fn parse(line: &str) -> Option<Cmd> {
    let mut words = line.split_whitespace();
    let name = words.next()?;
    let tag = if name == "spot" { words.next()?.to_string() } else { String::new() };
    let mut num = || words.next()?.parse::<u32>().ok();
    let cmd = match name {
        "size" => Cmd::Size { cols: num()?.max(1), cell_w: num()?.max(1), cell_h: num()?.max(1) },
        "show" => {
            let (first, last) = (num()?, num()?);
            Cmd::Show(first.min(last), first.max(last))
        }
        "colors" => {
            let mut rest = line.split_whitespace().skip(1);
            Cmd::Colors(color(rest.next()?), color(rest.next()?))
        }
        "spot" => Cmd::Spot { tag, row: num()?, col: num()? },
        "quit" => Cmd::Quit,
        _ => return None,
    };
    Some(cmd)
}

/// `#rrggbb`, or `-` (or anything else) for none.
fn color(word: &str) -> Option<Rgb> {
    let hex = word.strip_prefix('#').filter(|h| h.len() == 6)?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Read commands from stdin until it ends.
fn read_commands(tx: mpsc::Sender<Cmd>) {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let cmd = match line.trim_end().strip_prefix("text ") {
            Some(len) => {
                let Ok(len) = len.trim().parse::<usize>() else { continue };
                let mut body = vec![0; len];
                if input.read_exact(&mut body).is_err() {
                    break;
                }
                Cmd::Text(String::from_utf8_lossy(&body).into_owned())
            }
            None => match parse(&line) {
                Some(cmd) => cmd,
                None => continue,
            },
        };
        if tx.send(cmd).is_err() {
            return;
        }
    }
    let _ = tx.send(Cmd::Quit);
}

/// Inside tmux every graphics command has to be wrapped to reach the
/// terminal (which takes `set -g allow-passthrough on`).
fn tmux_wrap(out: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(out.len() + out.len() / 16);
    for seq in out.split_inclusive(|&b| b == b'\\') {
        // A sequence ends with ESC \; a backslash elsewhere (there is none
        // in base64 or in the keys) would only split it harmlessly.
        wrapped.extend_from_slice(b"\x1bPtmux;");
        for &b in seq {
            if b == 0x1b {
                wrapped.push(0x1b);
            }
            wrapped.push(b);
        }
        wrapped.extend_from_slice(b"\x1b\\");
    }
    wrapped
}

#[derive(Clone, Copy, PartialEq)]
struct Size {
    cols: u32,
    cell_w: u32,
    cell_h: u32,
}

struct Page {
    doc: Doc,
    size: Size,
    rows: u32,
    chunk_rows: u32,
}

impl Page {
    fn chunks(&self) -> u32 {
        self.rows.div_ceil(self.chunk_rows)
    }

    fn chunk_of_y(&self, y: u32) -> u32 {
        y / (self.chunk_rows * self.size.cell_h)
    }
}

struct Server {
    path: PathBuf,
    /// The Markdown; none until the editor sends it.
    source: Option<String>,
    engine: Engine,
    zoom: f32,
    force_dark: Option<bool>,
    colors: (Option<Rgb>, Option<Rgb>),
    size: Option<Size>,
    page: Option<Page>,
    /// The text, size or colors changed since the page was laid out.
    stale: bool,
    view: (u32, u32),
    /// Chunks whose image is in the terminal, with a hash of its pixels,
    /// and those of them that show the current page.
    uploaded: BTreeMap<u32, u64>,
    current: BTreeSet<u32>,
    uploader: kitty::Uploader,
    tmux: bool,
    base: u32,
    out: std::io::Stdout,
}

pub fn run(path: &Path, engine: Engine, zoom: f32, force_dark: Option<bool>) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || read_commands(tx));
    let mut server = Server {
        path: path.to_path_buf(),
        source: None,
        engine,
        zoom,
        force_dark,
        colors: (None, None),
        size: None,
        page: None,
        stale: true,
        view: (0, 0),
        uploaded: BTreeMap::new(),
        current: BTreeSet::new(),
        uploader: uploader(),
        tmux: std::env::var_os("TMUX").is_some(),
        base: (std::process::id() % 4000 + 1) * IDS,
        out: std::io::stdout(),
    };
    let result = server.serve(&rx);
    server.free_all();
    result
}

/// How to pass images to a terminal that cannot be asked (the editor owns
/// it): through shared memory to kitty on this machine, in temporary files
/// to another terminal on this machine, inline over SSH.
///
/// Inline sequences are long, and the editor's own output can get between
/// two writes of one; the other ways keep each command to a hundred bytes.
fn uploader() -> kitty::Uploader {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    match std::env::var("TERMD_TRANSFER").as_deref() {
        Ok("shm") => kitty::Uploader::new(true),
        Ok("file") => kitty::Uploader::with_files(std::env::temp_dir()),
        Ok("inline") => kitty::Uploader::new(false),
        _ if set("SSH_CONNECTION") || set("SSH_TTY") => kitty::Uploader::new(false),
        _ if set("KITTY_WINDOW_ID") => kitty::Uploader::new(true),
        _ => kitty::Uploader::with_files(std::env::temp_dir()),
    }
}

impl Server {
    fn serve(&mut self, rx: &mpsc::Receiver<Cmd>) -> Result<()> {
        loop {
            let coloring = self.page.as_ref().is_some_and(|p| p.doc.awaits_highlight());
            let wait = if coloring { Duration::from_millis(4) } else { Duration::from_secs(3600) };
            let first = match rx.recv_timeout(wait) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            // Take everything that has arrived: a burst of typing is laid
            // out once.
            let mut queries = Vec::new();
            for cmd in first.into_iter().chain(rx.try_iter()) {
                match cmd {
                    Cmd::Quit => return Ok(()),
                    Cmd::Size { cols, cell_w, cell_h } => {
                        let size = Some(Size { cols, cell_w, cell_h });
                        self.stale |= size != self.size;
                        self.size = size;
                    }
                    Cmd::Colors(bg, fg) => {
                        self.stale |= (bg, fg) != self.colors;
                        self.colors = (bg, fg);
                    }
                    Cmd::Text(text) => {
                        if self.source.as_ref() != Some(&text) {
                            self.source = Some(text);
                            self.engine.clear_image_cache();
                            self.stale = true;
                        }
                    }
                    Cmd::Show(first, last) => self.view = (first, last),
                    query => queries.push(query),
                }
            }
            if self.stale && self.size.is_some() && self.source.is_some() {
                self.layout()?;
            }
            for query in queries {
                self.answer(query)?;
            }
            self.color_code();
            self.render_view()?;
        }
    }

    fn layout(&mut self) -> Result<()> {
        let (Some(size), Some(source)) = (self.size, &self.source) else { return Ok(()) };
        self.stale = false;
        let opts = LayoutOpts {
            page_w: size.cols * size.cell_w,
            font_px: (size.cell_h as f32 * self.zoom).max(6.0),
            base_dir: base_dir(&self.path),
            theme: Theme::new(self.colors.0, self.colors.1, self.force_dark),
            scroll_tables: false,
        };
        let mut doc = Doc::new(&mut self.engine, source, opts);
        doc.advance(&mut self.engine, None, None);
        let rows = doc.height().div_ceil(size.cell_h).max(1);
        let chunk_rows = (CHUNK_PX / size.cell_h).max(rows.div_ceil(IDS)).clamp(1, MAX_CHUNK_ROWS);
        let page = Page { doc, size, rows, chunk_rows };
        // Chunks past the end go; the others keep showing the old page
        // until they are rendered again, which beats a blank.
        let mut out = Vec::new();
        for &chunk in self.uploaded.keys().filter(|&&c| c >= page.chunks()) {
            kitty::free(&mut out, self.id(chunk));
        }
        self.uploaded.retain(|&c, _| c < page.chunks());
        self.current.clear();
        self.send(&out)?;
        let mut map = format!("map {}", size.cell_h);
        for y in page.doc.line_offsets() {
            let _ = write!(map, " {y}");
        }
        self.reply(&map)?;
        self.reply(&format!("doc {rows} {chunk_rows} {}", self.base))?;
        self.page = Some(page);
        Ok(())
    }

    fn answer(&mut self, query: Cmd) -> Result<()> {
        let Cmd::Spot { tag, row, col } = query else { return Ok(()) };
        // The middle of the cell. Without a page there is no place to name,
        // but whoever asked is waiting for an answer.
        let spot = self.page.as_ref().and_then(|page| {
            let Size { cell_w, cell_h, .. } = page.size;
            page.doc.source_at((col as f32 + 0.5) * cell_w as f32, (row as f32 + 0.5) * cell_h as f32)
        });
        let (line, byte) = spot.unwrap_or((0, 0));
        self.reply(&format!("at {tag} {line} {byte}"))
    }

    /// Color code blocks for a moment; chunks with one that changed have
    /// to be rendered again.
    fn color_code(&mut self) {
        let Some(page) = &mut self.page else { return };
        if !page.doc.awaits_highlight() {
            return;
        }
        for (top, bottom) in page.doc.highlight(&mut self.engine, Some(Instant::now() + WORK_SLICE)) {
            for chunk in page.chunk_of_y(top)..=page.chunk_of_y(bottom.saturating_sub(1)) {
                self.current.remove(&chunk);
            }
        }
    }

    /// Upload what the window shows and the chunks either side of it, the
    /// nearer ones first, and free those far away.
    fn render_view(&mut self) -> Result<()> {
        let Some(page) = &self.page else { return Ok(()) };
        let last = page.chunks() - 1;
        let (top, bottom) = ((self.view.0 / page.chunk_rows).min(last), (self.view.1 / page.chunk_rows).min(last));
        let near = (1..=AHEAD).flat_map(|d| [top.checked_sub(d), Some(bottom + d).filter(|&c| c <= last)]).flatten();
        let wanted: Vec<u32> = (top..=bottom).chain(near).filter(|c| !self.current.contains(c)).collect();
        for chunk in wanted {
            let mut out = Vec::new();
            self.render_chunk(chunk, &mut out);
            self.send(&out)?;
        }
        let far: Vec<u32> =
            self.uploaded.keys().copied().filter(|&c| c + KEEP < top || c > bottom + KEEP).collect();
        if !far.is_empty() {
            let mut out = Vec::new();
            for chunk in far {
                kitty::free(&mut out, self.id(chunk));
                self.uploaded.remove(&chunk);
                self.current.remove(&chunk);
            }
            self.send(&out)?;
        }
        Ok(())
    }

    fn render_chunk(&mut self, chunk: u32, out: &mut Vec<u8>) {
        let Some(page) = &self.page else { return };
        let Size { cols, cell_w, cell_h } = page.size;
        let first = chunk * page.chunk_rows;
        let rows = page.chunk_rows.min(page.rows - first);
        let (w, h) = (cols * cell_w, rows * cell_h);
        let rgba = page.doc.render(&mut self.engine, 0, 0, first * cell_h, w, h, false);
        self.current.insert(chunk);
        // Most of a page stays as it was when its text changes in one
        // place; the terminal already has those pixels.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (w, h, &rgba[..]).hash(&mut hasher);
        let hash = hasher.finish();
        if self.uploaded.insert(chunk, hash) == Some(hash) {
            return;
        }
        let id = self.id(chunk);
        self.uploader.transmit(out, id, w, h, &rgba);
        // A virtual placement, for the placeholders to show; uploading the
        // image again replaced the previous one.
        let _ = write!(out, "\x1b_Ga=p,U=1,q=2,i={id},p=1,c={cols},r={rows}\x1b\\");
    }

    fn id(&self, chunk: u32) -> u32 {
        self.base + chunk % IDS
    }

    fn free_all(&mut self) {
        let mut out = Vec::new();
        for &chunk in self.uploaded.keys() {
            kitty::free(&mut out, self.id(chunk));
        }
        self.uploaded.clear();
        let _ = self.send(&out);
    }

    /// Escape sequences for the editor to pass to the terminal.
    fn send(&mut self, seqs: &[u8]) -> Result<()> {
        if seqs.is_empty() {
            return Ok(());
        }
        let wrapped;
        let seqs = if self.tmux {
            wrapped = tmux_wrap(seqs);
            &wrapped[..]
        } else {
            seqs
        };
        let mut out = self.out.lock();
        out.write_all(b"G ")?;
        out.write_all(seqs)?;
        out.write_all(b"\n")?;
        out.flush()?;
        Ok(())
    }

    fn reply(&mut self, line: &str) -> Result<()> {
        let mut out = self.out.lock();
        writeln!(out, "{line}")?;
        out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commands() {
        assert_eq!(parse("size 80 10 21\n"), Some(Cmd::Size { cols: 80, cell_w: 10, cell_h: 21 }));
        assert_eq!(parse("show 40 12"), Some(Cmd::Show(12, 40)));
        assert_eq!(parse("colors #1e1e2e -"), Some(Cmd::Colors(Some(Rgb(0x1e, 0x1e, 0x2e)), None)));
        assert_eq!(parse("spot a 3 14"), Some(Cmd::Spot { tag: "a".into(), row: 3, col: 14 }));
        assert_eq!(parse("spot a 3"), None);
        assert_eq!(parse("quit"), Some(Cmd::Quit));
        assert_eq!(parse("size 80"), None);
        assert_eq!(parse("dance"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn wraps_each_sequence_for_tmux() {
        let out = b"\x1b_Ga=t,i=1;AAAA\x1b\\\x1b_Ga=p,U=1,i=1\x1b\\";
        assert_eq!(
            tmux_wrap(out),
            b"\x1bPtmux;\x1b\x1b_Ga=t,i=1;AAAA\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b_Ga=p,U=1,i=1\x1b\x1b\\\x1b\\".to_vec()
        );
    }
}
