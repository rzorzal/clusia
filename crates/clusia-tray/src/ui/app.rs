//! Menu bar item + popover, fed by `data` on a background runtime (spec §7.3).
//! All AppKit state lives on the main thread in `UI`; the runtime hops there with GCD.

use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clusia_core::Paths;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSPopover,
    NSPopoverBehavior, NSPopoverDelegate, NSStatusBar, NSStatusItem, NSViewController,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRectEdge, NSSize,
};

use crate::actions;
use crate::data::{self, Update};
use crate::layout::{WIDTH, layout};
use crate::model::TrayModel;
use crate::ui::icon;
use crate::ui::paint::ContentView;

struct Ui {
    model: TrayModel,
    item: Retained<NSStatusItem>,
    popover: Retained<NSPopover>,
    content: Retained<ContentView>,
    paths: Paths,
    app_bin: Option<PathBuf>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Ui {
    /// Re-lays out the popover and updates the icon's dot.
    fn render(&self, mtm: MainThreadMarker) {
        let view = self.model.view(now());
        let l = layout(&view);
        let size = NSSize::new(WIDTH, l.height);
        self.content.set_layout(l);
        self.content.setFrameOrigin(NSPoint::new(0.0, 0.0));
        self.popover.setContentSize(size);
        if let Some(button) = self.item.button(mtm) {
            button.setImage(Some(&icon::status_image(view.has_news)));
        }
    }
}

/// Runs on the main queue.
fn deliver(update: Update) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    match update {
        Update::Snapshot(snapshot) => UI.with_borrow_mut(|ui| {
            if let Some(ui) = ui {
                ui.model.apply(snapshot);
                ui.render(mtm);
            }
        }),
        Update::Quit(reason) => {
            tracing::info!(%reason, "exiting");
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
}

/// Ends the process with a failure status, so the daemon's supervisor restarts the tray.
fn fail(reason: &str) -> ! {
    tracing::error!(%reason, "exiting with failure");
    std::process::exit(1);
}

fn deliver_on_main(update: Update) {
    DispatchQueue::main().exec_async(move || deliver(update));
}

#[derive(Default)]
struct DelegateIvars {
    setup: RefCell<Option<(Paths, Option<PathBuf>)>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Delegate` does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    struct Delegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the method signature matches NSApplicationDelegate.
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            let Some((paths, app_bin)) = self.ivars().setup.take() else { return };
            let item = NSStatusBar::systemStatusBar().statusItemWithLength(icon::ITEM_LENGTH);
            if let Some(button) = item.button(mtm) {
                button.setImage(Some(&icon::status_image(false)));
                // SAFETY: the delegate lives for the whole run of the app, so the target outlives the button.
                unsafe {
                    button.setTarget(Some(self));
                    button.setAction(Some(sel!(toggle:)));
                }
            }
            let model = TrayModel::new(app_bin.is_some());
            let content = ContentView::new(mtm, false);
            let first = layout(&model.view(now()));
            let size = NSSize::new(WIDTH, first.height);
            content.set_layout(first);
            let effect = NSVisualEffectView::initWithFrame(
                NSVisualEffectView::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), size),
            );
            effect.setMaterial(NSVisualEffectMaterial::Popover);
            effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            effect.setState(NSVisualEffectState::Active);
            effect.addSubview(&content);
            let controller = NSViewController::new(mtm);
            controller.setView(&effect);
            let popover = NSPopover::new(mtm);
            popover.setContentViewController(Some(&controller));
            popover.setContentSize(size);
            popover.setBehavior(NSPopoverBehavior::Transient);
            popover.setAnimates(true);
            popover.setDelegate(Some(ProtocolObject::from_ref(self)));
            content.on_click(Box::new(|action| {
                // Short borrow: closing the popover re-enters UI through popoverDidClose:.
                let target = UI.with_borrow(|ui| {
                    ui.as_ref().map(|ui| {
                        (actions::plan(&action, ui.app_bin.as_deref(), &ui.paths), ui.popover.clone())
                    })
                });
                if let Some((plan, popover)) = target {
                    // SAFETY: called on the main thread with no sender.
                    unsafe { popover.performClose(None) };
                    if let Some(launch) = plan {
                        actions::launch(&launch);
                    }
                }
            }));
            let socket = paths.socket();
            UI.set(Some(Ui { model, item, popover, content, paths, app_bin }));
            // Start the data loop only now, so no update can arrive before `UI` exists.
            let spawned = std::thread::Builder::new().name("clusia-tray-data".into()).spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => {
                        let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            rt.block_on(data::run(socket, deliver_on_main))
                        }));
                        if run.is_err() {
                            // Non-zero so the supervisor restarts us; exit 0 is reserved for "daemon gone".
                            fail("the data loop panicked");
                        }
                    }
                    Err(e) => fail(&format!("cannot start the runtime: {e}")),
                }
            });
            if let Err(e) = spawned {
                fail(&format!("cannot start the data thread: {e}"));
            }
        }
    }

    // SAFETY: the method signature matches NSPopoverDelegate.
    unsafe impl NSPopoverDelegate for Delegate {
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            UI.with_borrow_mut(|ui| {
                if let Some(ui) = ui {
                    ui.model.mark_seen();
                    ui.render(mtm);
                }
            });
        }
    }

    impl Delegate {
        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: Option<&AnyObject>) {
            let mtm = self.mtm();
            let target = UI.with_borrow(|ui| ui.as_ref().map(|ui| (ui.popover.clone(), ui.item.clone())));
            let Some((popover, item)) = target else { return };
            if popover.isShown() {
                // SAFETY: called on the main thread with no sender.
                unsafe { popover.performClose(None) };
                return;
            }
            // Fresh ages ("2m") at open time.
            UI.with_borrow(|ui| {
                if let Some(ui) = ui {
                    ui.render(mtm);
                }
            });
            if let Some(button) = item.button(mtm) {
                #[allow(deprecated)] // needed so the transient popover closes on outside clicks
                NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
                popover.showRelativeToRect_ofView_preferredEdge(button.bounds(), &button, NSRectEdge::MinY);
            }
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, paths: Paths, app_bin: Option<PathBuf>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            setup: RefCell::new(Some((paths, app_bin))),
        });
        // SAFETY: `init` is NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Runs the menu bar tray until the daemon goes away.
pub fn run(mtm: MainThreadMarker, paths: Paths, app_bin: Option<PathBuf>) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(mtm, paths, app_bin);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}
