//! Terms and Conditions: the text shown on the welcome page, its fingerprint, and the user's consent to
//! automatic sending, one provider at a time.
//!
//! The consent lives only in `settings.json` (version, date, SHA-256 of the text shown, providers). Without it,
//! or for a provider not in it, `kotodama_broadcast` sends nothing: the provider pages still open and work by
//! hand. Every change of the consent goes through the commands below, never through `set_settings`, so a stale
//! copy of the settings in the interface can never grant or drop a provider by accident.

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::settings::Settings;
use crate::AppState;

/// Version of the Terms in `ui/terms/*.html` (`data-version`, checked by a test). A wording fix raises the minor
/// part (1.0 -> 1.1) and does not show the page again; a substantial change raises the major part.
pub const TERMS_VERSION: &str = "1.1";
/// The major part an accepted consent must have to stay valid. Raise it together with `TERMS_VERSION`'s major
/// part for a substantial change (sections 6, 7 or 8, or something new done in the providers' pages): every
/// earlier consent then stops counting and the page is shown again once.
pub const TERMS_SUBSTANTIAL_VERSION: &str = "1";

/// The Terms exactly as the welcome page shows them. Compiled in, so the text hashed is the text displayed
/// in every build, dev or release.
const TERMS_IT: &str = include_str!("../../ui/terms/terms-it.html");
const TERMS_EN: &str = include_str!("../../ui/terms/terms-en.html");

/// The providers whose automatic sending the Terms can turn on, in the order of the Terms (section 3).
/// Copilot is still "coming soon" and cannot be chosen.
pub const AUTO_PROVIDERS: &[&str] = &[
    "openai", "anthropic", "gemini", "grok", "perplexity", "qwen", "deepseek", "zai", "mistral", "poe", "kimi", "meta",
];

/// The Terms exist in Italian and English; every other interface language shows the English text.
pub fn terms_lang(lang: &str) -> &'static str {
    if lang == "it" {
        "it"
    } else {
        "en"
    }
}

/// The Terms text in `lang` (see `terms_lang`), with line ends normalized: a checkout on Windows may turn the
/// files to CRLF, and the fingerprint must not depend on the machine that built the app.
pub fn terms_text(lang: &str) -> String {
    let raw = if terms_lang(lang) == "it" { TERMS_IT } else { TERMS_EN };
    raw.replace("\r\n", "\n")
}

/// SHA-256 of a text, lowercase hex.
pub fn fingerprint(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn major(v: &str) -> &str {
    v.split('.').next().unwrap_or("")
}

/// The user accepted a version of the Terms that still counts (same substantial version).
pub fn consent_valid(s: &Settings) -> bool {
    !s.terms_version.is_empty() && major(&s.terms_version) == TERMS_SUBSTANTIAL_VERSION
}

/// The welcome page has been answered for the current substantial version (accepted, or "Continue without
/// automatic sending"): it is not shown at startup again until the substantial version changes.
pub fn page_answered(s: &Settings) -> bool {
    consent_valid(s) || (!s.terms_seen.is_empty() && major(&s.terms_seen) == TERMS_SUBSTANTIAL_VERSION)
}

/// Kotodama may type into and read from this provider's page by itself.
pub fn auto_send_allowed(s: &Settings, key: &str) -> bool {
    consent_valid(s) && s.terms_providers.iter().any(|k| k == key)
}

/// Keeps only providers the Terms can turn on, once each, in the Terms' order.
fn clean_providers(list: &[String]) -> Vec<String> {
    AUTO_PROVIDERS.iter().filter(|k| list.iter().any(|x| x == *k)).map(|k| k.to_string()).collect()
}

/// Now, as an ISO 8601 date in UTC (`2026-10-25T09:41:07Z`). Computed by hand: no date crate for one line.
pub fn iso_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_utc(secs)
}

fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // civil_from_days (Howard Hinnant): days since 1970-01-01 to year/month/day
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// The record of the consent, as exported: everything needed to show what was accepted, and nothing else.
pub fn consent_record(s: &Settings, app_version: &str) -> serde_json::Value {
    let valid = consent_valid(s);
    serde_json::json!({
        "app": "Kotodama",
        "app_version": app_version,
        "terms_version": s.terms_version,
        "terms_language": s.terms_lang,
        "terms_sha256": s.terms_hash,
        "accepted_at": s.terms_accepted_at,
        "updated_at": s.terms_updated_at,
        "valid_for_current_terms": valid,
        "specific_approval_sections": if valid { serde_json::json!([6, 7, 8, 11]) } else { serde_json::json!([]) },
        "providers": if valid { s.terms_providers.clone() } else { Vec::new() },
        "terms_bulk": s.terms_bulk,
        "qwen_simulated_enter": valid && s.terms_providers.iter().any(|k| k == "qwen") && s.qwen_trusted_input,
        "exported_at": iso_utc_now(),
    })
}

