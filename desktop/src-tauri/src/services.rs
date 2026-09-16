//! macOS: Kotodama in every app's right-click menu, through the system Services.
//!
//! On Windows there is no way to add an entry to another program's context menu, so there the app recognises a
//! gesture instead (see gesture.rs). macOS has the supported route: an app declares in its Info.plist that it
//! knows how to handle selected text, and the system puts it in the Services submenu of EVERY application's
//! right-click menu. The user picks it on the selection and the text comes here.
//!
//! One condition is not obvious and was measured on macOS 15: the bundle must be SIGNED, at least ad-hoc.
//! An unsigned .app registers in the Services menu and the system reports the send as successful, but the text
//! never arrives. Hence "signingIdentity": "-" in tauri.conf.json.
//!
//! The entry runs the DEFAULT recipe, the same one the main shortcut uses: a Services item is a single command,
//! and a list of recipes would mean a list of entries in every app's menu, which is not a gift to anyone.

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_app_kit::{NSApplication, NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::{MainThreadMarker, NSString};
use std::sync::OnceLock;
use tauri::AppHandle;

static APP: OnceLock<AppHandle> = OnceLock::new();

define_class!(
    // The object macOS talks to. It only reads the pasteboard the system fills with the selection and hands the
    // text to the usual transform, so the Services route and the keyboard shortcut end up in the same place.
    #[unsafe(super(NSObject))]
    #[name = "KotodamaServiceProvider"]
    struct ServiceProvider;

    impl ServiceProvider {
        #[unsafe(method(kotodamaTransform:userData:error:))]
        fn kotodama_transform(&self, pboard: &NSPasteboard, _user_data: *mut NSString, _error: *mut *mut NSString) {
            let text = unsafe { pboard.stringForType(NSPasteboardTypeString) }
                .map(|s| s.to_string())
                .unwrap_or_default();
            crate::debug::log(format!("services: message received, len={}", text.len()));
            if text.trim().is_empty() {
                return;
            }
            if let Some(app) = APP.get() {
                crate::inline_transform_text(app.clone(), text);
            }
        }
    }
);

/// Keeps the handle the service method needs. Called early, while the rest of the app is being built.
pub fn set_app(app: &AppHandle) {
    let _ = APP.set(app.clone());
}

/// Registers the provider and tells macOS to re-read the Services of this bundle. Call it when the application
/// is ready, not before: an earlier registration is lost while AppKit finishes launching.
/// Failing is not fatal, it just means the entry does not appear.
pub fn register(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let Some(mtm) = MainThreadMarker::new() else { return };
    let provider: Retained<ServiceProvider> = unsafe { msg_send![ServiceProvider::alloc(), init] };
    let ns_app = NSApplication::sharedApplication(mtm);
    unsafe { ns_app.setServicesProvider(Some(&*provider)) };
    // Also register the provider as a listening port. setServicesProvider alone was NOT enough: measured on
    // macOS 15, the system delivered nothing until this call was there too.
    let port = NSString::from_str("Kotodama");
    unsafe { NSRegisterServicesProvider(&*provider as *const ServiceProvider as *const NSObject, &*port) };
    // Without this the Services menu keeps the previous list until the next login.
    unsafe { NSUpdateDynamicServices() };
    std::mem::forget(provider); // the app keeps it for its whole life
    crate::debug::log("services: provider registered");
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    fn NSUpdateDynamicServices();
    fn NSRegisterServicesProvider(provider: *const NSObject, port_name: *const NSString);
}
