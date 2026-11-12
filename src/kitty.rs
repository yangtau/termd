//! Kitty graphics protocol encoding.
//! https://sw.kovidgoyal.net/kitty/graphics-protocol/
//!
//! All commands use `q=2` so the terminal never replies; replies would
//! otherwise show up in the keyboard input stream.

use crate::theme::Rgba;
use base64::Engine as _;
use flate2::{Compression, write::ZlibEncoder};
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Uploads images, through shared memory when the terminal runs on this
/// machine and supports it, otherwise inline in the escape sequence.
///
/// Shared memory skips compressing, base64-encoding and parsing megabytes of
/// pixels per tile: the terminal maps the object and reads the pixels
/// directly.
pub struct Uploader {
    shm: bool,
    /// Pass images in temporary files in this directory instead (when not
    /// through shared memory).
    files: Option<PathBuf>,
    seq: u32,
    /// Objects handed to the terminal. It unlinks each one after reading it;
    /// ours is a safety net in case it never does.
    handed: VecDeque<(CString, Instant)>,
    handed_files: VecDeque<(PathBuf, Instant)>,
}

impl Uploader {
    /// `shm`: whether the terminal answered the shared memory probe.
    pub fn new(shm: bool) -> Uploader {
        Uploader { shm, files: None, seq: 0, handed: VecDeque::new(), handed_files: VecDeque::new() }
    }

    /// Pass images in temporary files in `dir` (which the terminal must
    /// accept as a temporary directory), for a terminal on this machine
    /// that may not do shared memory. Escape sequences stay short, which
    /// matters when they share the terminal with another program's output.
    pub fn with_files(dir: PathBuf) -> Uploader {
        let mut uploader = Uploader::new(false);
        uploader.files = Some(dir);
        uploader
    }

    pub fn uses_shm(&self) -> bool {
        self.shm
    }

    /// Upload an RGBA image under `id` without displaying it.
    pub fn transmit(&mut self, out: &mut Vec<u8>, id: u32, width: u32, height: u32, rgba: &[u8]) {
        self.collect(Duration::from_secs(10));
        if self.shm {
            self.seq += 1;
            let name = shm_name(self.seq);
            if shm_create(&name, rgba) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
                let size = rgba.len();
                let _ = write!(out, "\x1b_Ga=t,f=32,t=s,q=2,i={id},s={width},v={height},S={size};{b64}\x1b\\");
                self.handed.push_back((name, Instant::now()));
                return;
            }
            self.shm = false; // e.g. /dev/shm is full; stay inline from now on
        }
        if let Some(dir) = &self.files {
            self.seq += 1;
            // The terminal deletes a file passed this way once it has read
            // it, if its name says it is meant for that.
            let path = dir.join(format!("tty-graphics-protocol-termd-{}-{}", std::process::id(), self.seq));
            if std::fs::write(&path, rgba).is_ok() {
                let b64 = base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_encoded_bytes());
                let size = rgba.len();
                let _ = write!(out, "\x1b_Ga=t,f=32,t=t,q=2,i={id},s={width},v={height},S={size};{b64}\x1b\\");
                self.handed_files.push_back((path, Instant::now()));
                return;
            }
            let _ = std::fs::remove_file(&path);
            self.files = None; // e.g. the disk is full; stay inline from now on
        }
        transmit_inline(out, id, width, height, rgba);
    }

    /// Unlink handed-over objects older than `age`.
    fn collect(&mut self, age: Duration) {
        while self.handed.front().is_some_and(|(_, at)| at.elapsed() >= age) {
            if let Some((name, _)) = self.handed.pop_front() {
                shm_unlink(&name);
            }
        }
        while self.handed_files.front().is_some_and(|(_, at)| at.elapsed() >= age) {
            if let Some((path, _)) = self.handed_files.pop_front() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

impl Drop for Uploader {
    fn drop(&mut self) {
        self.collect(Duration::ZERO);
    }
}

/// Name for a shared memory object. Short, because macOS limits it to 31 bytes.
pub fn shm_name(seq: u32) -> CString {
    CString::new(format!("/termd-{}-{seq}", std::process::id())).expect("no NUL in name")
}

/// Create the shared memory object `name` holding `data`.
pub fn shm_create(name: &CString, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    // SAFETY: plain POSIX calls on a descriptor and mapping owned by this
    // function; the mapping is `data.len()` bytes and is written within bounds.
    unsafe {
        let flags = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR;
        let fd = libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint);
        if fd < 0 {
            return false;
        }
        let len = data.len();
        let mut ok = libc::ftruncate(fd, len as libc::off_t) == 0;
        // On Linux the object lives on a tmpfs of limited size; reserve the
        // pages now so a full tmpfs is an error here, not a SIGBUS below.
        #[cfg(target_os = "linux")]
        {
            ok = ok && libc::posix_fallocate(fd, 0, len as libc::off_t) == 0;
        }
        if ok {
            let prot = libc::PROT_READ | libc::PROT_WRITE;
            let map = libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0);
            ok = map != libc::MAP_FAILED;
            if ok {
                std::ptr::copy_nonoverlapping(data.as_ptr(), map.cast::<u8>(), len);
                libc::munmap(map, len);
            }
        }
        libc::close(fd);
        if !ok {
            libc::shm_unlink(name.as_ptr());
        }
        ok
    }
}

