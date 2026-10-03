//! Brand template image for the menu bar, with a dot variant for new activity (spec §7.3).
//!
//! Both variants come from the brand PNGs (`status*.png`). Each image is built from the @2x
//! data and sized in points, so macOS renders it crisply at 1x and 2x.

use std::cell::OnceCell;
use std::path::Path;

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSGraphicsContext, NSImage};
use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSSize, NSString};

const PLAIN: &[u8] = include_bytes!("../../assets/status-wide@2x.png");
const NEWS: &[u8] = include_bytes!("../../assets/status-new@2x.png");

/// Fixed status item length (pt): wide enough for the 21 pt dot variant, so the item never
/// changes width when the dot appears or disappears.
pub const ITEM_LENGTH: f64 = 26.0;

thread_local! {
    static IMAGES: OnceCell<(Retained<NSImage>, Retained<NSImage>)> = const { OnceCell::new() };
}

fn load(bytes: &[u8], size: NSSize) -> Retained<NSImage> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes))
        .expect("the embedded brand PNG decodes");
    image.setSize(size);
    image.setTemplate(true);
    image
}

/// The menu bar image; built once per thread, then shared.
pub fn status_image(news: bool) -> Retained<NSImage> {
    IMAGES.with(|cell| {
        let (plain, with_dot) = cell.get_or_init(|| {
            (
                load(PLAIN, NSSize::new(21.0, 18.0)),
                load(NEWS, NSSize::new(21.0, 18.0)),
            )
        });
        if news {
            with_dot.clone()
        } else {
            plain.clone()
        }
    })
}

/// Draws `status_image(news)` at 2x into a PNG (docs and tests).
pub fn render_png(news: bool, out: &Path) -> Result<(), String> {
    let image = status_image(news);
    let size = image.size();
    let (w, h) = ((size.width * 2.0) as isize, (size.height * 2.0) as isize);
    // SAFETY: a null plane pointer asks AppKit to allocate the pixel buffer; the arguments describe 8-bit RGBA.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
        NSBitmapImageRep::alloc(),
        std::ptr::null_mut(),
        w,
        h,
        8,
        4,
        true,
        false,
        &NSString::from_str("NSCalibratedRGBColorSpace"),
        0,
        0,
    )
    }
    .ok_or("cannot allocate a bitmap")?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)
        .ok_or("cannot create a drawing context")?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    image.drawInRect(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(w as f64, h as f64),
    ));
    NSGraphicsContext::restoreGraphicsState_class();
    // SAFETY: an empty properties dictionary is valid for PNG encoding.
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("cannot encode the PNG")?;
    std::fs::write(out, data.to_vec()).map_err(|e| format!("cannot write {}: {e}", out.display()))
}