/// Applies `f` to the settings in memory, saves them and returns the result for the interface to mirror.
fn update(app: &AppHandle, f: impl FnOnce(&mut Settings)) -> Settings {
    let snapshot = {
        let state = app.state::<AppState>();
        let mut g = state.settings.lock().unwrap();
        f(&mut g);
        g.clone()
    };
    crate::save_settings_bg(app, snapshot.clone());
    snapshot
}

#[derive(Serialize)]
pub struct TermsDoc {
    pub lang: String,
    pub html: String,
    pub version: String,
    pub hash: String,
    /// The current consent stops counting only below this substantial version.
    pub substantial: String,
    /// A consent that counts is on record (`consent_valid`).
    pub valid: bool,
    /// The page was answered for this substantial version (`page_answered`): not shown at startup.
    pub answered: bool,
}

/// The Terms to show, with the fingerprint that accepting them will record, and whether the page still has
/// to be shown at startup.
#[tauri::command]
pub fn terms_get(app: AppHandle, lang: String) -> TermsDoc {
    let lang = terms_lang(&lang);
    let html = terms_text(lang);
    let s = app.state::<AppState>().settings.lock().unwrap().clone();
    TermsDoc {
        valid: consent_valid(&s),
        answered: page_answered(&s),
        lang: lang.into(),
        hash: fingerprint(&html),
        html,
        version: TERMS_VERSION.into(),
        substantial: TERMS_SUBSTANTIAL_VERSION.into(),
    }
}

/// "Accept and enable the selected providers": records version, date, fingerprint of the text shown in `lang`
/// and the providers ticked; it replaces any earlier list. `qwen_trusted` is the Qwen line on the page (None =
/// not shown, the earlier answer stays). The page also says that icons are downloaded, so confirming it is
/// that decision too, as the old welcome was.
#[tauri::command]
pub fn terms_accept(
    app: AppHandle,
    lang: String,
    providers: Vec<String>,
    qwen_trusted: Option<bool>,
    bulk: Option<bool>,
) -> Result<Settings, String> {
    let providers = clean_providers(&providers);
    if providers.is_empty() {
        return Err("no provider".into());
    }
    let lang = terms_lang(&lang);
    let hash = fingerprint(&terms_text(lang));
    let now = iso_utc_now();
    crate::debug::log(format!("terms accepted v{TERMS_VERSION} lang={lang} providers={providers:?}"));
    Ok(update(&app, |g| {
        g.terms_version = TERMS_VERSION.into();
        g.terms_seen = TERMS_VERSION.into();
        g.terms_lang = lang.into();
        g.terms_hash = hash;
        g.terms_accepted_at = now.clone();
        g.terms_updated_at = now;
        g.terms_providers = providers;
        g.terms_bulk = bulk.unwrap_or(false);
        g.welcome_ack = true;
        g.prov_icons_decided = true;
        if let Some(on) = qwen_trusted {
            g.qwen_trusted_input = on;
            g.qwen_trusted_input_decided = g.qwen_trusted_input_decided || on;
        }
    }))
}

/// "Continue without automatic sending": the page is answered and does not come back at startup; whatever
/// consent there was stays as it is (at startup the page only appears when there is none that counts).
#[tauri::command]
pub fn terms_decline(app: AppHandle) -> Settings {
    crate::debug::log("terms: continue without automatic sending");
    update(&app, |g| {
        g.terms_seen = TERMS_VERSION.into();
        g.welcome_ack = true;
        g.prov_icons_decided = true;
    })
}

/// Withdraws the consent: automatic sending stops for every provider. The page is not shown again at startup;
/// it can be reopened from Settings > About.
#[tauri::command]
pub fn terms_withdraw(app: AppHandle) -> Settings {
    crate::debug::log("terms: consent withdrawn");
    update(&app, |g| {
        g.terms_version.clear();
        g.terms_lang.clear();
        g.terms_hash.clear();
        g.terms_accepted_at.clear();
        g.terms_updated_at = iso_utc_now();
        g.terms_providers.clear();
        g.terms_bulk = false;
        if g.terms_seen.is_empty() {
            g.terms_seen = TERMS_VERSION.into();
        }
    })
}

