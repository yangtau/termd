//! Mapping between what is on the page and the Markdown source.
//!
//! Every piece of text on the page remembers where in the source it came
//! from (`TextMap`), so a pixel can be traced to a source line (to open an
//! editor there), a selection to the Markdown behind it, and a reading
//! position can be found again after the page was laid out anew.

use std::ops::Range;

/// Line starts of a source text, for converting byte offsets to line numbers.
pub struct Lines {
    /// Byte offset at which each line starts; the first is 0.
    starts: Vec<u32>,
    len: u32,
}

impl Lines {
    pub fn new(source: &str) -> Lines {
        let starts = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i as u32 + 1))
            .collect();
        Lines { starts, len: source.len() as u32 }
    }

    /// 1-based line number containing byte `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.starts.partition_point(|&start| start <= offset) as u32
    }

    /// How many lines there are, the way an editor counts them: a line
    /// break at the very end does not start another line.
    pub fn count(&self) -> u32 {
        let ends_with_break = self.starts.len() > 1 && self.starts.last() == Some(&self.len);
        (self.starts.len() - ends_with_break as usize) as u32
    }

    /// Byte offset at which 1-based `line` starts (the end of the text for
    /// lines past the last).
    pub fn start_of(&self, line: u32) -> u32 {
        self.starts.get(line.saturating_sub(1) as usize).copied().unwrap_or(self.len)
    }

    /// Byte range of the whole lines that `range` touches, without the final
    /// line break.
    pub fn whole_lines(&self, range: Range<u32>) -> Range<u32> {
        let last = self.line_of(range.end.saturating_sub(1).max(range.start));
        // The line after the last one starts right behind its line break.
        let end = self.starts.get(last as usize).map_or(self.len, |next| next - 1);
        self.start_of(self.line_of(range.start))..end
    }
}

/// Where each stretch of a text item's characters comes from in the source.
///
/// A stretch is a run of text that maps to the source character by
/// character (one inline event, or one line of a code block). Text that the
/// parser changed (an entity, an escape, the space a line break turned into)
/// simply maps to the nearest end of its stretch.
#[derive(Default)]
pub struct TextMap {
    /// (offset in the item's text, source range), ordered by both.
    stretches: Vec<(u32, Range<u32>)>,
}

impl TextMap {
    /// A map for text assembled from pieces, given as (length of the piece
    /// in the text, its source range).
    pub fn from_pieces(pieces: impl IntoIterator<Item = (usize, Range<u32>)>) -> TextMap {
        let mut at = 0;
        let stretches = pieces
            .into_iter()
            .map(|(len, src)| {
                let start = at;
                at += len as u32;
                (start, src)
            })
            .collect();
        TextMap { stretches }
    }

    /// A map for text whose lines are the lines of `source[range]`, each
    /// possibly without a prefix: a code block inside a list or a quote
    /// loses its indentation or `> ` on every line.
    pub fn from_lines(text: &str, source: &str, range: Range<u32>) -> TextMap {
        let mut src_at = range.start;
        let mut text_at = 0u32;
        let mut src_lines = source[range.start as usize..range.end as usize].split('\n');
        let mut stretches = Vec::new();
        for line in text.split('\n') {
            // Lines the source no longer has (cannot happen for text taken
            // from it) map to its end.
            let src_line = src_lines.next().unwrap_or("");
            let src_len = src_line.trim_end_matches('\r').len() as u32;
            let len = (line.len() as u32).min(src_len);
            stretches.push((text_at, src_at + src_len - len..src_at + src_len));
            text_at += line.len() as u32 + 1;
            src_at = (src_at + src_line.len() as u32 + 1).min(range.end);
        }
        TextMap { stretches }
    }

    /// The source range all of the text comes from.
    pub fn range(&self) -> Range<u32> {
        match (self.stretches.first(), self.stretches.last()) {
            (Some((_, first)), Some((_, last))) => first.start..last.end.max(first.start),
            _ => 0..0,
        }
    }

    /// Source offset of the character at `offset` in the text.
    pub fn to_source(&self, offset: u32) -> u32 {
        let i = self.stretches.partition_point(|(start, _)| *start <= offset);
        match i.checked_sub(1).map(|i| &self.stretches[i]) {
            Some((start, src)) => src.start + (offset - start).min(src.end - src.start),
            None => self.range().start,
        }
    }

    /// Offset in the text of the character at source offset `src`; for a
    /// position between stretches (markup), the start of the next one.
    pub fn to_text(&self, src: u32) -> u32 {
        let i = self.stretches.partition_point(|(_, range)| range.end <= src);
        match self.stretches.get(i) {
            Some((start, range)) => start + src.saturating_sub(range.start),
            // Past the end: the end of the last stretch.
            None => self.stretches.last().map_or(0, |(start, range)| start + (range.end - range.start)),
        }
    }
}

