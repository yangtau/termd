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
//! chosen ones cannot draw. The scan result is cached on disk (`font_cache`).

use crate::font_cache::{self, Index};
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

/// Body text. Charter is the closest thing macOS ships to a contemporary
/// text serif and is a static font; the variable New York comes after it.
const SERIF: &[&str] = &[
    "Charter",
    "Iowan Old Style",
    "Georgia",
    ".New York",
    "New York",
    "Palatino",
    "Noto Serif",
    "Source Serif 4",
    "Source Serif Pro",
    "Liberation Serif",
    "DejaVu Serif",
    "Times New Roman",
    "Times",
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

const SERIF_SC: &[&str] = &[
    "Songti SC",
    "STSong",
    "Source Han Serif SC",
    "Source Han Serif CN",
    "Noto Serif CJK SC",
    "Noto Serif SC",
    "SimSun",
];
const SERIF_TC: &[&str] = &["Songti TC", "Source Han Serif TC", "Noto Serif CJK TC", "Noto Serif TC", "PMingLiU"];
const SERIF_JA: &[&str] = &[
    "Hiragino Mincho ProN",
    "Source Han Serif",
    "Source Han Serif JP",
    "Noto Serif CJK JP",
    "Noto Serif JP",
    "IPAMincho",
];
const SERIF_KO: &[&str] =
    &["AppleMyungjo", "Source Han Serif K", "Noto Serif CJK KR", "Noto Serif KR", "NanumMyeongjo"];

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
#[derive(Clone)]
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

/// What a piece of text is set in.
#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    /// Running text and headings: a serif, unless the user chose otherwise.
    Body,
    /// Labels around the text (front matter, the language of a code block).
    Ui,
    Mono,
}

/// What the user asked for on the command line.
#[derive(Default)]
pub struct Choice<'a> {
    /// Family for body text.
    pub body: Option<&'a str>,
    /// Family for code.
    pub mono: Option<&'a str>,
    /// Set body text in the sans-serif UI font instead of a serif.
    pub sans: bool,
}

pub struct Fonts {
    body: Family,
    /// CJK companion of `body` (a Song/Mincho face next to a serif).
    body_cjk: Option<Family>,
    ui: Family,
    /// CJK companion of `ui`, also used inside code.
    ui_cjk: Option<Family>,
    mono: Family,
    /// Every installed face; only metadata, the files are not kept open.
    all: Database,
    /// Families already copied into the `FontSystem`.
    loaded: Vec<String>,
    /// Characters already checked for coverage.
    checked: HashSet<char>,
}

/// What `scan` found on this machine.
pub struct Installed {
    index: Index,
    /// How long it took and whether it came from the cache, for `TERMD_DEBUG`.
    pub took: std::time::Duration,
    pub cached: bool,
}

impl Installed {
    pub fn faces(&self) -> usize {
        self.index.db.len()
    }
}

/// Index the installed fonts, from the cache if nothing has changed since
/// it was written. A real scan is slow (hundreds of milliseconds with many
/// fonts, seconds after a reboot), so `main` runs this on a thread while the
/// terminal is being probed.
pub fn scan() -> Installed {
    let start = std::time::Instant::now();
    let lang = cjk_lang().lang;
    if let Some(index) = font_cache::load(lang) {
        return Installed { index, took: start.elapsed(), cached: true };
    }
    let index = index(lang);
    font_cache::store(&index, lang);
    Installed { index, took: start.elapsed(), cached: false }
}

/// Really index the installed fonts.
fn index(lang: &str) -> Index {
    // On Linux and the BSDs fontconfig knows which fonts the user prefers.
    // The three questions run as concurrent processes while we scan.
    let (cjk_sans, cjk_serif) = (format!("sans-serif:lang={lang}"), format!("serif:lang={lang}"));
    // The Latin ones are asked for English text: they are used for non-CJK runs.
    let patterns = ["sans-serif:lang=en", "monospace:lang=en", &cjk_sans, "serif:lang=en", &cjk_serif];
    let asked = patterns.map(|pattern| {
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
    Index { db, defaults }
}

/// The installed fonts, for tests: not from the cache, and `None` where
/// there are none (a build sandbox), in which case text cannot be laid out.
#[cfg(test)]
pub fn installed_for_tests() -> Option<Installed> {
    let index = index(cjk_lang().lang);
    (!index.db.is_empty()).then(|| Installed { index, took: std::time::Duration::ZERO, cached: false })
}

fn has_name(face: &FaceInfo, name: &str) -> bool {
    face.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// CJK families for one region, and the fontconfig language to ask for.
struct Cjk {
    sans: &'static [&'static str],
    serif: &'static [&'static str],
    lang: &'static str,
}

const CJK: [Cjk; 5] = [
    Cjk { sans: CJK_SC, serif: SERIF_SC, lang: "zh-cn" },
    Cjk { sans: CJK_TC, serif: SERIF_TC, lang: "zh-tw" },
    Cjk { sans: CJK_HK, serif: SERIF_TC, lang: "zh-hk" },
    Cjk { sans: CJK_JA, serif: SERIF_JA, lang: "ja" },
    Cjk { sans: CJK_KO, serif: SERIF_KO, lang: "ko" },
];

/// The CJK region of the user's locale. Han characters are drawn
/// differently per region and the right font cannot be told from the text
/// alone, so the locale decides; Simplified Chinese is the default.
fn cjk_lang() -> &'static Cjk {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace('-', "_");
    let region = if locale.starts_with("ja") {
        3
    } else if locale.starts_with("ko") {
        4
    } else if locale.starts_with("zh_hk") || locale.starts_with("zh_mo") {
        2
    } else if locale.starts_with("zh_tw") || locale.starts_with("zh_hant") {
        1
    } else {
        0
    };
    &CJK[region]
}

