//! Kitty graphics protocol encoding.
//! https://sw.kovidgoyal.net/kitty/graphics-protocol/
//!
//! All commands use `q=2` so the terminal never replies; replies would
//! otherwise show up in the keyboard input stream.

use base64::Engine as _;
use flate2::{Compression, write::ZlibEncoder};
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::Write;
use std::time::{Duration, Instant};

/// Uploads images, through shared memory when the terminal runs on this
/// machine and supports it, otherwise inline in the escape sequence.
///
/// Shared memory skips compressing, base64-encoding and parsing megabytes of
/// pixels per tile: the terminal maps the object and reads the pixels
/// directly.
pub struct Uploader {
    shm: bool,
    seq: u32,
    /// Objects handed to the terminal. It unlinks each one after reading it;
    /// ours is a safety net in case it never does.
    handed: VecDeque<(CString, Instant)>,
}

impl Uploader {
    /// `shm`: whether the terminal answered the shared memory probe.
    pub fn new(shm: bool) -> Uploader {
        Uploader { shm, seq: 0, handed: VecDeque::new() }
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
        transmit_inline(out, id, width, height, rgba);
    }

    /// Unlink handed-over objects older than `age`.
    fn collect(&mut self, age: Duration) {
        while self.handed.front().is_some_and(|(_, at)| at.elapsed() >= age) {
            if let Some((name, _)) = self.handed.pop_front() {
                shm_unlink(&name);
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
    /// Terminal cell of the top-left corner, 1-based.
    pub row: u32,
    pub col: u32,
    /// Pixel offset inside that cell.
    pub x_off: u32,
    pub y_off: u32,
    /// First source row and number of rows to show.
    pub src_y: u32,
    pub width: u32,
    pub height: u32,
}

/// Show part of an image. Re-placing an image moves it in place.
pub fn place(out: &mut Vec<u8>, p: &Placement) {
    let Placement { id, row, col, x_off, y_off, src_y, width, height } = *p;
    let _ = write!(
        out,
        "\x1b[{row};{col}H\x1b_Ga=p,q=2,C=1,i={id},p=1,x=0,y={src_y},w={width},h={height},X={x_off},Y={y_off}\x1b\\"
    );
}

/// Hide image `id` but keep its pixels in the terminal.
pub fn unplace(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,q=2,i={id}\x1b\\");
}

/// Hide image `id` and free its pixels.
pub fn free(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,q=2,i={id}\x1b\\");
}

/// Remove every image on the current screen and free the data.
pub fn free_all(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\");
}
