//! Font selection.
//!
//! cosmic-text's own fallback is not usable for mixed CJK/Latin text: it only
//! accepts a fallback face whose weight matches exactly, so bold Chinese text
//! (PingFang has no weight-700 face) ends up being served glyph by glyph from
//! whichever installed fonts happen to be bold. It is also slow, because every
//! distinct text style makes it reopen every installed font file.
//!
//! So termd picks the families itself. The system is scanned once (metadata
//! only), and the `FontSystem` is given a small database holding just the
//! chosen Latin, monospace and CJK families. Each text run is then assigned a
//! family and a weight that family really has (see `Family::weight`). Other
//! families are copied in only when the document contains characters the
//! chosen ones cannot draw.

use cosmic_text::fontdb::{Database, FaceInfo, Style};
use cosmic_text::skrifa::{FontRef, MetadataProvider, Tag};
use cosmic_text::{FontSystem, PlatformFallback};
use std::collections::HashSet;
use std::process::Stdio;

const SANS: &[&str] = &[
    // macOS: the system UI font, then the classic fallbacks.
    ".SF NS",
    "SF Pro Text",
    "Helvetica Neue",
    // Linux / generic.
    "Inter",
    "Noto Sans",
    "Open Sans",
    "Roboto",
    "Ubuntu",
    "Cantarell",
    "Source Sans 3",
    "Liberation Sans",
    "DejaVu Sans",
    "Helvetica",
    "Arial",
];

const MONO: &[&str] = &[
    "SF Mono",
    ".SF NS Mono",
    "Menlo",
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "Source Code Pro",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Ubuntu Mono",
    "Monaco",
    "Courier New",
];

const CJK_SC: &[&str] = &[
    "PingFang SC",
    ".PingFang SC",
    "Hiragino Sans GB",
    "Heiti SC",
    "STHeiti",
    "Noto Sans CJK SC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "Noto Sans SC",
    "Sarasa Gothic SC",
    "Microsoft YaHei",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
    "Droid Sans Fallback",
];
const CJK_TC: &[&str] = &[
    "PingFang TC",
    ".PingFang TC",
    "Heiti TC",
    "Noto Sans CJK TC",
    "Source Han Sans TC",
    "Source Han Sans TW",
    "Noto Sans TC",
];
const CJK_HK: &[&str] = &["PingFang HK", ".PingFang HK", "Noto Sans CJK HK", "Source Han Sans HK", "Noto Sans HK"];
const CJK_JA: &[&str] = &[
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Noto Sans CJK JP",
    "Source Han Sans",
    "Source Han Sans JP",
    "Noto Sans JP",
    "IPAGothic",
];
const CJK_KO: &[&str] = &[
    "Apple SD Gothic Neo",
    "Noto Sans CJK KR",
    "Source Han Sans K",
    "Source Han Sans KR",
    "Noto Sans KR",
    "NanumGothic",
];

/// Tried first, in this order, for characters the main families lack.
const EXTRA: &[&str] = &[
    "Apple Color Emoji",
    "Noto Color Emoji",
    "Twemoji",
    "Apple Symbols",
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "Noto Sans Symbols2",
    "Noto Sans Math",
    "Menlo",
    "DejaVu Sans",
    "Arial Unicode MS",
    "Symbola",
];

/// A font family and the weights it can render.
pub struct Family {
    pub name: String,
    /// Weights of the upright static faces, ascending.
    weights: Vec<u16>,
    /// Range of the `wght` axis, for variable fonts.
    variable: Option<(u16, u16)>,
}

impl Family {
    /// The weight to request for `want`: the wanted weight itself if the
    /// family has it, otherwise the nearest one it does have (heavier on a
    /// tie, so "semibold" becomes bold rather than medium).
    pub fn weight(&self, want: u16) -> u16 {
        if let Some((lo, hi)) = self.variable {
            return want.clamp(lo, hi);
        }
        self.weights
            .iter()
            .copied()
            .min_by_key(|&w| (w.abs_diff(want), u16::MAX - w))
            .unwrap_or(want)
    }
}

pub struct Fonts {
    pub sans: Family,
    pub mono: Family,
    /// Used for CJK characters in both body text and code.
    pub cjk: Option<Family>,
    /// Every installed face; only metadata, the files are not kept open.
    all: Database,
    /// Families already copied into the `FontSystem`.
    loaded: Vec<String>,
    /// Characters already checked for coverage.
    checked: HashSet<char>,
}

/// What `scan` found on this machine.
pub struct Installed {
    db: Database,
    /// The system's configured default families (sans, monospace, CJK),
    /// where there is a way to ask.
    defaults: [Option<String>; 3],
    /// How long the scan took, for `TERMD_DEBUG`.
    pub took: std::time::Duration,
}

impl Installed {
    pub fn faces(&self) -> usize {
        self.db.len()
    }
}