/// Settings > Providers > Automatic sending, one provider. Turning one on needs a consent that counts (the
/// interface asks the user for that provider first); turning one off never needs anything.
#[tauri::command]
pub fn terms_set_provider(app: AppHandle, key: String, on: bool) -> Result<Settings, String> {
    if !AUTO_PROVIDERS.contains(&key.as_str()) {
        return Err("unknown provider".into());
    }
    if on && !consent_valid(&app.state::<AppState>().settings.lock().unwrap()) {
        return Err("no consent".into());
    }
    crate::debug::log(format!("terms: automatic sending {key} -> {on}"));
    Ok(update(&app, |g| {
        let mut list = g.terms_providers.clone();
        list.retain(|k| k != &key);
        if on {
            list.push(key.clone());
        }
        g.terms_providers = clean_providers(&list);
        g.terms_updated_at = iso_utc_now();
    }))
}

/// Pure part of `terms_set_providers`: the list after turning `keys` on or off (the Terms' order, no unknown
/// keys), and the bulk mark: the gesture marks the record, turning off leaves the earlier mark alone (the
/// proof of what was consented stays).
fn apply_many(list: &[String], keys: &[String], on: bool, bulk: bool, was_bulk: bool) -> (Vec<String>, bool) {
    let keys = clean_providers(keys);
    let mut out: Vec<String> = list.iter().filter(|k| !keys.contains(k)).cloned().collect();
    if on {
        out.extend(keys);
    }
    (clean_providers(&out), if on && bulk { true } else { was_bulk })
}

/// Settings > Providers > "Enable all" / "Disable all", several providers in one step. Turning them on needs a
/// consent that counts (the interface shows ONE dialog for the ones without it first); turning off needs
/// nothing and keeps the proof of the consents given. `bulk` = the gesture was used (`terms_bulk`).
#[tauri::command]
pub fn terms_set_providers(app: AppHandle, keys: Vec<String>, on: bool, bulk: bool) -> Result<Settings, String> {
    if on && !consent_valid(&app.state::<AppState>().settings.lock().unwrap()) {
        return Err("no consent".into());
    }
    crate::debug::log(format!("terms: automatic sending {keys:?} -> {on} (bulk {bulk})"));
    Ok(update(&app, |g| {
        let (list, b) = apply_many(&g.terms_providers, &keys, on, bulk, g.terms_bulk);
        g.terms_providers = list;
        g.terms_bulk = b;
        g.terms_updated_at = iso_utc_now();
    }))
}

