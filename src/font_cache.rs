//! On-disk cache of the installed-font index.
//!
//! Indexing the fonts means opening every font file on the machine. With a
//! thousand faces (a normal Mac) that takes about 300 ms, and seconds right
//! after a reboot when nothing is in the file cache — far longer than
//! everything else termd does before the first frame. The index only changes
//! when fonts are installed or removed, so it is saved, together with the
//! modification times of the directories it was built from, and reused as
//! long as none of those has changed.
//!
//! The file is plain text, one record per line with tab-separated fields:
//!
//! ```text
//! termd-fonts 1 <lang>
//! D <sans> <mono> <cjk>                 system default families ("" = none)
//! W <mtime> <path>                      a watched path ("-" = missing)
//! F <index> <weight> <style> <stretch> <mono> <path> <postscript> <family>...
//! ```

use cosmic_text::fontdb::{Database, FaceInfo, ID, Language, Source, Stretch, Style, Weight};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const HEADER: &str = "termd-fonts 1";

/// A font index: every installed face, and the system's default families
/// (sans, monospace, CJK) where there is a way to ask.
pub struct Index {
    pub db: Database,
    pub defaults: [Option<String>; 3],
}

fn cache_file() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Caches")
    } else {
        match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".cache"),
        }
    };
    Some(dir.join("termd").join("fonts"))
}

/// Modification time of `path` in nanoseconds, or "-" if it does not exist.
fn stamp(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or("-".to_string(), |d| d.as_nanos().to_string())
}

/// Places fonts are installed to. A font appearing in or disappearing from
/// one of them (or a folder below it) must invalidate the cache; so must
/// one of these being created.
fn roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if cfg!(target_os = "macos") {
        vec![
            "/Library/Fonts".into(),
            "/System/Library/Fonts".into(),
            "/System/Library/AssetsV2".into(),
            "/Network/Library/Fonts".into(),
            home.join("Library/Fonts"),
        ]
    } else {
        let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => home.join(".config"),
        };
        vec![
            "/usr/share/fonts".into(),
            "/usr/local/share/fonts".into(),
            home.join(".fonts"),
            home.join(".local/share/fonts"),
            // The fontconfig settings decide the default families.
            "/etc/fonts/fonts.conf".into(),
            "/etc/fonts/conf.d".into(),
            config.join("fontconfig/fonts.conf"),
            config.join("fontconfig/conf.d"),
        ]
    }
}

/// The paths whose modification time tells whether the index is current:
/// the roots, every directory holding a font, and the directories between
/// the two (adding a folder changes its parent, not the root).
fn watched(db: &Database) -> BTreeSet<PathBuf> {
    let roots = roots();
    let mut paths: BTreeSet<PathBuf> = roots.iter().cloned().collect();
    for face in db.faces() {
        let (Source::File(file) | Source::SharedFile(file, _)) = &face.source else { continue };
        let chain: Vec<&Path> = file.ancestors().skip(1).take(5).collect();
        // Outside the known roots, stop at the parent: further up are
        // directories like $HOME that change all the time.
        let keep = chain.iter().position(|dir| roots.iter().any(|r| r == dir)).map_or(2, |i| i + 1);
        paths.extend(chain.into_iter().take(keep).map(Path::to_path_buf));
    }
    paths
}

