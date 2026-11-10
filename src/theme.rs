//! Colors. Everything is derived from the terminal background so the rendered
//! page blends into whatever color scheme the terminal uses.

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
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    pub bg: Rgb,
    pub fg: Rgb,
    pub dim: Rgb,
    pub link: Rgb,
    pub code_bg: Rgb,
    pub border: Rgb,
    pub quote_bar: Rgb,
    pub table_head: Rgb,
}

impl Theme {
    /// `term_bg` is the background reported by the terminal (if any);
    /// `dark` forces the variant when the user asked for one explicitly.
    pub fn new(term_bg: Option<Rgb>, dark: Option<bool>) -> Theme {
        let dark = dark.unwrap_or_else(|| term_bg.is_none_or(|bg| bg.luma() < 0.5));
        let default_bg = if dark { Rgb(0x0d, 0x11, 0x17) } else { Rgb(0xff, 0xff, 0xff) };
        // Only trust the terminal background if it agrees with the variant.
        let bg = match term_bg {
            Some(bg) if (bg.luma() < 0.5) == dark => bg,
            _ => default_bg,
        };
        let fg = if dark { Rgb(0xe6, 0xed, 0xf3) } else { Rgb(0x1f, 0x23, 0x28) };
        let link = if dark { Rgb(0x58, 0xa6, 0xff) } else { Rgb(0x09, 0x69, 0xda) };
        Theme {
            dark,
            bg,
            fg,
            link,
            dim: bg.mix(fg, 0.62),
            code_bg: bg.mix(fg, 0.09),
            border: bg.mix(fg, 0.22),
            quote_bar: bg.mix(fg, 0.28),
            table_head: bg.mix(fg, 0.07),
        }
    }
}
