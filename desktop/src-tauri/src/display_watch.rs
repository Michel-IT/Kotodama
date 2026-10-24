//! Windows only: keeps the main window reachable after the monitor layout changes.
//!
//! Waking from standby with two monitors often brings one of them back late: Windows first moves every
//! window onto the primary monitor, then the second monitor appears. A window that ends up outside every
//! work area (or whose restored rectangle does, so a later Restore/Maximize throws it off screen) is
//! brought back into the primary work area. tao 0.37.1 handles WM_DPICHANGED only: WM_DISPLAYCHANGE and
//! WM_POWERBROADCAST are not handled there, so they are caught here with a window subclass.
//!
//! The reaction is debounced (the late monitor must have arrived) and never touches a window that is
//! parked off screen for an inline transform.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, SetWindowPlacement, SetWindowPos,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWMAXIMIZED, WINDOWPLACEMENT,
};

use crate::{debug, win_is_parked, win_sync_visible, AppState};

const WM_DISPLAYCHANGE: u32 = 0x007E;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
/// Subclass id ("KODS"), distinct from the lone-Alt guard's.
const SUBCLASS_ID: usize = 0x4B4F_4453;
/// Quiet time after the last event before acting: the late monitor has to be back.
const DEBOUNCE: Duration = Duration::from_millis(2500);
/// Smallest overlap with a work area that still counts as "reachable" (enough title bar to grab).
const MIN_VISIBLE_W: i32 = 48;
const MIN_VISIBLE_H: i32 = 32;

/// Plain rectangle (screen coordinates), kept free of Win32 types so the decision logic is testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Is enough of `rect` inside at least one of `areas` for the user to reach it?
pub fn rect_visible_in_any(rect: Rect, areas: &[Rect]) -> bool {
    areas.iter().any(|a| {
        let w = rect.right.min(a.right) - rect.left.max(a.left);
        let h = rect.bottom.min(a.bottom) - rect.top.max(a.top);
        // A window smaller than the threshold only needs to be fully inside.
        w >= MIN_VISIBLE_W.min(rect.width()).max(1) && h >= MIN_VISIBLE_H.min(rect.height()).max(1)
    })
}

/// Top-left position that puts `rect` (same size) inside `area`. If the window is larger than the
/// area, it is aligned to the area's top-left so the title bar stays reachable.
pub fn clamp_into_work_area(rect: Rect, area: Rect) -> (i32, i32) {
    let x = if rect.width() >= area.width() { area.left } else { rect.left.clamp(area.left, area.right - rect.width()) };
    let y = if rect.height() >= area.height() { area.top } else { rect.top.clamp(area.top, area.bottom - rect.height()) };
    (x, y)
}

/// A maximized (not minimized) window whose real rectangle is reachable on no monitor.
pub fn maximized_real_rect_lost(zoomed: bool, iconic: bool, real: Rect, full: &[Rect]) -> bool {
    zoomed && !iconic && !rect_visible_in_any(real, full)
}

/// Restored rectangle (workspace coordinates) moved into the primary work area `target`, same size;
/// `offset` is the primary work-area origin relative to its monitor origin.
pub fn restored_rect_in_primary(normal: Rect, target: Rect, offset: (i32, i32)) -> Rect {
    let (x, y) = clamp_into_work_area(normal, target);
    let (l, t) = (x - offset.0, y - offset.1);
    Rect { left: l, top: t, right: l + normal.width(), bottom: t + normal.height() }
}