fn encode(index: &Index, lang: &str, watched: &BTreeSet<PathBuf>) -> Option<String> {
    // Fields are separated by tabs and records by newlines, so neither may
    // occur inside a field; give up on caching if one does.
    let clean = |s: &str| (!s.contains(['\t', '\n', '\r'])).then_some(());
    let mut out = format!("{HEADER}\t{lang}\n");
    let [sans, mono, cjk] = index.defaults.each_ref().map(|d| d.as_deref().unwrap_or(""));
    for name in [sans, mono, cjk] {
        clean(name)?;
    }
    let _ = writeln!(out, "D\t{sans}\t{mono}\t{cjk}");
    for path in watched {
        let path_str = path.to_str()?;
        clean(path_str)?;
        let _ = writeln!(out, "W\t{}\t{path_str}", stamp(path));
    }
    for face in index.db.faces() {
        let (Source::File(file) | Source::SharedFile(file, _)) = &face.source else { continue };
        let file = file.to_str()?;
        clean(file)?;
        clean(&face.post_script_name)?;
        let style = match face.style {
            Style::Normal => 0,
            Style::Italic => 1,
            Style::Oblique => 2,
        };
        let (index, weight, stretch) = (face.index, face.weight.0, face.stretch.to_number());
        let (mono, name) = (u8::from(face.monospaced), &face.post_script_name);
        let _ = write!(out, "F\t{index}\t{weight}\t{style}\t{stretch}\t{mono}\t{file}\t{name}");
        for (family, _) in &face.families {
            clean(family)?;
            let _ = write!(out, "\t{family}");
        }
        out.push('\n');
    }
    Some(out)
}

/// Parse a cache file. `current` reports whether a watched path still has
/// the recorded stamp; the index is rejected if any does not.
fn decode(text: &str, lang: &str, current: impl Fn(&str, &str) -> bool) -> Option<Index> {
    let mut lines = text.lines();
    if lines.next()? != format!("{HEADER}\t{lang}") {
        return None;
    }
    let mut db = Database::new();
    let mut defaults = [None, None, None];
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["D", sans, mono, cjk] => {
                defaults = [sans, mono, cjk].map(|name| (!name.is_empty()).then(|| name.to_string()));
            }
            ["W", stamp, path] => {
                if !current(path, stamp) {
                    return None;
                }
            }
            ["F", index, weight, style, stretch, mono, file, post_script_name, families @ ..] => {
                db.push_face_info(FaceInfo {
                    id: ID::dummy(),
                    source: Source::File(PathBuf::from(file)),
                    index: index.parse().ok()?,
                    families: families.iter().map(|f| (f.to_string(), Language::English_UnitedStates)).collect(),
                    post_script_name: post_script_name.to_string(),
                    style: match *style {
                        "0" => Style::Normal,
                        "1" => Style::Italic,
                        "2" => Style::Oblique,
                        _ => return None,
                    },
                    weight: Weight(weight.parse().ok()?),
                    stretch: match *stretch {
                        "1" => Stretch::UltraCondensed,
                        "2" => Stretch::ExtraCondensed,
                        "3" => Stretch::Condensed,
                        "4" => Stretch::SemiCondensed,
                        "5" => Stretch::Normal,
                        "6" => Stretch::SemiExpanded,
                        "7" => Stretch::Expanded,
                        "8" => Stretch::ExtraExpanded,
                        "9" => Stretch::UltraExpanded,
                        _ => return None,
                    },
                    monospaced: *mono == "1",
                });
            }
            _ => return None,
        }
    }
    (!db.is_empty()).then_some(Index { db, defaults })
}

/// The saved index, if there is one and no font has been installed or
/// removed since. `lang` is the CJK language the defaults were asked for.
pub fn load(lang: &str) -> Option<Index> {
    let text = std::fs::read_to_string(cache_file()?).ok()?;
    decode(&text, lang, |path, recorded| stamp(Path::new(path)) == recorded)
}

