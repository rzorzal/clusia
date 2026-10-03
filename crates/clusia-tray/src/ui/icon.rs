//! Monochrome template image for the menu bar, with a dot variant for new activity (spec §7.3).

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::{NSBezierPath, NSColor, NSImage};
use objc2_foundation::{NSPoint, NSRect, NSSize, ns_string};

pub fn status_image(news: bool) -> Retained<NSImage> {
    let leaf = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        ns_string!("leaf"),
        Some(ns_string!("Clúsia")),
    )
    .expect("SF Symbols ship with macOS 11 and later");
    leaf.setTemplate(true);
    if !news {
        return leaf;
    }
    let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(20.0, 16.0));
    #[allow(deprecated)] // lockFocus is the simplest way to compose a small template image
    image.lockFocus();
    leaf.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(16.0, 16.0)));
    NSColor::blackColor().setFill();
    NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
        NSPoint::new(14.0, 10.0),
        NSSize::new(6.0, 6.0),
    ))
    .fill();
    #[allow(deprecated)]
    image.unlockFocus();
    image.setTemplate(true);
    image
}
