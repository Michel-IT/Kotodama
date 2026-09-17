//! macOS: Kotodama in every app's right-click menu, through the system Services.
//!
//! On Windows there is no way to add an entry to another program's context menu, so there the app recognises a
//! gesture instead (see gesture.rs). macOS has the supported route: the Services submenu that every application
//! shows on a text selection. The user picks Kotodama there and the text arrives here.
//!
//! It takes TWO pieces, and the reason is the part that is not in the documentation (all measured on macOS 15):
//!
//!  1. the service this app provides (`NSServices` in Info.plist + the provider below). It works, but the system
//!     treats a service coming from a third-party APP as restricted: the entry does not appear in the menu until
//!     the user goes and enables it in System Settings, which nobody would ever find.
//!  2. a small Automator service installed in `~/Library/Services` (`install_workflow` below). Services living
//!     there are NOT restricted: the entry shows up at once, in every application. All it does is hand the
//!     selection to piece 1.
//!
//! Hence the two different names: the workflow is called "Kotodama" (what people read in the menu) and this app's
//! own service "Kotodama Transform" (never shown). They MUST differ: the workflow asks the system for the service
//! by name, and with the same name it would find itself and loop.
//!
//! One more condition: the bundle must be SIGNED, at least ad-hoc. From an unsigned .app the system reports every
//! send as successful and the text never arrives. Hence "signingIdentity": "-" in tauri.conf.json.
//!
//! The entry runs the DEFAULT recipe, the same one the main shortcut uses: a Services item is a single command,
//! and a list of recipes would mean a list of entries in every app's menu, which is not a gift to anyone.

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_app_kit::{NSApplication, NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::{MainThreadMarker, NSString};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use tauri::AppHandle;

static APP: OnceLock<AppHandle> = OnceLock::new();
/// The application the text came from, so the answer can be pasted back into it. 0 = nothing to go back to
/// (the keyboard shortcut never leaves the user's application, so there it stays 0 and nothing happens).
static FRONT_PID: AtomicI32 = AtomicI32::new(0);
/// Our own pasteboard type for it: a service carries the selection, not its provenance.
const FRONT_PID_TYPE: &str = "com.kotodama.front-pid";

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
            // Which application the user came from. Performing a service brings US to the front, so without
            // this the answer would be pasted into Kotodama's own window instead of where the text was taken
            // from (measured in Notes: the answer reached the clipboard and the note stayed untouched).
            let pid = unsafe { pboard.stringForType(&NSString::from_str(FRONT_PID_TYPE)) }
                .and_then(|s| s.to_string().trim().parse::<i32>().ok())
                .unwrap_or(0);
            FRONT_PID.store(pid, Ordering::SeqCst);
            crate::debug::log(format!("services: message received, len={} from pid={pid}", text.len()));
            refocus_source_now();
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

/// Brings back the application the Services text came from, once, right before the answer is pasted. Without it
/// the paste lands in Kotodama, which took the foreground when the system performed the service.
pub fn refocus_source() {
    refocus(FRONT_PID.swap(0, Ordering::SeqCst), true);
}

/// The same, but keeping the application in mind for the paste at the end. Used the moment the text arrives:
/// performing a service puts Kotodama in front, and staying there would take the user out of what they were
/// doing and drop their selection while the answer is still being written.
pub fn refocus_source_now() {
    let pid = FRONT_PID.load(Ordering::SeqCst);
    if pid != 0 {
        // A few attempts, not one: the system is bringing US to the front around this very moment, and a single
        // activation can be undone by the one that follows it.
        std::thread::spawn(move || {
            for i in 0..4 {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
                refocus(pid, false);
            }
        });
    }
}

fn refocus(pid: i32, wait: bool) {
    if pid == 0 {
        return;
    }
    let running: Option<Retained<objc2_app_kit::NSRunningApplication>> =
        unsafe { objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid) };
    if let Some(app) = running {
        // 2 = NSApplicationActivateIgnoringOtherApps: we are the front application at this point, so a polite
        // activation would be ignored.
        let _: bool = unsafe { msg_send![&*app, activateWithOptions: 2usize] };
        // Activation is asynchronous, and the paste that follows goes to whichever application is in front WHEN
        // IT IS POSTED: without this wait the answer landed in Kotodama's own window while the text sat in the
        // clipboard (measured in TextEdit). Bounded, so a refused activation cannot hold up the answer.
        let ws = unsafe { objc2_app_kit::NSWorkspace::sharedWorkspace() };
        let mut waited = 0;
        while wait && waited < 2000 {
            let front: Option<Retained<objc2_app_kit::NSRunningApplication>> = unsafe { ws.frontmostApplication() };
            let front_pid = front.map(|f| unsafe { let p: i32 = msg_send![&*f, processIdentifier]; p });
            if front_pid == Some(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 50;
        }
        crate::debug::log(format!("services: focus back to pid={pid} after {waited}ms"));
    }
}

/// The visible half of the integration: a small Automator service in `~/Library/Services`, written when it is
/// missing or out of date (a first run, an update). All it does is take the selected text on standard input and
/// hand it to this app's own service, which is where the real work starts.
fn install_workflow() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
    let dir = home.join("Library/Services/Kotodama.workflow/Contents");
    let info = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>NSServices</key>
  <array>
    <dict>
      <key>NSMenuItem</key><dict><key>default</key><string>Kotodama</string></dict>
      <key>NSMessage</key><string>runWorkflowAsService</string>
      <key>NSSendTypes</key><array><string>NSStringPboardType</string></array>
    </dict>
  </array>
</dict>
</plist>
"#;
    // The Automator document, in the shape its "Run Shell Script" action expects.
    let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>AMApplicationBuild</key><string>534</string>
  <key>AMApplicationVersion</key><string>2.10</string>
  <key>AMDocumentVersion</key><string>2</string>
  <key>actions</key>
  <array>
    <dict>
      <key>action</key>
      <dict>
        <key>AMAccepts</key>
        <dict>
          <key>Container</key><string>List</string>
          <key>Optional</key><true/>
          <key>Types</key><array><string>com.apple.cocoa.string</string></array>
        </dict>
        <key>AMActionVersion</key><string>2.0.3</string>
        <key>AMApplication</key><array><string>Automator</string></array>
        <key>AMParameterProperties</key>
        <dict>
          <key>COMMAND_STRING</key><dict/>
          <key>CheckedForUserDefaultShell</key><dict/>
          <key>inputMethod</key><dict/>
          <key>shell</key><dict/>
          <key>source</key><dict/>
        </dict>
        <key>AMProvides</key>
        <dict>
          <key>Container</key><string>List</string>
          <key>Types</key><array><string>com.apple.cocoa.string</string></array>
        </dict>
        <key>ActionBundlePath</key><string>/System/Library/Automator/Run Shell Script.action</string>
        <key>ActionName</key><string>Run Shell Script</string>
        <key>ActionParameters</key>
        <dict>
          <key>COMMAND_STRING</key><string>__CMD__</string>
          <key>CheckedForUserDefaultShell</key><true/>
          <key>inputMethod</key><integer>0</integer>
          <key>shell</key><string>/bin/zsh</string>
          <key>source</key><string></string>
        </dict>
        <key>BundleIdentifier</key><string>com.apple.RunShellScript</string>
        <key>CFBundleVersion</key><string>2.0.3</string>
        <key>CanShowSelectedItemsWhenRun</key><false/>
        <key>CanShowWhenRun</key><true/>
        <key>Category</key><array><string>AMCategoryUtilities</string></array>
        <key>Class Name</key><string>RunShellScriptAction</string>
        <key>InputUUID</key><string>7C2A1E64-0B61-4F5E-9C2E-4C6E1C4B5A01</string>
        <key>OutputUUID</key><string>7C2A1E64-0B61-4F5E-9C2E-4C6E1C4B5A02</string>
        <key>UUID</key><string>7C2A1E64-0B61-4F5E-9C2E-4C6E1C4B5A03</string>
        <key>UnlocalizedApplications</key><array><string>Automator</string></array>
        <key>arguments</key>
        <dict>
          <key>0</key><dict><key>default value</key><integer>0</integer><key>name</key><string>inputMethod</string><key>required</key><string>0</string><key>type</key><string>0</string><key>uuid</key><string>0</string></dict>
          <key>1</key><dict><key>default value</key><false/><key>name</key><string>CheckedForUserDefaultShell</string><key>required</key><string>0</string><key>type</key><string>0</string><key>uuid</key><string>1</string></dict>
          <key>2</key><dict><key>default value</key><string></string><key>name</key><string>source</string><key>required</key><string>0</string><key>type</key><string>0</string><key>uuid</key><string>2</string></dict>
          <key>3</key><dict><key>default value</key><string></string><key>name</key><string>COMMAND_STRING</string><key>required</key><string>0</string><key>type</key><string>0</string><key>uuid</key><string>3</string></dict>
          <key>4</key><dict><key>default value</key><string>/bin/sh</string><key>name</key><string>shell</string><key>required</key><string>0</string><key>type</key><string>0</string><key>uuid</key><string>4</string></dict>
        </dict>
        <key>conversionLabel</key><integer>0</integer>
        <key>isViewVisible</key><integer>1</integer>
        <key>location</key><string>309.000000:305.000000</string>
        <key>nibPath</key><string>/System/Library/Automator/Run Shell Script.action/Contents/Resources/Base.lproj/main.nib</string>
      </dict>
      <key>isViewVisible</key><integer>1</integer>
    </dict>
  </array>
  <key>connectors</key><dict/>
  <key>workflowMetaData</key>
  <dict>
    <key>applicationBundleIDsByPath</key><dict/>
    <key>applicationPaths</key><array/>
    <key>inputTypeIdentifier</key><string>com.apple.Automator.text</string>
    <key>outputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
    <key>presentationMode</key><integer>11</integer>
    <key>processesInput</key><false/>
    <key>serviceInputTypeIdentifier</key><string>com.apple.Automator.text</string>
    <key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
    <key>serviceProcessesInput</key><false/>
    <key>systemImageName</key><string>NSActionTemplate</string>
    <key>useAutomaticInputType</key><false/>
    <key>workflowTypeIdentifier</key><string>com.apple.Automator.servicesMenu</string>
  </dict>
</dict>
</plist>
"#;
    let doc = doc.replace("__CMD__", CMD);
    let same = |path: &std::path::Path, want: &str| {
        std::fs::read_to_string(path).map(|cur| cur == want).unwrap_or(false)
    };
    let info_path = dir.join("Info.plist");
    let doc_path = dir.join("document.wflow");
    if same(&info_path, info) && same(&doc_path, &doc) {
        return; // already there and current: no write, and no reason to rebuild the menus
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ok = std::fs::write(&info_path, info).is_ok() && std::fs::write(&doc_path, &doc).is_ok();
    crate::debug::log(format!("services: workflow installed ok={ok}"));
}

/// What the workflow runs: read the selection from standard input, note WHICH application the user is in, put
/// both on a private pasteboard and perform this app's own service on it. JavaScript for Automation is part of
/// macOS, so this needs nothing installed and asks for no permission.
const CMD: &str = r##"V="$(cat)" osascript -l JavaScript -e 'ObjC.import("stdlib"); ObjC.import("AppKit"); function run(){ var t=$.NSString.stringWithUTF8String($.getenv("V")); var front=$.NSWorkspace.sharedWorkspace.frontmostApplication; var pb=$.NSPasteboard.pasteboardWithUniqueName; pb.clearContents; pb.setStringForType(t, $.NSPasteboardTypeString); if (!front.isNil()) pb.setStringForType(String(front.processIdentifier), "com.kotodama.front-pid"); return $.NSPerformService($("Kotodama Transform"), pb); }'"##;

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
    install_workflow();
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