/// Index the installed fonts. Slow (tens of milliseconds, more on machines
/// with many fonts), so `main` runs it on a thread while the terminal is
/// being probed.
pub fn scan() -> Installed {
    let start = std::time::Instant::now();
    // On Linux and the BSDs fontconfig knows which fonts the user prefers.
    // The three questions run as concurrent processes while we scan.
    let cjk_pattern = format!("sans-serif:lang={}", cjk_lang().1);
    let asked = ["sans-serif", "monospace", &cjk_pattern].map(|pattern| {
        if cfg!(target_os = "macos") {
            return None;
        }
        std::process::Command::new("fc-match")
            .args(["-f", "%{family[0]}", pattern])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    });

    let mut db = Database::new();
    db.load_system_fonts();
    // Since macOS 15 PingFang is no longer in /System/Library/Fonts.
    #[cfg(target_os = "macos")]
    for dir in [
        "/System/Library/PrivateFrameworks/FontServices.framework/Resources/Reserved",
        "/System/Library/PrivateFrameworks/FontServices.framework/Versions/A/Resources/Reserved",
    ] {
        if !db.faces().any(|f| has_name(f, "PingFang SC")) {
            db.load_fonts_dir(dir);
        }
    }
    let defaults = asked.map(|child| {
        let out = child?.wait_with_output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !name.is_empty()).then_some(name)
    });
    Installed { db, defaults, took: start.elapsed() }
}

fn has_name(face: &FaceInfo, name: &str) -> bool {
    face.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// The CJK family list and fontconfig language for the user's locale. Han
/// characters are drawn differently per region and the right font cannot be
/// told from the text alone, so the locale decides; Simplified Chinese is
/// the default.
fn cjk_lang() -> (&'static [&'static str], &'static str) {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace('-', "_");
    if locale.starts_with("ja") {
        (CJK_JA, "ja")
    } else if locale.starts_with("ko") {
        (CJK_KO, "ko")
    } else if locale.starts_with("zh_hk") || locale.starts_with("zh_mo") {
        (CJK_HK, "zh-hk")
    } else if locale.starts_with("zh_tw") || locale.starts_with("zh_hant") {
        (CJK_TC, "zh-tw")
    } else {
        (CJK_SC, "zh-cn")
    }
}

/// CJK families to try, best first.
fn cjk_candidates() -> Vec<&'static str> {
    let first = cjk_lang().0;
    let mut list = first.to_vec();
    for more in [CJK_SC, CJK_TC, CJK_HK, CJK_JA, CJK_KO] {
        list.extend(more.iter().filter(|n| !first.contains(n)));
    }
    list
}

impl Fonts {
    /// Choose the families and build a `FontSystem` that knows only them.
    /// `sans` / `mono` are the user's explicit choices.
    pub fn new(installed: Installed, sans: Option<&str>, mono: Option<&str>) -> (FontSystem, Fonts) {
        let Installed { db: all, defaults: [fc_sans, fc_mono, fc_cjk], .. } = installed;
        // Explicit choice, then the system's configured default, then the
        // first installed family of our own list.
        let pick = |explicit: Option<&str>, configured: &Option<String>, list: &[&str]| {
            explicit
                .into_iter()
                .chain(configured.as_deref())
                .chain(list.iter().copied())
                .find_map(|name| family(&all, name))
        };
        let unknown = |name: &str| Family { name: name.into(), weights: vec![], variable: None };
        let sans = pick(sans, &fc_sans, SANS)
            .or_else(|| any_family(&all, false))
            .unwrap_or_else(|| unknown("sans-serif"));
        let mono = pick(mono, &fc_mono, MONO)
            .or_else(|| any_family(&all, true))
            .unwrap_or_else(|| unknown("monospace"));
        // fontconfig answers with its default font when nothing supports the
        // language, so its CJK answer only counts if the font has Han glyphs.
        let fc_cjk = fc_cjk.filter(|name| {
            let mut probe = vec!['中'];
            uncovered(&all, name, &mut probe);
            probe.is_empty()
        });
        let cjk = pick(None, &fc_cjk, &cjk_candidates());

        let mut db = Database::new();
        let mut loaded = Vec::new();
        for fam in [Some(&sans), Some(&mono), cjk.as_ref()].into_iter().flatten() {
            copy_family(&all, &mut db, &fam.name, &mut loaded);
        }
        db.set_sans_serif_family(sans.name.clone());
        db.set_monospace_family(mono.name.clone());

        let locale = "en-US".to_string(); // only used by cosmic-text's own fallback lists
        let fs = FontSystem::new_with_locale_and_db_and_fallback(locale, db, PlatformFallback);
        (fs, Fonts { sans, mono, cjk, all, loaded, checked: HashSet::new() })
    }

