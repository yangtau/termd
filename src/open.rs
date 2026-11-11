//! Opening link targets with the system's handler.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Whether `target` names a scheme (`https:`, `mailto:`), as opposed to a
/// path. A single letter is a Windows drive, not a scheme.
fn has_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once(':') else { return false };
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Undo percent-encoding (`%20`), leaving malformed escapes as they are.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What to hand to the system for link destination `target` of a document
/// in directory `base`: the URL itself, or the file a relative link means.
pub fn resolve(target: &str, base: &Path) -> Result<OsString, String> {
    if has_scheme(target) {
        return Ok(target.into());
    }
    // A link to a file may carry a fragment or query the file system does
    // not know about.
    let path = target.split(['#', '?']).next().unwrap_or(target);
    let path = base.join(percent_decode(path));
    match path.exists() {
        true => Ok(path.into_os_string()),
        false => Err(format!("{} does not exist", path.display())),
    }
}

/// Open link destination `target` the way the desktop would. Returns what
/// was opened.
pub fn open(target: &str, base: &Path) -> Result<String, String> {
    let what = resolve(target, base)?;
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child = Command::new(opener)
        .arg(&what)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run {opener}: {error}"))?;
    // Reap it whenever it finishes; the viewer does not wait.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(what.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_passed_on() {
        let base = Path::new("/nowhere");
        for url in ["https://example.com/a?b#c", "mailto:someone@example.com", "x-app://thing"] {
            assert_eq!(resolve(url, base), Ok(url.into()));
        }
    }

    #[test]
    fn relative_links_mean_files_next_to_the_document() {
        let dir = std::env::temp_dir().join(format!("termd-open-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        std::fs::write(dir.join("sub dir/notes.md"), "x").unwrap();
        let expect = dir.join("sub dir/notes.md").into_os_string();
        assert_eq!(resolve("sub%20dir/notes.md#section", &dir), Ok(expect.clone()));
        assert_eq!(resolve("sub dir/notes.md", &dir), Ok(expect));
        assert!(resolve("missing.md", &dir).unwrap_err().contains("does not exist"));
        // Not a scheme: a one-letter prefix, or a colon later in a path.
        assert!(resolve("c:notes.md", &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b%E4%B8%AD"), "a b中");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }
}
