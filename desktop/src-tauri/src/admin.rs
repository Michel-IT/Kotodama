//! Running as administrator, by explicit choice.
//!
//! Kotodama works as a normal program: that is the safe default and it is what a fresh install gets. But Windows
//! refuses synthetic keystrokes and mouse-hook events between a normal program and a window that runs as
//! administrator (UIPI), so in VS Code or a terminal started as administrator the text transform and the double
//! right-click gesture simply cannot work. This module is the user's way to lift that, and to put it back.
//!
//! Two pieces, because they cover two different launches:
//!  - a logon task with the highest privileges, so the automatic start is elevated WITHOUT a prompt every time;
//!  - the "run as administrator" compatibility flag on the executable, so a manual start is elevated too.
//! Creating the task needs administrator rights itself, so it goes through Windows' own elevation prompt.

#[cfg(windows)]
pub const TASK_NAME: &str = "Kotodama avvio con privilegi";

/// Is the elevated start set up? True only when BOTH pieces are in place, which is what the switch promises.
#[cfg(windows)]
pub fn enabled(exe: &std::path::Path) -> bool {
    task_exists() && compat_flag(exe)
}

#[cfg(windows)]
fn task_exists() -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: never flash a console from a GUI app
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn compat_flag(exe: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;
    let key = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";
    std::process::Command::new("reg.exe")
        .args(["query", key, "/v", &exe.to_string_lossy()])
        .creation_flags(0x0800_0000)
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("RUNASADMIN"))
        .unwrap_or(false)
}

/// Turns the elevated start on or off. Returns once Windows has asked the user and the commands have run; a
/// refused prompt comes back as an error, so the switch can go back to where it was.
#[cfg(windows)]
pub fn set_enabled(exe: &std::path::Path, on: bool) -> Result<(), String> {
    let exe = exe.to_string_lossy().to_string();
    let layers = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";
    // One elevated shell for both pieces: one prompt, not two.
    let script = if on {
        format!(
            "schtasks /create /tn \"{TASK_NAME}\" /tr \"\\\"{exe}\\\" --silent\" /sc onlogon /rl highest /f & \
             reg add \"{layers}\" /v \"{exe}\" /t REG_SZ /d \"~ RUNASADMIN\" /f"
        )
    } else {
        format!(
            "schtasks /delete /tn \"{TASK_NAME}\" /f & reg delete \"{layers}\" /v \"{exe}\" /f"
        )
    };
    run_elevated("cmd.exe", &format!("/c {script}"))
}

/// Starts this same executable again with administrator rights (Windows asks the user), for the moment someone
/// discovers that the window they are working in is out of reach.
#[cfg(windows)]
pub fn relaunch_elevated(exe: &std::path::Path) -> Result<(), String> {
    run_elevated(&exe.to_string_lossy(), "")
}

#[cfg(windows)]
fn run_elevated(file: &str, params: &str) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(file);
    let params = HSTRING::from(params);
    // ShellExecuteW returns a value <= 32 on failure; the one that matters here is the user saying no.
    let rc = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    if rc.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!("elevation refused or failed (code {})", rc.0 as usize))
    }
}

#[cfg(not(windows))]
pub fn enabled(_exe: &std::path::Path) -> bool {
    false
}
#[cfg(not(windows))]
pub fn set_enabled(_exe: &std::path::Path, _on: bool) -> Result<(), String> {
    Err("only on Windows".into())
}
#[cfg(not(windows))]
pub fn relaunch_elevated(_exe: &std::path::Path) -> Result<(), String> {
    Err("only on Windows".into())
}
