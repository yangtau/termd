//! Opening the file in the user's editor, at a given line.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// The user's editor: `$VISUAL`, then `$EDITOR`, then `vi`. The value may
/// carry arguments (`code --wait`), as it may for git.
pub fn configured() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// The arguments that make `editor` open `path` at 1-based `line`. Nearly
/// every terminal editor understands `+LINE FILE`; the graphical ones have
/// their own spellings.
pub fn arguments(editor: &str, path: &Path, line: u32) -> Vec<OsString> {
    let program = editor.split_whitespace().next().unwrap_or("");
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    let at_line = || {
        let mut target = path.as_os_str().to_os_string();
        target.push(format!(":{line}"));
        target
    };
    match name {
        "code" | "code-insiders" | "codium" | "cursor" | "windsurf" => vec!["--goto".into(), at_line()],
        "subl" | "zed" | "hx" | "helix" => vec![at_line()],
        _ => vec![format!("+{line}").into(), path.as_os_str().to_os_string()],
    }
}

/// Run the editor on `path` at `line` and wait for it to exit. The terminal
/// must be in its normal state.
pub fn run(path: &Path, line: u32) -> std::io::Result<ExitStatus> {
    let editor = configured();
    // Through the shell, so quoting inside the variable works as users
    // expect from other tools; the file name is passed as an argument and
    // never interpreted by the shell.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .args(arguments(&editor, path, line))
        .spawn()?;
    // Ctrl-C belongs to the editor while it runs (it goes to the whole
    // foreground process group); it must not end the viewer underneath it.
    // SAFETY: changing the disposition of SIGINT is always sound.
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = child.wait();
    // SAFETY: as above; `previous` came from `signal`.
    unsafe { libc::signal(libc::SIGINT, previous) };
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(editor: &str) -> Vec<String> {
        arguments(editor, Path::new("notes/a b.md"), 42).into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn terminal_editors_take_plus_line() {
        for editor in ["vi", "nvim", "/usr/bin/vim", "nano", "emacs -nw", "emacsclient -t", "micro", "kak", ""] {
            assert_eq!(args(editor), ["+42", "notes/a b.md"], "{editor:?}");
        }
    }

    #[test]
    fn other_editors_take_their_own_form() {
        assert_eq!(args("code --wait"), ["--goto", "notes/a b.md:42"]);
        assert_eq!(args("/usr/local/bin/cursor -w"), ["--goto", "notes/a b.md:42"]);
        assert_eq!(args("hx"), ["notes/a b.md:42"]);
        assert_eq!(args("zed --wait"), ["notes/a b.md:42"]);
        assert_eq!(args("subl -w"), ["notes/a b.md:42"]);
    }
}
