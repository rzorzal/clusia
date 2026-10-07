//! The real poster: `UNUserNotificationCenter`. Only usable inside an app bundle whose main
//! executable is this process; nothing here runs in tests.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSDictionary, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationInterruptionLevel, UNNotificationPresentationOptions, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::{Interruption, Notification, OPEN_KEY, PermissionStatus, Poster, sound_file};

type OnOpen = Box<dyn Fn(String) + Send + Sync>;

struct DelegateIvars {
    on_open: OnOpen,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `CenterDelegate` does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateIvars]
    struct CenterDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for CenterDelegate {}

    // SAFETY: the method signatures match UNUserNotificationCenterDelegate.
    unsafe impl UNUserNotificationCenterDelegate for CenterDelegate {
        /// The tray is always the foreground app, so without this macOS would swallow the banner.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &block2::DynBlock<dyn Fn()>,
        ) {
            let info = response.notification().request().content().userInfo();
            let key = NSString::from_str(OPEN_KEY);
            if let Some(value) = info.objectForKey(&key)
                && let Ok(text) = value.downcast::<NSString>()
            {
                (self.ivars().on_open)(text.to_string());
            }
            done.call(());
        }
    }
);

impl CenterDelegate {
    fn new(mtm: MainThreadMarker, on_open: OnOpen) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars { on_open });
        // SAFETY: `init` is NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

fn status_of(s: UNAuthorizationStatus) -> PermissionStatus {
    if s == UNAuthorizationStatus::Authorized
        || s == UNAuthorizationStatus::Provisional
        || s == UNAuthorizationStatus::Ephemeral
    {
        PermissionStatus::Allowed
    } else if s == UNAuthorizationStatus::Denied {
        PermissionStatus::Denied
    } else {
        PermissionStatus::NotDetermined
    }
}

/// Posts through the system notification center. `on_open` gets the `userInfo.open` JSON of a
/// clicked notification; `on_status` gets every permission status read or granted. Both
/// run on an arbitrary thread (the callers hop to the main one).
pub struct UNPoster {
    center: Retained<UNUserNotificationCenter>,
    // The center keeps only a weak reference to its delegate.
    _delegate: Retained<CenterDelegate>,
    on_status: Arc<dyn Fn(PermissionStatus) + Send + Sync>,
    last: Arc<Mutex<PermissionStatus>>,
}

impl UNPoster {
    /// Registers the delegate: do it before the run loop starts, so a click that launched the
    /// app is delivered.
    pub fn new(
        mtm: MainThreadMarker,
        on_open: impl Fn(String) + Send + Sync + 'static,
        on_status: impl Fn(PermissionStatus) + Send + Sync + 'static,
    ) -> Self {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = CenterDelegate::new(mtm, Box::new(on_open));
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let last = Arc::new(Mutex::new(PermissionStatus::NotDetermined));
        let remember = last.clone();
        Self {
            center,
            _delegate: delegate,
            on_status: Arc::new(move |status| {
                if let Ok(mut last) = remember.lock() {
                    *last = status;
                }
                on_status(status);
            }),
            last,
        }
    }
}

impl Poster for UNPoster {
    fn post(&self, n: &Notification, open_json: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&n.title));
        content.setSubtitle(&NSString::from_str(&n.subtitle));
        content.setBody(&NSString::from_str(&n.body));
        if let Some(sound) = &n.sound {
            // The bundle's `Resources/<id>.aiff`.
            let name = NSString::from_str(&sound_file(sound));
            content.setSound(Some(&UNNotificationSound::soundNamed(&name)));
        }
        // Time-sensitive only breaks through a Focus where macOS grants it to the app.
        content.setInterruptionLevel(match n.interruption() {
            Interruption::Active => UNNotificationInterruptionLevel::Active,
            Interruption::TimeSensitive => UNNotificationInterruptionLevel::TimeSensitive,
        });
        let key = NSString::from_str(OPEN_KEY);
        let value = NSString::from_str(open_json);
        let info: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_retained_objects(&[&*key], &[value.into()]);
        // SAFETY: the dictionary holds only property-list values (one string).
        unsafe { content.setUserInfo(info.cast_unchecked::<AnyObject, AnyObject>()) };
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&n.id),
            &content,
            None,
        );
        let id = n.id.clone();
        let done = RcBlock::new(move |error: *mut NSError| {
            if !error.is_null() {
                tracing::warn!(%id, "the notification was not posted");
            }
        });
        self.center
            .addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }

    fn status(&self) -> PermissionStatus {
        self.last.lock().map(|s| *s).unwrap_or_default()
    }

    fn request_authorization(&self) {
        let on_status = self.on_status.clone();
        let done = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            on_status(if granted.as_bool() {
                PermissionStatus::Allowed
            } else {
                PermissionStatus::Denied
            });
        });
        self.center
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &done,
            );
    }

    fn refresh_status(&self) {
        let on_status = self.on_status.clone();
        let done = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: the system passes a valid settings object for the call's duration.
            let settings = unsafe { settings.as_ref() };
            on_status(status_of(settings.authorizationStatus()));
        });
        self.center
            .getNotificationSettingsWithCompletionHandler(&done);
    }
}