    /// Make sure every character of `text` can be drawn, copying additional
    /// families into `fs` for the ones the chosen families lack (emoji,
    /// symbols, other scripts). Cheap when nothing is missing.
    pub fn cover(&mut self, fs: &mut FontSystem, text: &str) {
        let mut missing: Vec<char> = text
            .chars()
            .filter(|&c| c as u32 > 0x7e && !c.is_whitespace() && !c.is_control())
            .filter(|&c| self.checked.insert(c))
            .collect();
        if missing.is_empty() {
            return;
        }
        for name in &self.loaded {
            uncovered(&self.all, name, &mut missing);
        }
        if missing.is_empty() {
            return;
        }

        // Preferred extras first, then everything else that is installed.
        let mut names: Vec<String> = EXTRA.iter().chain(cjk_candidates().iter()).map(|s| s.to_string()).collect();
        let mut rest: Vec<&FaceInfo> = self.all.faces().collect();
        rest.sort_by_key(|f| (f.style != Style::Normal, f.weight.0.abs_diff(400), f.monospaced));
        names.extend(rest.iter().filter_map(|f| f.families.first()).map(|(n, _)| n.clone()));

        let mut seen = HashSet::new();
        let mut added = Vec::new();
        for name in names {
            if missing.is_empty() {
                break;
            }
            if !seen.insert(name.to_ascii_lowercase()) || self.loaded.iter().any(|l| l.eq_ignore_ascii_case(&name)) {
                continue;
            }
            let before = missing.len();
            uncovered(&self.all, &name, &mut missing);
            if missing.len() < before {
                added.push(name);
            }
        }
        if !added.is_empty() {
            let db = fs.db_mut();
            for name in added {
                copy_family(&self.all, db, &name, &mut self.loaded);
            }
        }
    }
}

/// Describe the family called `name`, if it is installed.
fn family(all: &Database, name: &str) -> Option<Family> {
    let faces: Vec<&FaceInfo> = all.faces().filter(|f| has_name(f, name)).collect();
    let first = faces.first()?;
    // Use the spelling the font itself uses; cosmic-text compares exactly.
    let name = first.families.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?.0.clone();
    let upright: Vec<&&FaceInfo> = faces.iter().filter(|f| f.style == Style::Normal).collect();
    let pool = if upright.is_empty() { faces.iter().collect() } else { upright };
    let mut weights: Vec<u16> = pool.iter().map(|f| f.weight.0).collect();
    weights.sort_unstable();
    weights.dedup();
    let variable = pool.iter().find_map(|f| {
        all.with_face_data(f.id, |data, index| {
            let axes = FontRef::from_index(data, index).ok()?.axes();
            let axis = axes.get_by_tag(Tag::new(b"wght"))?;
            Some((axis.min_value() as u16, axis.max_value() as u16))
        })?
    });
    Some(Family { name, weights, variable })
}

/// Last resort when none of the known names is installed.
fn any_family(all: &Database, monospaced: bool) -> Option<Family> {
    let face = all
        .faces()
        .filter(|f| f.monospaced == monospaced && f.style == Style::Normal)
        .min_by_key(|f| f.weight.0.abs_diff(400))?;
    family(all, &face.families.first()?.0)
}

fn copy_family(all: &Database, db: &mut Database, name: &str, loaded: &mut Vec<String>) {
    for face in all.faces().filter(|f| has_name(f, name)) {
        db.push_face_info(face.clone());
    }
    loaded.push(name.to_string());
}

/// Remove from `chars` the ones the regular face of family `name` can draw.
fn uncovered(all: &Database, name: &str, chars: &mut Vec<char>) {
    let face = all
        .faces()
        .filter(|f| has_name(f, name))
        .min_by_key(|f| (f.style != Style::Normal, f.weight.0.abs_diff(400)));
    let Some(face) = face else { return };
    all.with_face_data(face.id, |data, index| {
        if let Ok(font) = FontRef::from_index(data, index) {
            let map = font.charmap();
            chars.retain(|&c| map.map(c).is_none());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_nearest_weight() {
        let pingfang = Family { name: "PingFang SC".into(), weights: vec![100, 300, 400, 500, 600], variable: None };
        assert_eq!(pingfang.weight(400), 400);
        assert_eq!(pingfang.weight(600), 600);
        assert_eq!(pingfang.weight(700), 600);

        let noto = Family { name: "Noto Sans CJK SC".into(), weights: vec![400, 700], variable: None };
        assert_eq!(noto.weight(600), 700);
        assert_eq!(noto.weight(500), 400);

        let sf = Family { name: ".SF NS".into(), weights: vec![400], variable: Some((1, 1000)) };
        assert_eq!(sf.weight(600), 600);

        let single = Family { name: "WenQuanYi Zen Hei".into(), weights: vec![400], variable: None };
        assert_eq!(single.weight(600), 400);
    }
}