/// Brings a window that is outside every monitor back into the primary work area (same size).
/// A normal window is tested by its own rectangle; a maximized or minimized one by its restored
/// rectangle (in workspace coordinates, so the test uses the full monitor rectangles and the target is
/// shifted back). Returns true when something was moved. The maximized/minimized state is kept.
#[allow(clippy::too_many_arguments)]
unsafe fn fix_position(
    hwnd: HWND,
    zoomed: bool,
    iconic: bool,
    wr: RECT,
    pl: &mut WINDOWPLACEMENT,
    have_pl: bool,
    full: &[Rect],
    work: &[Rect],
    target: Rect,
    primary: Option<&tauri::Monitor>,
) -> bool {
    let mut changed = false;
    if !zoomed && !iconic {
        // Normal window: it must overlap a work area, else move it into the primary one (size kept).
        let rect = to_rect(wr);
        if !rect_visible_in_any(rect, work) {
            let (x, y) = clamp_into_work_area(rect, target);
            debug::log(format!("DISPLAY moving main window {rect:?} -> ({x},{y})"));
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            changed = true;
        }
    } else if have_pl {
        // Maximized or minimized. The restored rectangle can be wrong, and a maximized window can also
        // sit outside every monitor with a perfectly good restored rectangle (moved there while hidden
        // in the tray), so the real rectangle is tested too.
        let normal = to_rect(pl.rcNormalPosition);
        let real_lost = maximized_real_rect_lost(zoomed, iconic, to_rect(wr), full);
        if real_lost || !rect_visible_in_any(normal, full) {
            let (ox, oy) = match primary {
                Some(m) => (m.work_area().position.x - m.position().x, m.work_area().position.y - m.position().y),
                None => (0, 0),
            };
            let fixed = restored_rect_in_primary(normal, target, (ox, oy));
            pl.rcNormalPosition = RECT { left: fixed.left, top: fixed.top, right: fixed.right, bottom: fixed.bottom };
            if real_lost {
                // Re-anchor: a maximized window follows the monitor of its restored rectangle.
                pl.showCmd = SW_SHOWMAXIMIZED.0 as u32;
            }
            debug::log(format!("DISPLAY fixing restored rect {normal:?} -> {fixed:?} (real rect lost: {real_lost})"));
            let _ = SetWindowPlacement(hwnd, pl);
            changed = true;
        }
    }
    changed
}