/// The line of `new` that corresponds to 1-based `line` of `old`, for
/// restoring a position after the file was edited: the nearest line with
/// the same text, or the same line number if the text is gone.
pub fn relocate(old: &str, new: &str, line: u32) -> u32 {
    let index = line.saturating_sub(1) as usize;
    let new_lines: Vec<&str> = new.lines().collect();
    // A blank or very short line occurs everywhere; look for the first
    // distinctive line at or after the wanted one and keep the distance.
    let wanted = old.lines().enumerate().skip(index).find(|(_, text)| text.trim().len() >= 4);
    let Some((old_index, text)) = wanted else { return line.min(new_lines.len().max(1) as u32) };
    let nearest = new_lines
        .iter()
        .enumerate()
        .filter(|(_, candidate)| **candidate == text)
        .min_by_key(|(i, _)| i.abs_diff(old_index));
    let found = match nearest {
        Some((new_index, _)) => new_index.saturating_sub(old_index - index),
        None => index,
    };
    (found.min(new_lines.len().saturating_sub(1)) + 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers() {
        let lines = Lines::new("one\ntwo\n\nfour");
        assert_eq!([0, 3, 4, 7, 8, 9, 12].map(|o| lines.line_of(o)), [1, 1, 2, 2, 3, 4, 4]);
        assert_eq!([1, 2, 3, 4, 5].map(|l| lines.start_of(l)), [0, 4, 8, 9, 13]);
        assert_eq!(lines.count(), 4);
        assert_eq!(["", "\n", "a\nb", "a\nb\n", "a\n\n"].map(|text| Lines::new(text).count()), [1, 1, 2, 2, 2]);
    }

    #[test]
    fn whole_lines_of_a_range() {
        let source = "one\ntwo\nthree\n";
        let lines = Lines::new(source);
        let text = |r: Range<u32>| &source[r.start as usize..r.end as usize];
        assert_eq!(text(lines.whole_lines(5..6)), "two");
        assert_eq!(text(lines.whole_lines(1..9)), "one\ntwo\nthree");
        assert_eq!(text(lines.whole_lines(4..8)), "two");
        let unterminated = Lines::new("a\nbc");
        assert_eq!(unterminated.whole_lines(3..4), 2..4);
    }

    #[test]
    fn maps_pieces() {
        // "Hello " (0..6), then "world" from inside `**world**` (8..13).
        let map = TextMap::from_pieces([(6, 0..6), (5, 8..13)]);
        assert_eq!(map.range(), 0..13);
        assert_eq!([0, 5, 6, 10, 11].map(|o| map.to_source(o)), [0, 5, 8, 12, 13]);
        assert_eq!([0, 5, 8, 12].map(|s| map.to_text(s)), [0, 5, 6, 10]);
        // Inside the markup between the pieces: the start of the next piece.
        assert_eq!(map.to_text(7), 6);
        assert_eq!(map.to_text(99), 11);
    }

    #[test]
    fn pieces_the_parser_changed_map_to_their_ends() {
        // "&" from "&amp;" (source 2..7), and "a " whose space was a line break.
        let map = TextMap::from_pieces([(1, 2..7), (2, 7..8)]);
        assert_eq!(map.to_source(0), 2);
        assert_eq!(map.to_source(1), 7);
        assert_eq!(map.to_source(2), 8);
    }

    #[test]
    fn maps_code_lines_with_stripped_prefixes() {
        let source = "- item\n\n  ```\n  let a = 1;\n\n    indented\n  ```\n";
        let start = source.find("  let").unwrap() as u32;
        let end = (source.find("    indented").unwrap() + "    indented".len()) as u32;
        let text = "let a = 1;\n\n  indented";
        let map = TextMap::from_lines(text, source, start..end);
        let lines = Lines::new(source);
        // Every character maps to the same character on the right line.
        for (offset, ch) in text.char_indices().filter(|(_, c)| *c != '\n') {
            let src = map.to_source(offset as u32) as usize;
            assert_eq!(source[src..].chars().next(), Some(ch), "offset {offset}");
            assert_eq!(map.to_text(src as u32), offset as u32);
        }
        assert_eq!(lines.line_of(map.to_source(0)), 4);
        assert_eq!(lines.line_of(map.to_source(11)), 5);
        assert_eq!(lines.line_of(map.to_source(14)), 6);
    }

    #[test]
    fn relocates_lines_after_an_edit() {
        let old = "# Title\n\nfirst paragraph\n\nsecond paragraph\n\nthird paragraph\n";
        // Two lines added at the top.
        let new = format!("new line\n\n{old}");
        assert_eq!(relocate(old, &new, 5), 7);
        // A blank line follows the next distinctive line's offset.
        assert_eq!(relocate(old, &new, 4), 6);
        // The line itself was rewritten: stay at the same number.
        let rewritten = old.replace("second paragraph", "2nd");
        assert_eq!(relocate(old, &rewritten, 5), 5);
        // Text removed above.
        let shorter = old.replace("first paragraph\n\n", "");
        assert_eq!(relocate(old, &shorter, 5), 3);
        // Past the end of a shorter file.
        assert_eq!(relocate(old, "only\n", 7), 1);
        assert_eq!(relocate("", "", 1), 1);
    }

    #[test]
    fn relocate_prefers_the_nearest_duplicate() {
        let old = "same line here\nother text\nsame line here\n";
        assert_eq!(relocate(old, old, 3), 3);
        assert_eq!(relocate(old, old, 1), 1);
    }
}
