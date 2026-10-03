//! One custom NSView paints the whole popover from a `Layout` and hit-tests clicks against it.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSBezierPath, NSColor, NSEvent,
    NSFont, NSFontAttributeName, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSLineBreakMode, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSStringDrawingOptions, NSStringNSExtendedStringDrawing,
    NSTextAlignment, NSView,
};
use objc2_foundation::{NSArray, NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::actions::Action;
use crate::layout::{Ink, Layout, Rect, Shape, Style, WIDTH};
use crate::theme;

pub type ClickHandler = Box<dyn Fn(Action)>;

#[derive(Default)]
pub struct ContentIvars {
    layout: RefCell<Layout>,
    on_click: RefCell<Option<ClickHandler>>,
    /// Paint a window background (offscreen renders have no vibrancy behind them).
    opaque: Cell<bool>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements and `ContentView` does not implement Drop.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ContentIvars]
    pub struct ContentView;

    impl ContentView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            if self.ivars().opaque.get() {
                NSColor::windowBackgroundColor().setFill();
                NSBezierPath::fillRect(self.bounds());
            }
            for shape in &self.ivars().layout.borrow().shapes {
                paint(shape);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let action = self.ivars().layout.borrow().hit(p.x, p.y).cloned();
            if let (Some(action), Some(handler)) = (action, self.ivars().on_click.borrow().as_ref()) {
                handler(action);
            }
        }
    }
);

impl ContentView {
    pub fn new(mtm: MainThreadMarker, opaque: bool) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ContentIvars::default());
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.ivars().opaque.set(opaque);
        view
    }

    pub fn set_layout(&self, layout: Layout) {
        let size = NSSize::new(WIDTH, layout.height);
        *self.ivars().layout.borrow_mut() = layout;
        self.setFrameSize(size);
        self.setNeedsDisplay(true);
    }

    pub fn on_click(&self, handler: ClickHandler) {
        *self.ivars().on_click.borrow_mut() = Some(handler);
    }
}

fn ns_rect(r: Rect) -> NSRect {
    NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h))
}

fn srgb((r, g, b): (f64, f64, f64), alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, alpha)
}

/// Whether the appearance being drawn right now best matches DarkAqua.
fn drawing_dark() -> bool {
    // SAFETY: the appearance names are immutable NSString statics.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    NSAppearance::currentDrawingAppearance()
        .bestMatchFromAppearancesWithNames(&names)
        .is_some_and(|best| &*best == dark)
}

fn color(ink: Ink) -> Retained<NSColor> {
    match ink {
        Ink::Primary => NSColor::labelColor(),
        Ink::Secondary => NSColor::secondaryLabelColor(),
        Ink::Tertiary => NSColor::tertiaryLabelColor(),
        Ink::Green => srgb(theme::green(drawing_dark()), 1.0),
        Ink::Orange => srgb(theme::orange(drawing_dark()), 1.0),
    }
}

fn rounded(r: Rect, radius: f64) {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(ns_rect(r), radius, radius).fill();
}

fn paint(shape: &Shape) {
    match shape {
        Shape::Text {
            rect,
            text,
            style,
            ink,
            right,
        } => draw_text(*rect, text, *style, *ink, *right),
        Shape::Cell { rect, level } => {
            let c = if *level == 0 {
                NSColor::quaternaryLabelColor()
            } else {
                srgb(
                    theme::green(drawing_dark()),
                    theme::HEAT_ALPHA[usize::from((*level).min(4))],
                )
            };
            c.setFill();
            rounded(*rect, 2.0);
        }
        Shape::Panel { rect } => {
            NSColor::quaternaryLabelColor().setFill();
            rounded(*rect, 6.0);
        }
        Shape::Dot { rect, ink } => {
            color(*ink).setFill();
            NSBezierPath::bezierPathWithOvalInRect(ns_rect(*rect)).fill();
        }
        Shape::Pill { rect, ink } => {
            color(*ink).colorWithAlphaComponent(0.18).setFill();
            rounded(*rect, rect.h / 2.0);
        }
        Shape::Divider { rect } => {
            NSColor::separatorColor().setFill();
            NSBezierPath::fillRect(ns_rect(*rect));
        }
    }
}

fn font(style: Style) -> Retained<NSFont> {
    // SAFETY: the weight constants are immutable CGFloat statics.
    let (regular, semibold) = unsafe { (NSFontWeightRegular, NSFontWeightSemibold) };
    match style {
        Style::Title => NSFont::systemFontOfSize_weight(15.0, semibold),
        Style::Heading => NSFont::systemFontOfSize_weight(11.0, semibold),
        Style::Body => NSFont::systemFontOfSize_weight(13.0, regular),
        Style::Number => NSFont::monospacedDigitSystemFontOfSize_weight(12.0, regular),
        Style::Meta | Style::CounterLabel => NSFont::systemFontOfSize_weight(11.0, regular),
        Style::CounterValue => NSFont::monospacedDigitSystemFontOfSize_weight(20.0, semibold),
        Style::Badge => NSFont::systemFontOfSize_weight(10.0, semibold),
        Style::Glyph => NSFont::systemFontOfSize_weight(18.0, regular),
        Style::Chevron => NSFont::systemFontOfSize_weight(13.0, regular),
    }
}

fn draw_text(r: Rect, text: &str, style: Style, ink: Ink, right: bool) {
    let para = NSMutableParagraphStyle::new();
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    para.setAlignment(if right {
        NSTextAlignment::Right
    } else if matches!(style, Style::Badge | Style::Glyph | Style::Chevron) {
        NSTextAlignment::Center
    } else {
        NSTextAlignment::Left
    });
    let font = font(style);
    let color = color(ink);
    // SAFETY: the attribute keys are immutable NSString statics.
    let keys: [&NSString; 3] = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        ]
    };
    let values: [&AnyObject; 3] = [&font, &color, &para];
    let attrs = NSDictionary::from_slices(&keys, &values);
    // SAFETY: every attribute value has the type its key expects; called while drawing.
    unsafe {
        NSString::from_str(text).drawWithRect_options_attributes_context(
            ns_rect(r),
            NSStringDrawingOptions::UsesLineFragmentOrigin
                | NSStringDrawingOptions::TruncatesLastVisibleLine,
            Some(&attrs),
            None,
        )
    };
}