/// Called right before the main window is shown again (tray, shortcut, recall): if it is hidden
/// outside every current monitor (monitor unplugged or layout changed while it sat in the tray), puts
/// it back in the primary work area first. A window parked off screen for an inline transform is left
/// alone. Must run on the main thread. Returns true when the position was corrected.
pub(crate) fn ensure_reachable(w: &tauri::Window) -> bool {
    let Ok(h) = w.hwnd() else { return false };
    let raw = h.0 as isize;
    if win_is_parked(raw) {
        return false;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let monitors = w.available_monitors().unwrap_or_default();
    if monitors.is_empty() {
        return false;
    }
    let mon_rect = |p: &tauri::PhysicalPosition<i32>, s: &tauri::PhysicalSize<u32>| Rect {
        left: p.x,
        top: p.y,
        right: p.x + s.width as i32,
        bottom: p.y + s.height as i32,
    };
    let full: Vec<Rect> = monitors.iter().map(|m| mon_rect(m.position(), m.size())).collect();
    let work: Vec<Rect> = monitors.iter().map(|m| mon_rect(&m.work_area().position, &m.work_area().size)).collect();
    let primary = w.primary_monitor().ok().flatten();
    let target = primary
        .as_ref()
        .map(|m| mon_rect(&m.work_area().position, &m.work_area().size))
        .or_else(|| work.first().copied());
    let Some(target) = target else { return false };
    unsafe {
        let zoomed = IsZoomed(hwnd).as_bool();
        let iconic = IsIconic(hwnd).as_bool();
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let mut pl = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let have_pl = GetWindowPlacement(hwnd, &mut pl).is_ok();
        fix_position(hwnd, zoomed, iconic, wr, &mut pl, have_pl, &full, &work, target, primary.as_ref())
    }
}

static TRIGGER: OnceLock<Sender<()>> = OnceLock::new();

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let relevant = msg == WM_DISPLAYCHANGE
        || (msg == WM_POWERBROADCAST && (wparam.0 == PBT_APMRESUMEAUTOMATIC || wparam.0 == PBT_APMRESUMESUSPEND));
    if relevant {
        if let Some(tx) = TRIGGER.get() {
            let _ = tx.send(());
        }
    }
    // Never swallowed: tao and the webviews still see the message.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Subclasses the main window and starts the debounce worker. Call once, after the window exists.
pub fn install(app: &AppHandle, hwnd_raw: isize) {
    let (tx, rx) = channel::<()>();
    if TRIGGER.set(tx).is_err() {
        return;
    }
    // Not undone with RemoveWindowSubclass: the main window is created once and only ever hidden.
    let ok = unsafe {
        SetWindowSubclass(HWND(hwnd_raw as *mut core::ffi::c_void), Some(subclass_proc), SUBCLASS_ID, 0)
    };
    if !ok.as_bool() {
        debug::log(format!("display watch: SetWindowSubclass failed for hwnd {hwnd_raw:#x}"));
        return;
    }
    let app = app.clone();
    // One worker, one job at a time: a burst of events restarts the quiet period instead of stacking jobs.
    std::thread::spawn(move || loop {
        if rx.recv().is_err() {
            return;
        }
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(()) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let (done_tx, done_rx) = channel::<()>();
        let app2 = app.clone();
        if app
            .run_on_main_thread(move || {
                run_job(&app2);
                let _ = done_tx.send(());
            })
            .is_err()
        {
            return;
        }
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
    });
}

fn to_rect(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

/// Runs on the main thread, once the layout has been quiet for `DEBOUNCE`.
fn run_job(app: &AppHandle) {
    let Some(w) = app.get_window("main") else { return };
    let Ok(h) = w.hwnd() else { return };
    let raw = h.0 as isize;
    let hwnd = HWND(raw as *mut core::ffi::c_void);

    // Parked for an inline transform (or being restored from it): that code owns the position.
    let transforming = app.state::<AppState>().inline_saved_pos.lock().unwrap().is_some();
    if transforming || win_is_parked(raw) {
        debug::log("DISPLAY change: main window parked or inline transform running, nothing done");
        return;
    }

    let monitors = w.available_monitors().unwrap_or_default();
    let mon_rect = |p: &tauri::PhysicalPosition<i32>, s: &tauri::PhysicalSize<u32>| Rect {
        left: p.x,
        top: p.y,
        right: p.x + s.width as i32,
        bottom: p.y + s.height as i32,
    };
    let full: Vec<Rect> = monitors.iter().map(|m| mon_rect(m.position(), m.size())).collect();
    let work: Vec<Rect> =
        monitors.iter().map(|m| mon_rect(&m.work_area().position, &m.work_area().size)).collect();
    let primary = w.primary_monitor().ok().flatten();

    unsafe {
        let visible = IsWindowVisible(hwnd).as_bool();
        let zoomed = IsZoomed(hwnd).as_bool();
        let iconic = IsIconic(hwnd).as_bool();
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let mut pl = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        let have_pl = GetWindowPlacement(hwnd, &mut pl).is_ok();

        if debug::enabled() {
            for (i, m) in monitors.iter().enumerate() {
                debug::log(format!(
                    "DISPLAY monitor[{i}]: rect={:?} work={:?} scale={}",
                    full[i],
                    work[i],
                    m.scale_factor()
                ));
            }
            debug::log(format!(
                "DISPLAY main window: rect={:?} normal={:?} visible={visible} zoomed={zoomed} iconic={iconic} \
                 tao_is_visible={:?}",
                to_rect(wr),
                to_rect(pl.rcNormalPosition),
                w.is_visible()
            ));
        }

        // Monitors still being enumerated, or a hidden window (tray): nothing to correct safely.
        if work.is_empty() || !visible {
            return;
        }
        let Some(target) = primary.as_ref().map(|m| mon_rect(&m.work_area().position, &m.work_area().size)).or_else(|| work.first().copied())
        else {
            return;
        };

        let changed = fix_position(hwnd, zoomed, iconic, wr, &mut pl, have_pl, &full, &work, target, primary.as_ref());

        // tao's own visible flag must follow the real one, or a later Maximize hides the window.
        // Only after a correction: show() activates, and a plain wake-up must not steal the focus.
        if changed && !iconic {
            win_sync_visible(&w, true, "display_change");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> Rect {
        Rect { left: l, top: t, right: rr, bottom: b }
    }

    #[test]
    fn hidden_far_offscreen_window_is_moved_into_primary() {
        let areas = [r(0, 0, 1920, 1040)];
        let win = r(5000, 3000, 6200, 3800);
        assert!(!rect_visible_in_any(win, &areas));
        let (x, y) = clamp_into_work_area(win, areas[0]);
        assert!(rect_visible_in_any(r(x, y, x + 1200, y + 800), &areas));
        assert_eq!((x, y), (720, 240));
    }

    #[test]
    fn visible_on_second_monitor() {
        let areas = [r(0, 0, 1920, 1040), r(1920, 0, 3840, 1080)];
        assert!(rect_visible_in_any(r(2000, 100, 3000, 800), &areas));
    }

    #[test]
    fn not_visible_when_second_monitor_gone() {
        let areas = [r(0, 0, 1920, 1040)];
        assert!(!rect_visible_in_any(r(2000, 100, 3000, 800), &areas));
    }

    #[test]
    fn sliver_overlap_is_not_reachable() {
        let areas = [r(0, 0, 1920, 1040)];
        assert!(!rect_visible_in_any(r(1900, 100, 2900, 800), &areas));
        assert!(rect_visible_in_any(r(1800, 100, 2800, 800), &areas));
    }

    #[test]
    fn negative_coordinates_monitor() {
        let areas = [r(-1920, 0, 0, 1080), r(0, 0, 1920, 1040)];
        assert!(rect_visible_in_any(r(-1500, 50, -500, 700), &areas));
    }

    #[test]
    fn clamp_keeps_size_and_moves_inside() {
        let area = r(0, 0, 1920, 1040);
        assert_eq!(clamp_into_work_area(r(2500, 300, 3500, 900), area), (920, 300));
        assert_eq!(clamp_into_work_area(r(-800, -200, 200, 400), area), (0, 0));
        assert_eq!(clamp_into_work_area(r(100, 100, 600, 500), area), (100, 100));
    }

    #[test]
    fn clamp_oversized_window_aligns_top_left() {
        let area = r(0, 0, 1280, 720);
        assert_eq!(clamp_into_work_area(r(3000, 2000, 5000, 3000), area), (0, 0));
    }

    #[test]
    fn maximized_window_moved_off_screen_is_lost() {
        let full = [r(0, 0, 1920, 1080)];
        assert!(maximized_real_rect_lost(true, false, r(5000, 3000, 7576, 4416), &full));
        assert!(!maximized_real_rect_lost(true, false, r(-8, -8, 1928, 1088), &full));
        assert!(!maximized_real_rect_lost(false, false, r(5000, 3000, 7576, 4416), &full));
        assert!(!maximized_real_rect_lost(true, true, r(-32000, -32000, -31000, -31000), &full));
    }

    #[test]
    fn restored_rect_lands_in_primary_with_workspace_offset() {
        let work = r(0, 0, 1920, 1040);
        let out = restored_rect_in_primary(r(5000, 3000, 6200, 3800), work, (0, 0));
        assert_eq!(out, r(720, 240, 1920, 1040));
        // taskbar on the left: work area starts at x=48 of its monitor
        let out = restored_rect_in_primary(r(5000, 3000, 6200, 3800), r(48, 0, 1920, 1040), (48, 0));
        assert_eq!(out, r(672, 240, 1872, 1040));
    }
}
