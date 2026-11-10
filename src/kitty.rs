//! Kitty graphics protocol encoding.
//! https://sw.kovidgoyal.net/kitty/graphics-protocol/
//!
//! All commands use `q=2` so the terminal never replies; replies would
//! otherwise show up in the keyboard input stream.

use base64::Engine as _;
use flate2::{Compression, write::ZlibEncoder};
use std::io::Write;

/// Upload an RGBA image under `id` without displaying it.
pub fn transmit(out: &mut Vec<u8>, id: u32, width: u32, height: u32, rgba: &[u8]) {
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

/// Show the `width`×`height` strip of image `id` starting at source row
/// `src_y`, with its top-left at terminal row `row` (1-based), shifted down by
/// `y_off` pixels inside that cell. Re-placing an image moves it in place.
pub fn place(out: &mut Vec<u8>, id: u32, row: u32, src_y: u32, width: u32, height: u32, y_off: u32) {
    let _ = write!(
        out,
        "\x1b[{row};1H\x1b_Ga=p,q=2,C=1,i={id},p=1,x=0,y={src_y},w={width},h={height},Y={y_off}\x1b\\"
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
