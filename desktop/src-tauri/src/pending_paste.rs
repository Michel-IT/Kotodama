//! A finished inline-transform result that was NOT pasted because the window the text came from
//! is no longer in front (or no longer exists). The result stays in the clipboard and the toast
//! offers "Paste in <window>" / "Copy". This module holds the pure decision logic (unit
//! tested) and the thin Windows helpers around it.
//!
//! Windows only for now: the macOS side (same app instead of same window) is still to do, and
//! Linux keeps today's behaviour.
#![cfg_attr(not(windows), allow(dead_code))]

/// What to do once the window the text came from has been asked to come back to the front.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PasteDecision {
    /// The original window is alive and in front: Ctrl+V reaches it.
    Paste,
    /// Anything else: do NOT paste, keep the result in the clipboard and tell the user.
    Hold,
}

/// Pure: paste only when the target still exists AND is the foreground window.
pub fn decide_paste(target_alive: bool, foreground_is_target: bool) -> PasteDecision {
    if target_alive && foreground_is_target {
        PasteDecision::Paste
    } else {
        PasteDecision::Hold
    }
}

/// Which look the "result ready" toast takes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ReadyView {
    /// The original window still exists: "Paste in <window>" is offered.
    Ready,
    /// The original window was closed: only "Copy" is left.
    Closed,
}

impl ReadyView {
    /// Mode name understood by `toast.html`.
    pub fn mode(self) -> &'static str {
        match self {
            ReadyView::Ready => "ready",
            ReadyView::Closed => "closed",
        }
    }
}

/// Pure: the toast state for a pending result, from whether the original window still exists.
pub fn ready_view(target_alive: bool) -> ReadyView {
    if target_alive {
        ReadyView::Ready
    } else {
        ReadyView::Closed
    }
}

/// What a click on one of the two buttons leads to.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ClickOutcome {
    /// The window is in front: paste now.
    Paste,
    /// The window cannot be switched to: stay on the toast and say so.
    Stay,
}

/// Pure: after trying to bring the chosen window to the front, paste or stay on the toast.
/// (A closed original window is shown by `ready_view`, recomputed when the toast is re-shown.)
pub fn click_outcome(target_alive: bool, foreground_is_target: bool) -> ClickOutcome {
    match decide_paste(target_alive, foreground_is_target) {
        PasteDecision::Paste => ClickOutcome::Paste,
        PasteDecision::Hold => ClickOutcome::Stay,
    }
}

/// Longest window name shown on a button, in characters.
const NAME_MAX_CHARS: usize = 40;

/// Pure: a readable application name from a window title. Titles usually end with the program
/// ("Untitled - Notepad", "Page - Google Chrome", "Doc1 - Word"), so the last segment after a
/// dash separator is the name; a title without a separator is used whole. Empty when there is no
/// title (the toast then says "the original window"). Capped so it never overflows a button.
pub fn window_display_name(title: &str) -> String {
    let title = title.trim();
    let mut last = title;
    for sep in [" - ", " \u{2013} ", " \u{2014} "] {
        if let Some(i) = last.rfind(sep) {
            let tail = last[i + sep.len()..].trim();
            if !tail.is_empty() {
                last = tail;
            }
        }
    }
    if last.chars().count() > NAME_MAX_CHARS {
        let mut s: String = last.chars().take(NAME_MAX_CHARS - 1).collect();
        s.push('\u{2026}');
        return s;
    }
    last.to_string()
}

/// A result waiting for the user's decision.
#[derive(Debug, Clone)]
pub struct PendingPaste {
    /// The text to paste (plain, already in the clipboard).
    pub text: String,
    /// Window the text came from (HWND as isize).
    pub target: isize,
    /// Readable name of that window, for the button.
    pub name: String,
    /// Process id of that window when it was recorded; guards against a reused HWND.
    pub pid: u32,
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, SetForegroundWindow,
        ShowWindow, SW_RESTORE,
    };

    fn hwnd(h: isize) -> HWND {
        HWND(h as *mut core::ffi::c_void)
    }

    /// Process id owning the window (0 when it cannot be read).
    pub fn window_pid(h: isize) -> u32 {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd(h), Some(&mut pid));
        }
        pid
    }

    /// The window still exists AND still belongs to the process it had when recorded (a closed
    /// window's HWND can be handed to an unrelated window later).
    pub fn is_window(h: isize, pid: u32) -> bool {
        h != 0 && unsafe { IsWindow(Some(hwnd(h))).as_bool() } && (pid == 0 || window_pid(h) == pid)
    }

    /// Brings the window to the front and VERIFIES it: polls the foreground window for up to ~1 s.
    /// `false` when the window is gone or Windows refused to give it the focus.
    pub fn bring_front(h: isize, pid: u32) -> bool {
        if !is_window(h, pid) {
            return false;
        }
        let w = hwnd(h);
        unsafe {
            if GetForegroundWindow() == w {
                return true;
            }
            if IsIconic(w).as_bool() {
                let _ = ShowWindow(w, SW_RESTORE);
            }
            let _ = SetForegroundWindow(w);
            for _ in 0..50 {
                if GetForegroundWindow() == w {
                    return true;
                }
                if !IsWindow(Some(w)).as_bool() {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            GetForegroundWindow() == w
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_only_when_alive_and_in_front() {
        assert_eq!(decide_paste(true, true), PasteDecision::Paste);
        assert_eq!(decide_paste(true, false), PasteDecision::Hold);
        assert_eq!(decide_paste(false, false), PasteDecision::Hold);
        assert_eq!(decide_paste(false, true), PasteDecision::Hold);
    }

    #[test]
    fn view_follows_target_existence() {
        assert_eq!(ready_view(true), ReadyView::Ready);
        assert_eq!(ready_view(false), ReadyView::Closed);
        assert_eq!(ReadyView::Ready.mode(), "ready");
        assert_eq!(ReadyView::Closed.mode(), "closed");
    }

    #[test]
    fn click_pastes_only_when_window_is_in_front() {
        assert_eq!(click_outcome(true, true), ClickOutcome::Paste);
        assert_eq!(click_outcome(true, false), ClickOutcome::Stay);
        assert_eq!(click_outcome(false, false), ClickOutcome::Stay);
    }

    #[test]
    fn name_is_last_segment_of_title() {
        assert_eq!(window_display_name("Senza nome - Blocco note"), "Blocco note");
        assert_eq!(window_display_name("Gmail - Posta - Google Chrome"), "Google Chrome");
        assert_eq!(window_display_name("Doc1 \u{2013} Word"), "Word");
        assert_eq!(window_display_name("Doc1 \u{2014} Word"), "Word");
        assert_eq!(window_display_name("  Calcolatrice  "), "Calcolatrice");
        assert_eq!(window_display_name("Doc - "), "Doc -");
    }

    #[test]
    fn name_empty_title_and_cap() {
        assert_eq!(window_display_name(""), "");
        assert_eq!(window_display_name("   "), "");
        let long = format!("x - {}", "a".repeat(80));
        let n = window_display_name(&long);
        assert_eq!(n.chars().count(), NAME_MAX_CHARS);
        assert!(n.ends_with('\u{2026}'));
        // multi-byte characters never split
        let jp = "\u{3053}".repeat(60);
        assert_eq!(window_display_name(&jp).chars().count(), NAME_MAX_CHARS);
    }
}
