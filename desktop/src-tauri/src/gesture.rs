//! Double right-click anywhere: the recipe menu, on any program.
//!
//! Windows has no way to add an entry to another program's context menu: every program draws its own (Win32
//! menus, XAML, Chromium in HTML), and the shell's context-menu extensions only apply to files in Explorer.
//! What CAN be done is to recognise a gesture no program uses, and open OUR menu for it. A second right-click
//! within `DOUBLE_MS` and a few pixels of the first is that gesture: the first click still opens the program's
//! own menu (nothing is taken away), the second is swallowed and replaced by Kotodama's recipes.
//!
//! macOS gets the supported route instead (a system Service in every app's right-click menu), so this whole
//! module is Windows only.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, MSG,
        MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_RBUTTONDOWN,
    };

    /// Two right-clicks closer than this are one gesture. Windows' own double-click time is the honest
    /// reference, but it is not readable per-user without a system call on every click, so this is its default.
    const DOUBLE_MS: u64 = 500;
    /// The mouse may drift a little between the two clicks; further than this and they are two separate clicks.
    const SLOP_PX: i32 = 8;

    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static LAST_X: AtomicI32 = AtomicI32::new(0);
    static LAST_Y: AtomicI32 = AtomicI32::new(0);
    static ENABLED: AtomicBool = AtomicBool::new(true);

    type Handler = Box<dyn Fn(i32, i32) + Send + Sync>;
    static ON_GESTURE: OnceLock<Handler> = OnceLock::new();

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && wparam.0 as u32 == WM_RBUTTONDOWN && ENABLED.load(Ordering::Relaxed) {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let (x, y, t) = (info.pt.x, info.pt.y, now_ms());
            let near = (x - LAST_X.load(Ordering::Relaxed)).abs() <= SLOP_PX
                && (y - LAST_Y.load(Ordering::Relaxed)).abs() <= SLOP_PX;
            if near && t.saturating_sub(LAST_MS.load(Ordering::Relaxed)) <= DOUBLE_MS {
                LAST_MS.store(0, Ordering::Relaxed); // a third click starts a new gesture
                if let Some(f) = ON_GESTURE.get() {
                    f(x, y);
                }
                return LRESULT(1); // swallowed: the program must not open a second menu under ours
            }
            LAST_MS.store(t, Ordering::Relaxed);
            LAST_X.store(x, Ordering::Relaxed);
            LAST_Y.store(y, Ordering::Relaxed);
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// Installs the hook on its own thread: a low-level mouse hook only delivers events to a thread that runs a
    /// message loop, and the app's own loop must stay free.
    pub fn start(on_gesture: Handler) {
        if ON_GESTURE.set(on_gesture).is_err() {
            return; // already started
        }
        std::thread::spawn(|| unsafe {
            let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(hook), None, 0) {
                Ok(h) => h,
                Err(e) => {
                    crate::debug::log(format!("gesture: SetWindowsHookEx failed: {e}"));
                    return;
                }
            };
            crate::debug::log(format!("gesture: mouse hook installed {hook:?}"));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    }

    pub fn set_enabled(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
    }
}

#[cfg(windows)]
pub use imp::{set_enabled, start};

#[cfg(not(windows))]
pub fn start(_on_gesture: Box<dyn Fn(i32, i32) + Send + Sync>) {}
#[cfg(not(windows))]
pub fn set_enabled(_on: bool) {}
