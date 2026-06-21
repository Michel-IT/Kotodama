//! Gated runtime diagnostics.
//!
//! Active ONLY when the environment variable `KOTODAMA_DEBUG` is set (any value).
//! No effect on normal builds/runs. Output goes to stderr prefixed with `[KDBG]`,
//! captured by `tools/debug-desktop/debug-run.ps1` into a timestamped log file.
//!
//! This lets us diagnose hard runtime problems (e.g. the in-app provider webview
//! loading blank) without shipping noise to end users — and stays in the source
//! so future issues can be debugged on any build by just setting the env var.

use std::sync::OnceLock;

/// True if `KOTODAMA_DEBUG` is set. Evaluated once.
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("KOTODAMA_DEBUG").is_ok())
}

/// Logs a diagnostic line to stderr (only when enabled).
pub fn log(msg: impl AsRef<str>) {
    if enabled() {
        eprintln!("[KDBG] {}", msg.as_ref());
    }
}
