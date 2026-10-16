//! Installed fonts, asked of Core Text: the families to choose from, and the files of one
//! family. Both can take a while on a Mac with many fonts, so callers run them off the main
//! thread.

use std::path::PathBuf;
use std::ptr;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFURL};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontManagerCopyAvailableFontFamilyNames, CTFontSymbolicTraits,
    CTFontUIFontType, kCTFontFamilyNameAttribute, kCTFontURLAttribute,
};

/// One face of a family: the file it is in, and its PostScript name (which face of a
/// collection file it is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceFile {
    pub path: PathBuf,
    pub postscript: String,
}

/// The faces text asks for: the regular one, and the bold and italic ones a family has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyFiles {
    pub regular: FaceFile,
    pub bold: Option<FaceFile>,
    pub italic: Option<FaceFile>,
    pub bold_italic: Option<FaceFile>,
}

/// The families installed on this Mac, sorted, without the hidden ones (a leading dot).
pub fn installed_families() -> Vec<String> {
    // SAFETY: Core Text returns a new array of family names (CFStrings).
    let names = unsafe { CTFontManagerCopyAvailableFontFamilyNames() };
    // SAFETY: the documented element type of the array is CFString.
    let names: CFRetained<objc2_core_foundation::CFArray<CFString>> =
        unsafe { CFRetained::cast_unchecked(names) };
    let mut out: Vec<String> = (0..names.len())
        .filter_map(|i| names.get(i))
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty() && !name.starts_with('.'))
        .collect();
    out.sort_by_key(|name| name.to_lowercase());
    out.dedup();
    out
}

/// The files of `family`, or `None` when it is not installed. `system` asks for the macOS
/// system font instead, whose family has a hidden name.
pub fn family_files(family: &str, system: bool) -> Option<FamilyFiles> {
    let base = if system {
        // SAFETY: no language; Core Text returns the system UI font or nothing.
        unsafe { CTFont::new_ui_font_for_language(CTFontUIFontType::System, 13.0, None) }?
    } else {
        let key: &CFString = unsafe { kCTFontFamilyNameAttribute };
        let name = CFString::from_str(family);
        let attributes = CFDictionary::from_slices(&[key], &[&*name]);
        // SAFETY: a dictionary of descriptor attributes; a null matrix is the identity.
        let font = unsafe {
            let descriptor = CTFontDescriptor::with_attributes(attributes.as_opaque());
            CTFont::with_font_descriptor(&descriptor, 13.0, ptr::null())
        };
        // Core Text answers with a substitute for a family it does not have.
        let found = unsafe { font.family_name() }.to_string();
        if !found.eq_ignore_ascii_case(family) {
            return None;
        }
        font
    };
    let styled = |traits: CTFontSymbolicTraits| {
        let mask = CTFontSymbolicTraits::TraitBold | CTFontSymbolicTraits::TraitItalic;
        // SAFETY: a null matrix keeps the base font's; `None` when the family lacks the style.
        unsafe { base.copy_with_symbolic_traits(0.0, ptr::null(), traits, mask) }
            .and_then(|font| face_file(&font))
    };
    let regular = face_file(&base)?;
    let other = |face: Option<FaceFile>| face.filter(|f| *f != regular);
    Some(FamilyFiles {
        bold: other(styled(CTFontSymbolicTraits::TraitBold)),
        italic: other(styled(CTFontSymbolicTraits::TraitItalic)),
        bold_italic: other(styled(
            CTFontSymbolicTraits::TraitBold | CTFontSymbolicTraits::TraitItalic,
        )),
        regular,
    })
}

fn face_file(font: &CTFont) -> Option<FaceFile> {
    let key: &CFString = unsafe { kCTFontURLAttribute };
    // SAFETY: the URL attribute of a font is a CFURL, when the font has a file.
    let url = unsafe { font.attribute(key) }?.downcast::<CFURL>().ok()?;
    Some(FaceFile {
        path: url.to_file_path()?,
        postscript: unsafe { font.post_script_name() }.to_string(),
    })
}
