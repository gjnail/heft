//! Desktop notifications (macOS). From Heft.app they go through the
//! notification center, and clicking one opens Heft's window, like the
//! balloon on Windows. The notification center only works for apps in a
//! bundle, so a bare binary (`cargo run`) uses AppleScript's `display
//! notification` instead.

use std::sync::Once;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

/// Heft's bundle identifier when it runs from its app bundle.
pub fn bundle_id() -> Option<String> {
    NSBundle::mainBundle().bundleIdentifier().map(|id| id.to_string())
}

pub fn send(title: &str, body: &str) {
    if bundle_id().is_some() {
        send_to_center(title, body);
    } else {
        let script = format!(
            "display notification {} with title \"Heft\" subtitle {}",
            super::applescript_quote(body),
            super::applescript_quote(title)
        );
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Ask for permission (macOS asks you the first time only, so nothing is
/// asked until there's something to say), then post it.
fn send_to_center(title: &str, body: &str) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    set_delegate(&center);
    let (title, body) = (title.to_string(), body.to_string());
    let post = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        if !granted.as_bool() {
            return;
        }
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&title));
        content.setBody(&NSString::from_str(&body));
        // The title names the drive, so a newer warning replaces an older one.
        let id = NSString::from_str(&format!("heft.low-space.{title}"));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
    });
    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert, &post);
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate doesn't
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "HeftNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Show it even when Heft is the app in front.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
        }

        /// Clicked: open Heft's window. This may come on any thread.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            _response: &UNNotificationResponse,
            handler: &DynBlock<dyn Fn()>,
        ) {
            dispatch2::DispatchQueue::main().exec_async(super::menubar::show_window);
            handler.call(());
        }
    }
);

fn set_delegate(center: &UNUserNotificationCenter) {
    static SET: Once = Once::new();
    SET.call_once(|| {
        let delegate: Retained<Delegate> = unsafe { msg_send![super(Delegate::alloc().set_ivars(())), init] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center only keeps a weak reference, and it's needed for good.
        std::mem::forget(delegate);
    });
}
