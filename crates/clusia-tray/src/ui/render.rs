//! Paints a layout offscreen to a PNG (tests, docs, quick visual checks).

use std::path::Path;

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSBitmapImageFileType,
};
use objc2_foundation::NSDictionary;

use crate::layout::{Layout, search_placeholder};
use crate::ui::paint::ContentView;

pub fn render_png(
    mtm: MainThreadMarker,
    mut layout: Layout,
    dark: bool,
    backdrop: Option<(f64, f64, f64)>,
    out: &Path,
) -> Result<(), String> {
    // No native search field offscreen: paint its placeholder in the reserved row.
    if let Some(r) = layout.search {
        layout.shapes.extend(search_placeholder(r));
    }
    let view = ContentView::new(mtm);
    view.set_backdrop(backdrop);
    view.set_layout(layout);
    // SAFETY: the appearance names are immutable NSString statics.
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    view.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
    let bounds = view.bounds();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or("cannot allocate a bitmap")?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // SAFETY: an empty properties dictionary is valid for PNG encoding.
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("cannot encode the PNG")?;
    std::fs::write(out, data.to_vec()).map_err(|e| format!("cannot write {}: {e}", out.display()))
}