/// CJK families to try, the user's region first; `serif` selects the
/// Song/Mincho lists.
fn cjk_candidates(serif: bool) -> Vec<&'static str> {
    let list = |cjk: &Cjk| if serif { cjk.serif } else { cjk.sans };
    let first = list(cjk_lang());
    let mut names = first.to_vec();
    for more in CJK.iter().map(list) {
        names.extend(more.iter().filter(|n| !names.contains(n)).collect::<Vec<_>>());
    }
    names
}

impl Fonts {
    /// Choose the families and build a `FontSystem` that knows only them.
    pub fn new(installed: Installed, choice: &Choice) -> (FontSystem, Fonts) {
        let Index { db: all, defaults: [fc_sans, fc_mono, fc_cjk, fc_serif, fc_cjk_serif] } = installed.index;
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
        // fontconfig answers with its default font when nothing supports the
        // language, so its CJK answers only count if the font has Han glyphs.
        let has_han = |name: &String| {
            let mut probe = vec!['中'];
            uncovered(&all, name, &mut probe);
            probe.is_empty()
        };

        let ui = pick(None, &fc_sans, SANS)
            .or_else(|| any_family(&all, false))
            .unwrap_or_else(|| unknown("sans-serif"));
        let ui_cjk = pick(None, &fc_cjk.filter(has_han), &cjk_candidates(false));
        let mono = pick(choice.mono, &fc_mono, MONO)
            .or_else(|| any_family(&all, true))
            .unwrap_or_else(|| unknown("monospace"));
        let (serif, serif_cjk) = match choice.sans {
            true => (None, None),
            false => (pick(None, &fc_serif, SERIF), pick(None, &fc_cjk_serif.filter(has_han), &cjk_candidates(true))),
        };
        let body = choice.body.and_then(|name| family(&all, name)).or(serif).unwrap_or_else(|| ui.clone());
        let body_cjk = serif_cjk.or_else(|| ui_cjk.clone());

        let mut db = Database::new();
        let mut loaded = Vec::new();
        for fam in [Some(&body), body_cjk.as_ref(), Some(&ui), ui_cjk.as_ref(), Some(&mono)].into_iter().flatten() {
            if !loaded.contains(&fam.name) {
                copy_family(&all, &mut db, &fam.name, &mut loaded);
            }
        }
        db.set_sans_serif_family(ui.name.clone());
        db.set_serif_family(body.name.clone());
        db.set_monospace_family(mono.name.clone());

        let locale = "en-US".to_string(); // only used by cosmic-text's own fallback lists
        let fs = FontSystem::new_with_locale_and_db_and_fallback(locale, db, PlatformFallback);
        (fs, Fonts { body, body_cjk, ui, ui_cjk, mono, all, loaded, checked: HashSet::new() })
    }

    /// The family for text in `face`; `cjk` for its CJK characters.
    pub fn family(&self, face: Face, cjk: bool) -> &Family {
        let (latin, han) = match face {
            Face::Body => (&self.body, &self.body_cjk),
            Face::Ui => (&self.ui, &self.ui_cjk),
            Face::Mono => (&self.mono, &self.ui_cjk),
        };
        match (cjk, han) {
            (true, Some(han)) => han,
            _ => latin,
        }
    }

    /// The families in use, for `TERMD_DEBUG`.
    pub fn names(&self) -> String {
        let name = |f: &Option<Family>| f.as_ref().map_or("none".to_string(), |f| format!("{:?}", f.name));
        format!(
            "body={:?}+{} ui={:?}+{} mono={:?}",
            self.body.name,
            name(&self.body_cjk),
            self.ui.name,
            name(&self.ui_cjk),
            self.mono.name
        )
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
        let mut names: Vec<String> = EXTRA.iter().chain(&cjk_candidates(false)).map(|s| s.to_string()).collect();
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