pub fn shm_unlink(name: &CString) {
    // SAFETY: `name` is a valid NUL-terminated string.
    unsafe {
        libc::shm_unlink(name.as_ptr());
    }
}

/// Upload as zlib-compressed, base64-encoded data inside the escape sequence.
fn transmit_inline(out: &mut Vec<u8>, id: u32, width: u32, height: u32, rgba: &[u8]) {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    z.write_all(rgba).expect("writing to a Vec cannot fail");
    let compressed = z.finish().expect("writing to a Vec cannot fail");
    let b64 = base64::engine::general_purpose::STANDARD.encode(compressed);

    let mut chunks = b64.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            let _ = write!(out, "\x1b_Ga=t,f=32,o=z,q=2,i={id},s={width},v={height},m={more};");
            first = false;
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Where and which part of an uploaded image to show.
pub struct Placement {
    pub id: u32,
    /// Which of the image's placements this is; an image can be shown in
    /// several places at once.
    pub placement: u32,
    /// Terminal cell of the top-left corner, 1-based.
    pub row: u32,
    pub col: u32,
    /// Pixel offset inside that cell.
    pub x_off: u32,
    pub y_off: u32,
    /// Top-left corner of the part of the image to show, and its size.
    pub src_x: u32,
    pub src_y: u32,
    pub width: u32,
    pub height: u32,
    /// Stacking order among images.
    pub z: i32,
}

/// Show part of an image. Placing the same placement again moves it.
pub fn place(out: &mut Vec<u8>, p: &Placement) {
    let Placement { id, placement, row, col, x_off, y_off, src_x, src_y, width, height, z } = *p;
    let _ = write!(
        out,
        "\x1b[{row};{col}H\x1b_Ga=p,q=2,C=1,i={id},p={placement},x={src_x},y={src_y},w={width},h={height},\
         X={x_off},Y={y_off},z={z}\x1b\\"
    );
}

/// Take placement `placement` of image `id` off the screen, keeping the
/// image's pixels in the terminal.
pub fn unplace(out: &mut Vec<u8>, id: u32, placement: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,q=2,i={id},p={placement}\x1b\\");
}

/// Hide image `id` and free its pixels.
pub fn free(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,q=2,i={id}\x1b\\");
}

/// Remove every image on the current screen and free the data.
pub fn free_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\");
}

/// A solid rectangle on the screen, in pixels from the top left corner.
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub color: Rgba,
    /// Stacking order among images; page tiles are at 0, negative is below.
    pub z: i32,
}

/// Draws solid rectangles (scroll bars, what a held table header stands on)
/// by showing crops of small one-color images. Moving or resizing a rectangle
/// is then a placement command of a few dozen bytes, with no pixels to
/// render or send.
#[derive(Default)]
pub struct Sprites {
    /// The colors uploaded so far; the image of color `n` has id `SPRITE_ID + n`.
    colors: Vec<Rgba>,
    /// How many placements of each image the last frame used.
    shown: Vec<u32>,
}

/// Size of a sprite image. Larger rectangles are tiled from several crops.
const SPRITE_W: u32 = 512;
const SPRITE_H: u32 = 128;
/// Image ids from here on are sprites; page tiles count up from 1.
const SPRITE_ID: u32 = 0x4000_0000;