/// Save `index` for the next start. Failing to is not an error: the next
/// start just scans again.
pub fn store(index: &Index, lang: &str) {
    let Some(file) = cache_file() else { return };
    let Some(text) = encode(index, lang, &watched(&index.db)) else { return };
    let Some(dir) = file.parent() else { return };
    // Write to the side and rename, so a concurrent start never reads half.
    let tmp = dir.join(format!("fonts.{}.tmp", std::process::id()));
    let written = std::fs::create_dir_all(dir).is_ok() && std::fs::write(&tmp, text).is_ok();
    if !written || std::fs::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(file: &str, index: u32, families: &[&str], weight: u16, style: Style, monospaced: bool) -> FaceInfo {
        FaceInfo {
            id: ID::dummy(),
            source: Source::File(PathBuf::from(file)),
            index,
            families: families.iter().map(|f| (f.to_string(), Language::English_UnitedStates)).collect(),
            post_script_name: format!("{}-{weight}", families[0].replace(' ', "")),
            style,
            weight: Weight(weight),
            stretch: Stretch::Normal,
            monospaced,
        }
    }

    fn sample() -> Index {
        let mut db = Database::new();
        db.push_face_info(face("/fonts/PingFang.ttc", 3, &["PingFang SC", "苹方-简"], 600, Style::Normal, false));
        db.push_face_info(face("/fonts/sub dir/Menlo.ttc", 1, &["Menlo"], 400, Style::Italic, true));
        Index { db, defaults: [Some("Inter".into()), None, Some("Noto Sans CJK SC".into())] }
    }

    fn summary(index: &Index) -> Vec<String> {
        let mut faces: Vec<String> = index
            .db
            .faces()
            .map(|f| {
                let Source::File(file) = &f.source else { panic!("cached faces are files") };
                let families: Vec<&str> = f.families.iter().map(|(n, _)| n.as_str()).collect();
                let (file, weight) = (file.display(), f.weight.0);
                let (style, mono, name) = (f.style, f.monospaced, &f.post_script_name);
                format!("{file}#{} {families:?} {weight} {style:?} {mono} {name}", f.index)
            })
            .collect();
        faces.sort();
        faces
    }

    #[test]
    fn round_trips() {
        let index = sample();
        let watched: BTreeSet<PathBuf> = ["/fonts", "/fonts/sub dir"].iter().map(PathBuf::from).collect();
        let text = encode(&index, "zh-cn", &watched).unwrap();
        let back = decode(&text, "zh-cn", |_, _| true).expect("valid cache");
        assert_eq!(summary(&back), summary(&index));
        assert_eq!(back.defaults, index.defaults);
    }

    #[test]
    fn rejected_when_stale() {
        let watched: BTreeSet<PathBuf> = ["/fonts", "/fonts/sub dir"].iter().map(PathBuf::from).collect();
        let text = encode(&sample(), "zh-cn", &watched).unwrap();
        // A watched directory changed (the paths do not exist, so their recorded stamp is "-").
        assert!(decode(&text, "zh-cn", |path, stamp| !(path == "/fonts/sub dir" && stamp == "-")).is_none());
        // Asked for another language's defaults.
        assert!(decode(&text, "ja", |_, _| true).is_none());
        // Another format version, or garbage.
        assert!(decode(&text.replace("termd-fonts 1", "termd-fonts 0"), "zh-cn", |_, _| true).is_none());
        assert!(decode(&format!("{text}X\tunknown record\n"), "zh-cn", |_, _| true).is_none());
        assert!(decode("", "zh-cn", |_, _| true).is_none());
    }

    #[test]
    fn names_with_separators_are_not_cached() {
        let mut index = sample();
        index.db.push_face_info(face("/fonts/odd\tname.ttf", 0, &["Odd"], 400, Style::Normal, false));
        assert!(encode(&index, "zh-cn", &BTreeSet::new()).is_none());
    }

    #[test]
    fn watches_font_directories_up_to_their_root() {
        let mut db = Database::new();
        let root = roots().into_iter().next().unwrap();
        let nested = root.join("vendor/family/Font.ttf");
        db.push_face_info(face(nested.to_str().unwrap(), 0, &["Font"], 400, Style::Normal, false));
        db.push_face_info(face("/opt/app/fonts/Other.ttf", 0, &["Other"], 400, Style::Normal, false));
        let watched = watched(&db);
        for dir in [root.join("vendor/family"), root.join("vendor"), root.clone()] {
            assert!(watched.contains(&dir), "{} should be watched", dir.display());
        }
        assert!(!watched.contains(root.parent().unwrap()));
        // Not under a known root: the directory and its parent only.
        assert!(watched.contains(Path::new("/opt/app/fonts")) && watched.contains(Path::new("/opt/app")));
        assert!(!watched.contains(Path::new("/opt")));
    }
}
