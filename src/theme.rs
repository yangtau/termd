//! Colors.
//!
//! The page has no background of its own: it is drawn on transparent pixels
//! so the terminal's background (including its opacity or blur) shows
//! through. The other colors are tints between the terminal's background and
//! text colors, so the page fits whatever color scheme the terminal uses.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Linear blend from `self` towards `other`; `t` in 0..=1.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(f(self.0, other.0), f(self.1, other.1), f(self.2, other.2))
    }

    pub fn luma(self) -> f32 {
        (0.299 * self.0 as f32 + 0.587 * self.1 as f32 + 0.114 * self.2 as f32) / 255.0
    }

    pub fn alpha(self, a: f32) -> Rgba {
        Rgba(self.0, self.1, self.2, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    pub fn opaque(self) -> Rgba {
        Rgba(self.0, self.1, self.2, 255)
    }
}

/// Straight (non-premultiplied) color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    /// Opaque page color. `None` means transparent, which is the normal case;
    /// it is only set for PNG export and when `--theme` contradicts the
    /// terminal's real background.
    pub page: Option<Rgb>,
    pub fg: Rgba,
    /// Secondary text: labels, struck-through text, raw HTML.
    pub dim: Rgba,
    /// Text inside block quotes.
    pub quote: Rgba,
    pub link: Rgba,
    /// Background of inline code.
    pub code_bg: Rgba,
    /// Rules, table lines and the frame of code blocks.
    pub border: Rgba,
    pub quote_bar: Rgba,
    pub table_head: Rgba,
    /// Fill of a checked task box, and the check mark drawn on it.
    pub check: Rgba,
    pub check_mark: Rgba,
    /// Highlight behind selected text.
    pub select: Rgba,
}

const DARK_BG: Rgb = Rgb(0x0d, 0x11, 0x17);
const LIGHT_BG: Rgb = Rgb(0xff, 0xff, 0xff);
const DARK_FG: Rgb = Rgb(0xe6, 0xed, 0xf3);
const LIGHT_FG: Rgb = Rgb(0x1f, 0x23, 0x28);

impl Theme {
    /// `term_bg` / `term_fg` are the colors reported by the terminal (if
    /// any); `force_dark` is set when the user asked for a variant.
    pub fn new(term_bg: Option<Rgb>, term_fg: Option<Rgb>, force_dark: Option<bool>) -> Theme {
        let term_dark = term_bg.map(|bg| bg.luma() < 0.5).or(term_fg.map(|fg| fg.luma() >= 0.5));
        let dark = force_dark.or(term_dark).unwrap_or(true);
        // A forced variant can only be honored on a transparent page if the
        // terminal background agrees with it.
        let transparent = force_dark.is_none() || term_dark == force_dark;
        // Use the terminal's own text color when it suits the variant.
        let fg = match term_fg {
            Some(fg) if transparent && (fg.luma() >= 0.5) == dark => fg,
            _ if dark => DARK_FG,
            _ => LIGHT_FG,
        };
        match transparent {
            true => Theme::with(dark, None, term_bg, fg),
            false => Theme::opaque(dark),
        }
    }

    /// Theme that paints its own page background (PNG export).
    pub fn opaque(dark: bool) -> Theme {
        let bg = if dark { DARK_BG } else { LIGHT_BG };
        Theme::with(dark, Some(bg), Some(bg), if dark { DARK_FG } else { LIGHT_FG })
    }

    /// `behind` is the color the page is seen against, if known.
    fn with(dark: bool, page: Option<Rgb>, behind: Option<Rgb>, fg: Rgb) -> Theme {
        // A tint is the text color at strength `t` over the background. With
        // a known background it is mixed here into an opaque color, because
        // terminals disagree on how to blend translucent pixels (kitty blends
        // in linear light, which makes a 9% overlay look like 25%). Only when
        // the terminal did not report its background is real alpha used.
        let tint = |t: f32| match behind {
            Some(bg) => bg.mix(fg, t).opaque(),
            None => fg.alpha(t),
        };
        let link = if dark { Rgb(0x6c, 0xb6, 0xff) } else { Rgb(0x09, 0x69, 0xda) };
        Theme {
            dark,
            page,
            fg: fg.opaque(),
            dim: tint(0.58),
            quote: tint(0.8),
            link: link.opaque(),
            code_bg: tint(if dark { 0.1 } else { 0.065 }),
            border: tint(if dark { 0.2 } else { 0.14 }),
            quote_bar: tint(0.38),
            table_head: tint(if dark { 0.07 } else { 0.05 }),
            check: tint(0.34),
            check_mark: behind.unwrap_or(if dark { DARK_BG } else { LIGHT_BG }).opaque(),
            // Drawn under the text when the page is transparent (opaque
            // then, if the background is known); a veil over it otherwise.
            select: match (page, behind) {
                (None, Some(bg)) => bg.mix(link, if dark { 0.38 } else { 0.26 }).opaque(),
                _ => link.alpha(0.3),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_terminal_colors() {
        let t = Theme::new(Some(Rgb(30, 30, 46)), Some(Rgb(205, 214, 244)), None);
        assert!(t.dark);
        assert_eq!(t.page, None);
        assert_eq!(t.fg, Rgba(205, 214, 244, 255));
        // Tints are opaque mixes of the terminal's two colors.
        assert_eq!(t.code_bg, Rgb(30, 30, 46).mix(Rgb(205, 214, 244), 0.1).opaque());
    }

    #[test]
    fn forced_variant_needs_matching_background() {
        // Light page requested on a dark terminal: must paint its own background.
        let t = Theme::new(Some(Rgb(30, 30, 46)), Some(Rgb(205, 214, 244)), Some(false));
        assert!(!t.dark);
        assert_eq!(t.page, Some(LIGHT_BG));
        assert_eq!(t.fg, LIGHT_FG.opaque());
        // Same variant as the terminal: stays transparent.
        assert_eq!(Theme::new(Some(Rgb(30, 30, 46)), None, Some(true)).page, None);
    }

    #[test]
    fn unknown_terminal_defaults_to_dark() {
        let t = Theme::new(None, None, None);
        assert!(t.dark);
        assert_eq!(t.page, None);
        // Unknown background: tints fall back to translucency.
        assert_eq!(t.code_bg.3, 26);
    }
}
