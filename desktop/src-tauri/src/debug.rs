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

/// Seconds since the Unix epoch, to place a diagnostic line in time.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Debug only: a panic on any thread is written to the log with its time before the default handler
/// runs, so a crash leaves a trace even when stderr is not being watched.
pub fn install_panic_hook() {
    if !enabled() {
        return;
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(format!("PANIC unix={} {info}", unix_now()));
        default(info);
    }));
}

/// Debug only: appends one network-capture record to `<temp>/kotodama-netcap/<key>.jsonl`, wrapped with
/// the broadcast id. A file per provider, because a capture session is read one provider at a time.
pub fn netcap(key: &str, bid: &str, record: &str) {
    if !enabled() {
        return;
    }
    use std::io::Write;
    let dir = std::env::temp_dir().join("kotodama-netcap");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let safe: String = key.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    let path = dir.join(format!("{safe}.jsonl"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{{\"bid\":{},\"rec\":{}}}", serde_json::to_string(bid).unwrap_or_default(), record);
    }
}