/// The consent record as pretty JSON, for the interface to save where the user chooses.
#[tauri::command]
pub fn terms_consent_json(app: AppHandle) -> String {
    let s = app.state::<AppState>().settings.lock().unwrap().clone();
    let v = consent_record(&s, &app.package_info().version.to_string());
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// First free file name `kotodama-consenso-YYYY-MM-DD.json`, then `-2`, `-3`... if it exists: an export never
/// overwrites an earlier one.
fn free_export_path(dir: &std::path::Path, date: &str) -> std::path::PathBuf {
    let first = dir.join(format!("kotodama-consenso-{date}.json"));
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| dir.join(format!("kotodama-consenso-{date}-{n}.json")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Export of the consent record: always written to Downloads/Kotodama (no system save dialog, the webview's one
/// fails on protected folders) and the folder is shown, as for exported audio. Returns the saved path.
#[tauri::command]
pub fn terms_export_file(app: AppHandle) -> Result<String, String> {
    let json = terms_consent_json(app.clone());
    let dir = app.path().download_dir().map_err(|e| e.to_string())?.join("Kotodama");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dst = free_export_path(&dir, &iso_utc_now()[..10]);
    std::fs::write(&dst, json).map_err(|e| e.to_string())?;
    {
        use tauri_plugin_opener::OpenerExt;
        let _ = app.opener().reveal_item_in_dir(&dst);
    }
    Ok(dst.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files carry the version the code records, so the fingerprint is always tied to the right number.
    #[test]
    fn terms_files_match_the_version() {
        for lang in ["it", "en"] {
            let t = terms_text(lang);
            assert!(t.contains(&format!("data-version=\"{TERMS_VERSION}\"")), "{lang}");
            assert!(!t.contains('\r') && !t.contains('\u{2014}'), "{lang}");
            assert!(t.contains("legal@kuramalab.net") && t.contains("https://github.com/Michel-IT/Kotodama/issues"), "{lang}");
            for n in 1..=11 {
                assert!(t.contains(&format!("id=\"tc-{lang}-s{n}\"")), "{lang} section {n}");
            }
        }
        assert_eq!(major(TERMS_VERSION), TERMS_SUBSTANTIAL_VERSION);
        assert_eq!(terms_lang("fr"), "en");
        assert_eq!(terms_text("de"), terms_text("en"));
    }

    #[test]
    fn export_name_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("koto-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = free_export_path(&dir, "2026-10-25");
        assert_eq!(a.file_name().unwrap(), "kotodama-consenso-2026-10-25.json");
        std::fs::write(&a, "x").unwrap();
        let b = free_export_path(&dir, "2026-10-25");
        assert_eq!(b.file_name().unwrap(), "kotodama-consenso-2026-10-25-2.json");
        std::fs::write(&b, "x").unwrap();
        assert_eq!(free_export_path(&dir, "2026-10-25").file_name().unwrap(), "kotodama-consenso-2026-10-25-3.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_is_sha256() {
        assert_eq!(fingerprint("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_ne!(fingerprint(&terms_text("it")), fingerprint(&terms_text("en")));
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_utc(1_792_920_067), "2026-10-25T09:21:07Z");
    }

    /// Who may send: nobody without consent, only the ticked providers with it, nobody once the substantial
    /// version moves on. A wording fix (minor version) keeps the consent; the page shows again only on a
    /// substantial change, also for whoever chose to continue without automatic sending.
    #[test]
    fn consent_rules() {
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        assert!(!consent_valid(&s) && !page_answered(&s) && !auto_send_allowed(&s, "openai"));
        s.terms_seen = TERMS_VERSION.into();
        assert!(page_answered(&s) && !auto_send_allowed(&s, "openai"));
        s.terms_version = TERMS_VERSION.into();
        s.terms_providers = clean_providers(&["qwen".into(), "copilot".into(), "openai".into(), "openai".into()]);
        assert_eq!(s.terms_providers, ["openai", "qwen"]);
        assert!(auto_send_allowed(&s, "openai") && auto_send_allowed(&s, "qwen") && !auto_send_allowed(&s, "anthropic"));
        s.terms_version = format!("{TERMS_SUBSTANTIAL_VERSION}.9");
        assert!(auto_send_allowed(&s, "openai"));
        s.terms_version = "0.9".into();
        s.terms_seen = "0.9".into();
        assert!(!consent_valid(&s) && !page_answered(&s) && !auto_send_allowed(&s, "openai"));
        let r = consent_record(&s, "1.0.0");
        assert_eq!(r["providers"], serde_json::json!([]));
    }

    /// A consent given on the 1.0 text still counts with the 1.1 (same substantial version): the page does not
    /// come back, and a decline recorded on 1.0 is still an answer.
    #[test]
    fn consent_of_1_0_survives_minor_bump() {
        assert_eq!(TERMS_VERSION, "1.1");
        assert_eq!(TERMS_SUBSTANTIAL_VERSION, "1");
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        s.terms_version = "1.0".into();
        s.terms_seen = "1.0".into();
        s.terms_providers = vec!["openai".into()];
        assert!(consent_valid(&s) && page_answered(&s) && auto_send_allowed(&s, "openai"));
        let mut d: Settings = serde_json::from_str("{}").unwrap();
        d.terms_seen = "1.0".into();
        assert!(page_answered(&d) && !consent_valid(&d));
    }

    /// The bulk gesture: single providers are listed (never "all"), unknown keys dropped, the mark is set by the
    /// gesture and survives "disable all", which keeps the record.
    #[test]
    fn bulk_gesture_lists_single_providers() {
        let all: Vec<String> = AUTO_PROVIDERS.iter().map(|k| k.to_string()).chain(["copilot".to_string()]).collect();
        let (list, bulk) = apply_many(&["poe".into()], &all, true, true, false);
        assert_eq!(list, AUTO_PROVIDERS);
        assert!(bulk && !list.iter().any(|k| k == "all" || k == "copilot"));
        let (list, bulk) = apply_many(&list, &all, false, false, bulk);
        assert!(list.is_empty() && bulk);
        let (list, bulk) = apply_many(&[], &["kimi".into(), "openai".into()], true, false, false);
        assert_eq!(list, ["openai", "kimi"]);
        assert!(!bulk);
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(!s.terms_bulk);
        let mut s = s;
        s.terms_bulk = true;
        assert_eq!(consent_record(&s, "1.1.0")["terms_bulk"], serde_json::json!(true));
    }
}
