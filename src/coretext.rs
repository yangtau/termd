//! Asking macOS where its fonts are.
//!
//! The system keeps its own index of the installed fonts, so finding the
//! file of a family, or a font for a character, is a question to Core Text
//! that takes a fraction of a millisecond. That is why termd does not index
//! the fonts itself on macOS (see `fonts`).

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRange, CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::CFURL;
use std::path::PathBuf;
use std::ptr::null;

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontFamilyNameAttribute: CFStringRef;
    static kCTFontURLAttribute: CFStringRef;
    fn CTFontDescriptorCreateWithAttributes(attributes: CFDictionaryRef) -> CFTypeRef;
    fn CTFontDescriptorCreateMatchingFontDescriptors(descriptor: CFTypeRef, mandatory: CFTypeRef) -> CFArrayRef;
    fn CTFontDescriptorCopyAttribute(descriptor: CFTypeRef, attribute: CFStringRef) -> CFTypeRef;
    fn CTFontCreateWithFontDescriptor(descriptor: CFTypeRef, size: f64, matrix: *const f64) -> CFTypeRef;
    fn CTFontCreateForString(font: CFTypeRef, string: CFStringRef, range: CFRange) -> CFTypeRef;
    fn CTFontCopyAttribute(font: CFTypeRef, attribute: CFStringRef) -> CFTypeRef;
}

/// Take ownership of an object a Create or Copy function returned.
///
/// SAFETY: `object` must be null or such a result, not released yet.
unsafe fn owned(object: CFTypeRef) -> Option<CFType> {
    // SAFETY: not null, and the caller hands over the reference it owns.
    (!object.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(object) })
}

/// A description of the fonts of the family called `name`.
fn descriptor(name: &str) -> Option<CFType> {
    // SAFETY: the attribute key is a constant of the framework; the
    // dictionary outlives the call that reads it.
    unsafe {
        let key = CFString::wrap_under_get_rule(kCTFontFamilyNameAttribute);
        let attributes = CFDictionary::from_CFType_pairs(&[(key, CFString::new(name))]);
        owned(CTFontDescriptorCreateWithAttributes(attributes.as_concrete_TypeRef()))
    }
}

/// The file a font or font descriptor attribute names.
fn path(url: Option<CFType>) -> Option<PathBuf> {
    url?.downcast_into::<CFURL>()?.to_path()
}

/// The font files that hold the family called `name` (in any case), or
/// nothing if no such family is installed.
pub fn family_files(name: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Some(descriptor) = descriptor(name) else { return files };
    // SAFETY: `descriptor` is a font descriptor; the result is null or an
    // array of font descriptors that we own.
    let matches = unsafe { CTFontDescriptorCreateMatchingFontDescriptors(descriptor.as_CFTypeRef(), null()) };
    if matches.is_null() {
        return files;
    }
    // SAFETY: see above.
    let matches: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(matches) };
    for face in matches.iter() {
        // SAFETY: `face` is a font descriptor and the key a framework constant.
        let url = unsafe { owned(CTFontDescriptorCopyAttribute(face.as_CFTypeRef(), kCTFontURLAttribute)) };
        if let Some(file) = path(url).filter(|file| !files.contains(file)) {
            files.push(file);
        }
    }
    files
}

/// For the characters of `chars` that text set in the family `family`
/// would need another font for, the files of the fonts the system would
/// take, in the order of the characters.
pub fn files_for(family: &str, chars: &[char]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    // SAFETY: `descriptor` is a font descriptor. (A family that does not
    // exist still gives a font: the system's default.)
    let base = descriptor(family).and_then(|d| unsafe { owned(CTFontCreateWithFontDescriptor(d.as_CFTypeRef(), 16.0, null())) });
    let Some(base) = base else { return files };
    for c in chars {
        let text = CFString::new(c.encode_utf8(&mut [0; 4]));
        let range = CFRange::init(0, c.len_utf16() as isize);
        // SAFETY: `base` is a font, and `range` is all of `text`, counted in
        // UTF-16 units as Core Foundation does. The result is a font we own.
        let font = unsafe { owned(CTFontCreateForString(base.as_CFTypeRef(), text.as_concrete_TypeRef(), range)) };
        let Some(font) = font else { continue };
        // SAFETY: `font` is a font and the key a framework constant.
        let url = unsafe { owned(CTFontCopyAttribute(font.as_CFTypeRef(), kCTFontURLAttribute)) };
        // The font of last resort draws a placeholder for anything.
        let real = |file: &PathBuf| file.file_stem().is_some_and(|name| name != "LastResort");
        if let Some(file) = path(url).filter(|file| real(file) && !files.contains(file)) {
            files.push(file);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_fonts_that_every_mac_has() {
        let menlo = family_files("menlo");
        assert!(menlo.iter().any(|file| file.ends_with("Menlo.ttc")), "{menlo:?}");
        assert!(family_files("No Such Family 123").is_empty());
        // Menlo has no emoji; the system knows a font that has.
        let emoji = files_for("Menlo", &['a', '😀']);
        assert!(emoji.iter().any(|file| file.to_string_lossy().contains("Emoji")), "{emoji:?}");
    }
}