impl Sprites {
    /// Show exactly `rects`, replacing what the previous call showed.
    /// `cell` is the size of a terminal cell in pixels.
    pub fn draw(&mut self, out: &mut Vec<u8>, uploader: &mut Uploader, rects: &[Rect], cell: (u32, u32)) {
        let mut used = vec![0u32; self.colors.len()];
        for rect in rects {
            let image = match self.colors.iter().position(|c| *c == rect.color) {
                Some(image) => image,
                None => {
                    let Rgba(r, g, b, a) = rect.color;
                    let pixels = [r, g, b, a].repeat((SPRITE_W * SPRITE_H) as usize);
                    let image = self.colors.len();
                    uploader.transmit(out, SPRITE_ID + image as u32, SPRITE_W, SPRITE_H, &pixels);
                    self.colors.push(rect.color);
                    used.push(0);
                    image
                }
            };
            for dy in (0..rect.h).step_by(SPRITE_H as usize) {
                for dx in (0..rect.w).step_by(SPRITE_W as usize) {
                    used[image] += 1;
                    let (x, y) = (rect.x + dx, rect.y + dy);
                    let (w, h) = ((rect.w - dx).min(SPRITE_W), (rect.h - dy).min(SPRITE_H));
                    let _ = write!(
                        out,
                        "\x1b[{};{}H\x1b_Ga=p,q=2,C=1,i={},p={},x=0,y=0,w={w},h={h},X={},Y={},z={}\x1b\\",
                        y / cell.1 + 1,
                        x / cell.0 + 1,
                        SPRITE_ID + image as u32,
                        used[image],
                        x % cell.0,
                        y % cell.1,
                        rect.z,
                    );
                }
            }
        }
        // Take away what the previous frame showed beyond this one.
        self.shown.resize(used.len(), 0);
        for (image, (&now, &before)) in used.iter().zip(&self.shown).enumerate() {
            for placement in now + 1..=before {
                let _ = write!(out, "\x1b_Ga=d,d=i,q=2,i={},p={placement}\x1b\\", SPRITE_ID + image as u32);
            }
        }
        self.shown = used;
    }

    /// The terminal's images were all freed: start over.
    pub fn forget(&mut self) {
        self.colors.clear();
        self.shown.clear();
    }
}

/// Put `text` on the system clipboard through the terminal (OSC 52), which
/// also works when the terminal is on another machine.
pub fn copy(out: &mut Vec<u8>, text: &str) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let _ = write!(out, "\x1b]52;c;{b64}\x1b\\");
}

/// Set the shape of the mouse pointer over the window (OSC 22), by its CSS
/// name; an empty name restores the terminal's own.
pub fn pointer(out: &mut Vec<u8>, shape: &str) {
    let _ = write!(out, "\x1b]22;{shape}\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placements(out: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(out)
            .split("\x1b_G")
            .filter(|c| c.starts_with("a=p") || c.starts_with("a=d"))
            .map(|c| c.split("\x1b\\").next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn rectangles_are_placed_as_crops_of_one_image() {
        let mut sprites = Sprites::default();
        let mut uploader = Uploader::new(false);
        let blue = Rgba(0, 0, 255, 255);
        let mut out = Vec::new();
        // Wider than a sprite: two crops side by side.
        let rect = Rect { x: 25, y: 47, w: SPRITE_W + 10, h: 30, color: blue, z: -1 };
        sprites.draw(&mut out, &mut uploader, &[rect], (10, 20));
        let text = String::from_utf8_lossy(&out).to_string();
        assert_eq!(text.matches("a=t,").count(), 1, "the color is uploaded once");
        assert!(text.contains("\x1b[3;3H"), "placed at the cell the corner is in");
        let id = SPRITE_ID;
        assert_eq!(
            placements(&out),
            [
                format!("a=p,q=2,C=1,i={id},p=1,x=0,y=0,w={SPRITE_W},h=30,X=5,Y=7,z=-1"),
                format!("a=p,q=2,C=1,i={id},p=2,x=0,y=0,w=10,h=30,X=7,Y=7,z=-1"),
            ]
        );

        // The next frame needs one crop: no new upload, the second one goes.
        let mut out = Vec::new();
        let rect = Rect { x: 0, y: 0, w: 40, h: 30, color: blue, z: -1 };
        sprites.draw(&mut out, &mut uploader, &[rect], (10, 20));
        assert!(!String::from_utf8_lossy(&out).contains("a=t,"));
        assert_eq!(
            placements(&out),
            [format!("a=p,q=2,C=1,i={id},p=1,x=0,y=0,w=40,h=30,X=0,Y=0,z=-1"), format!("a=d,d=i,q=2,i={id},p=2")]
        );

        // Nothing to show: everything goes.
        let mut out = Vec::new();
        sprites.draw(&mut out, &mut uploader, &[], (10, 20));
        assert_eq!(placements(&out), [format!("a=d,d=i,q=2,i={id},p=1")]);
    }

    #[test]
    fn copies_through_osc_52() {
        let mut out = Vec::new();
        copy(&mut out, "héllo\n");
        assert_eq!(out, b"\x1b]52;c;aMOpbGxvCg==\x1b\\");
    }
}
