//! Startup terminal probing: colors, Kitty graphics support, whether images
//! can be passed through shared memory, and window pixel size. Must run in
//! raw mode, before the event loop starts reading stdin.

use crate::kitty;
use crate::theme::Rgb;
use base64::Engine as _;
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Probe {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub kitty_graphics: bool,
    /// The terminal could read an image from a POSIX shared memory object,
    /// i.e. it supports that transport and runs on this machine.
    pub shm: bool,
    /// Text area size in pixels (width, height), from `CSI 14 t`.
    pub pixels: Option<(u32, u32)>,
    /// The terminal can report the mouse position in pixels (mode 1016)
    /// rather than cells.
    pub pixel_mouse: bool,
}

pub fn probe() -> Probe {
    // A 1x1 image in shared memory for the transport query. The terminal
    // unlinks it if it reads it; otherwise we do, below.
    let shm = kitty::shm_name(0);
    let shm_query = if kitty::shm_create(&shm, &[0; 4]) {
        let name = base64::engine::general_purpose::STANDARD.encode(shm.as_bytes());
        format!("\x1b_Gi=32,s=1,v=1,a=q,t=s,f=32,S=4;{name}\x1b\\")
    } else {
        String::new()
    };

    let mut out = std::io::stdout().lock();
    // OSC 10/11 (foreground/background), a 1x1 graphics query, the shared
    // memory query, CSI 14 t (pixel size), DECRQM for pixel mouse reports,
    // and finally DA1, which every terminal answers: it marks the end of
    // the replies.
    let _ = write!(
        out,
        "\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\{shm_query}\x1b[14t\x1b[?1016$p\x1b[c"
    );
    let _ = out.flush();
    let probe = parse(&read_until_da1(Duration::from_millis(1000)));
    kitty::shm_unlink(&shm);
    probe
}

fn read_until_da1(timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: `chunk` is a valid writable buffer of the given length.
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if has_da1(&buf) {
            break;
        }
    }
    buf
}

/// DA1 reply: `ESC [ ? ... c`.
fn has_da1(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|i| buf[i..].contains(&b'c'))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse(buf: &[u8]) -> Probe {
    let mut p = Probe {
        kitty_graphics: find(buf, b"_Gi=31;OK").is_some(),
        shm: find(buf, b"_Gi=32;OK").is_some(),
        // DECRPM: 1 = set, 2 = reset, 3 = permanently set; 0 and 4 mean it
        // is not available.
        pixel_mouse: find(buf, b"\x1b[?1016;").is_some_and(|i| matches!(buf.get(i + 8), Some(b'1' | b'2' | b'3'))),
        fg: parse_color(buf, b"]10;rgb:"),
        bg: parse_color(buf, b"]11;rgb:"),
        ..Probe::default()
    };

    if let Some(i) = find(buf, b"\x1b[4;") {
        let s = String::from_utf8_lossy(&buf[i + 4..]);
        if let Some(body) = s.split('t').next() {
            let mut it = body.split(';').map(|n| n.parse::<u32>().ok());
            if let (Some(Some(h)), Some(Some(w))) = (it.next(), it.next())
                && w > 0
                && h > 0
            {
                p.pixels = Some((w, h));
            }
        }
    }
    p
}

/// An OSC color reply: `rgb:RRRR/GGGG/BBBB` after `prefix`.
fn parse_color(buf: &[u8], prefix: &[u8]) -> Option<Rgb> {
    let start = find(buf, prefix)? + prefix.len();
    let s = String::from_utf8_lossy(&buf[start..]);
    let mut parts = s.split('/').map(|c| {
        let hex: String = c.chars().take_while(char::is_ascii_hexdigit).collect();
        let v = u32::from_str_radix(&hex, 16).ok()?;
        // Components have 1-4 hex digits; scale to 8 bits.
        Some(match hex.len() {
            1 => v * 17,
            2 => v,
            3 => v >> 4,
            _ => v >> 8,
        } as u8)
    });
    Some(Rgb(parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        let p = parse(
            b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\\x1b]11;rgb:1e1e/2a2a/ffff\x1b\\\x1b_Gi=31;OK\x1b\\\
              \x1b_Gi=32;OK\x1b\\\x1b[4;1200;1920t\x1b[?1016;2$y\x1b[?62;c",
        );
        assert!(p.pixel_mouse);
        assert_eq!(p.fg, Some(Rgb(0xcd, 0xd6, 0xf4)));
        assert_eq!(p.bg, Some(Rgb(0x1e, 0x2a, 0xff)));
        assert!(p.kitty_graphics);
        assert!(p.shm);
        assert_eq!(p.pixels, Some((1920, 1200)));
    }

    #[test]
    fn shared_memory_error_is_not_support() {
        // What a terminal on another machine (ssh) answers.
        let p = parse(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EBADF:Failed to open shared memory\x1b\\\x1b[?62;c");
        assert!(p.kitty_graphics);
        assert!(!p.shm);
    }

    #[test]
    fn handles_missing_replies() {
        let p = parse(b"\x1b[?62;c");
        assert_eq!((p.fg, p.bg), (None, None));
        assert!(!p.pixel_mouse);
        // "Not recognized" is an answer too.
        assert!(!parse(b"\x1b[?1016;0$y\x1b[?62;c").pixel_mouse);
        assert!(!p.kitty_graphics);
        assert!(!p.shm);
        assert_eq!(p.pixels, None);
    }
}
