//! Settings persisted in `app_config_dir/settings.json`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Manager};

/// Serializes writes to disk: saves can be started from background threads
/// (see `lib.rs`); this lock prevents two concurrent writes from overlapping
/// and corrupting the file.
fn io_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Interface language (code, e.g. "it","en","fr"…). Empty = "auto":
    /// on first launch the frontend sets it to the OS language (English fallback).
    pub language: String,
    /// Provider selected at startup (PROVIDERS key on the frontend side).
    pub default_provider: String,
    /// Global accelerator, Tauri/W3C code format (e.g. "Control+Alt+Space" = Ctrl+Alt+Space).
    pub hotkey: String,
    /// Global clipboard monitor enabled.
    pub monitor_enabled: bool,
    /// Start on login.
    pub autostart: bool,
    /// Interface theme: "teal" | "glass" | "flat".
    pub theme: String,
    /// Ricetta predefinita (★) usata quando si copia; all'avvio diventa anche l'attiva.
    /// Formato: "key:<builtin>" oppure "id:<custom>".
    pub recipe: String,
    /// Last UI state: Length option index.
    pub length: u32,
    /// Last UI state: Tone option index.
    pub tone: u32,
    /// "Risposta" field: 0 = Normale (nothing added), 1 = Solo testo (append answer-only
    /// constraint). Default 1.
    pub resp_fmt: u32,
    /// Finestra sempre in primo piano (galleggia sopra le altre finestre). Default: on.
    pub always_on_top: bool,
    /// La modale di benvenuto (primo avvio, privacy + confine provider) e' stata confermata
    /// con "non mostrare piu'": se true non ricompare all'avvio. Default: false.
    pub welcome_ack: bool,
    /// Scorciatoie globali PER RICETTA: chiave = "key:<builtin>"/"id:<custom>", valore =
    /// accelerator W3C. Premuta -> flusso clipboard con QUELLA ricetta (non la predefinita).
    /// Le entry che non si registrano (conflitti/invalidi) vengono scartate al salvataggio.
    pub recipe_hotkeys: std::collections::HashMap<String, String>,
    /// Notifiche toast per scorciatoia-ricetta: chiave = stessa di `recipe_hotkeys`, valore =
    /// mostra i toast "elaborazione/fatto/errore" per QUELLA scorciatoia. Assente = abilitata
    /// (default): solo chi disattiva esplicitamente una ricetta molto usata sparisce dalla mappa.
    pub recipe_notify: std::collections::HashMap<String, bool>,
    /// Kotodama broadcast: usa la chat TEMPORANEA/anonima dei provider (dove supportata),
    /// cosi' le richieste multi-provider non intasano le cronologie dei siti. Default: on.
    pub kt_temp_chats: bool,
    /// Chat temporanea PER PROVIDER (key -> abilitata). Assente = abilitata dove supportata.
    /// Il frontend sa quali provider la supportano; i toggle degli altri sono disabilitati.
    pub kt_temp_providers: std::collections::HashMap<String, bool>,
    /// Provider con cui un broadcast Kotodama e' andato a buon fine ALMENO una volta (segno che
    /// l'utente ha un account attivo li'). Usato dal frontend per pre-selezionare SOLO questi
    /// nella chat "chiedi a tutti", invece di tutti gli 8 a prescindere dal login: un broadcast
    /// verso un provider mai autenticato creava comunque la sua webview e finiva su un muro di
    /// login, per niente. Gli altri provider restano disponibili, l'utente li aggiunge a mano la
    /// prima volta.
    pub known_providers: std::collections::HashSet<String>,
    /// Resource saving. Two effects, both with a cost that must stay the user's choice:
    ///  - no hardware acceleration for the webviews (`--disable-gpu`): the GPU process gets lighter
    ///    (measured -30/-50 MB, it does NOT disappear), in exchange drawing moves to the CPU and
    ///    scrolling in long chats can feel less smooth;
    ///  - providers idle for 3 minutes are suspended (`TrySuspend`): their page freezes and returns
    ///    memory, and is woken on first use with a moment's wait.
    /// Default OFF = historic behaviour: nobody gets worse off without asking for it.
    /// Requires an app RESTART: the browser arguments are read by WebView2 once, when it creates its
    /// environment (see `run()`), before any window exists.
    pub low_power: bool,
}

/// Default modifier pair for every built-in shortcut, per platform.
///
/// macOS gets Ctrl+Cmd instead of Ctrl+Alt: on a Mac keyboard Option is a dead-key modifier that
/// composes characters (Opt+C types a cedilla), so Ctrl+Alt+<letter> is both awkward to press and
/// prone to clashing with text input, while Ctrl+Cmd is the free combination Apple leaves to apps.
/// Windows and Linux keep Ctrl+Alt, which is what those users already have.
#[cfg(target_os = "macos")]
pub const DEFAULT_MOD: &str = "Control+Super"; // Super == Command on macOS
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_MOD: &str = "Control+Alt";

/// The modifier every platform used before this split. Only settings still holding EXACTLY these
/// combinations are migrated (see `migrate_platform_hotkeys`).
pub const LEGACY_MOD: &str = "Control+Alt";

/// The shortcuts a fresh install ships with. Also the reference used to tell an untouched default
/// from a user's own choice when migrating (see `migrate_platform_hotkeys`).
pub fn default_hotkey() -> String {
    // The main shortcut cannot be <mod>+Space on macOS: BOTH candidates are taken by the system
    // there -- Ctrl+Cmd+Space opens Emoji & Symbols, and Ctrl+Opt+Space switches input source. K
    // (for Kotodama) is free under Ctrl+Cmd, unlike F/Q/D/Space which macOS already claims.
    // `cfg!` rather than `#[cfg]` on purpose: both arms are compiled on every platform, so a
    // Windows build still type-checks the macOS one.
    let key = if cfg!(target_os = "macos") { "KeyK" } else { "Space" };
    format!("{DEFAULT_MOD}+{key}")
}
pub fn default_recipe_hotkeys() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([
        ("key:rephrase".to_string(), format!("{DEFAULT_MOD}+KeyC")),
        ("key:translate".to_string(), format!("{DEFAULT_MOD}+KeyT")),
    ])
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: "".into(), // auto: the frontend detects the OS language on first launch
            default_provider: "openai".into(),
            hotkey: default_hotkey(),
            monitor_enabled: true,
            autostart: true,
            theme: "sumi".into(),
            recipe: "key:neutral".into(),
            length: 0,
            tone: 0,
            resp_fmt: 1, // default "Solo testo": clean, answer-only output
            always_on_top: true,
            welcome_ack: false,
            // Default per-recipe shortcuts (fresh installs): Rephrase = <mod>+C, Translate =
            // <mod>+T, where <mod> is Ctrl+Alt everywhere and Ctrl+Cmd on macOS (see DEFAULT_MOD).
            // Applied via serde container-default when the field is absent from settings.json;
            // users who already set their own keep theirs.
            recipe_hotkeys: default_recipe_hotkeys(),
            recipe_notify: std::collections::HashMap::new(),
            kt_temp_chats: true,
            kt_temp_providers: std::collections::HashMap::new(),
            known_providers: std::collections::HashSet::new(),
            low_power: false, // hardware acceleration on: no user gets worse off unknowingly
        }
    }
}

/// Custom recipe created by the user (the built-in ones live in the frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub instruction: String,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

/// True if `settings.json` already exists -- i.e. NOT a fresh install. Used once at startup to
/// decide whether to force-enable autostart for a brand-new user without touching an existing
/// user's explicit choice (see `lib.rs::setup`).
pub fn exists(app: &AppHandle) -> bool {
    settings_path(app).map(|p| p.exists()).unwrap_or(false)
}

/// Load the settings; on error or missing file returns the defaults.
pub fn load(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Moves an install off the shortcuts it inherited from the historic Ctrl+Alt defaults onto the
/// ones this platform now ships (see `DEFAULT_MOD`). In practice: macOS only, since every other
/// platform still defaults to Ctrl+Alt and the function then does nothing.
///
/// Only values still EXACTLY equal to the old default are touched: a combination the user picked
/// themselves is their choice and stays, even if it happens to be a poor one on this platform.
/// Returns true when something changed, so the caller can persist and re-register.
pub fn migrate_platform_hotkeys(s: &mut Settings) -> bool {
    if DEFAULT_MOD == LEGACY_MOD {
        return false; // this platform never moved: nothing to migrate
    }
    let mut changed = false;
    if s.hotkey == format!("{LEGACY_MOD}+Space") {
        s.hotkey = default_hotkey();
        changed = true;
    }
    let fresh = default_recipe_hotkeys();
    for (recipe, legacy_key) in [("key:rephrase", "KeyC"), ("key:translate", "KeyT")] {
        let legacy = format!("{LEGACY_MOD}+{legacy_key}");
        if s.recipe_hotkeys.get(recipe) == Some(&legacy) {
            if let Some(v) = fresh.get(recipe) {
                s.recipe_hotkeys.insert(recipe.to_string(), v.clone());
                changed = true;
            }
        }
    }
    changed
}

/// Save the settings to disk.
pub fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let _guard = io_lock().lock().unwrap();
    fs::write(path, json).map_err(|e| e.to_string())
}

fn recipes_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("recipes.json"))
}

/// Load the custom recipes (empty if missing/error).
pub fn load_recipes(app: &AppHandle) -> Vec<Recipe> {
    recipes_path(app)
        .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save the custom recipes to disk.
pub fn save_recipes(app: &AppHandle, recipes: &[Recipe]) -> Result<(), String> {
    let path = recipes_path(app)?;
    let json = serde_json::to_string_pretty(recipes).map_err(|e| e.to_string())?;
    let _guard = io_lock().lock().unwrap();
    fs::write(path, json).map_err(|e| e.to_string())
}

/// Custom field created by the user (extra section beyond Length/Tone).
/// `value` is the index of the selected option in `options`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub id: String,
    pub label: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub value: u32,
}

fn fields_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("fields.json"))
}

/// Load the custom fields (empty if missing/error).
pub fn load_fields(app: &AppHandle) -> Vec<Field> {
    fields_path(app)
        .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save the custom fields to disk.
pub fn save_fields(app: &AppHandle, fields: &[Field]) -> Result<(), String> {
    let path = fields_path(app)?;
    let json = serde_json::to_string_pretty(fields).map_err(|e| e.to_string())?;
    let _guard = io_lock().lock().unwrap();
    fs::write(path, json).map_err(|e| e.to_string())
}

fn kt_sessions_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("kotodama-sessions.json"))
}

/// Load the Kotodama meta-chat sessions ('[]' if missing/error). The payload is opaque
/// (owned and versioned by the frontend), so it stays a raw JSON value on this side.
pub fn load_kt_sessions(app: &AppHandle) -> serde_json::Value {
    kt_sessions_path(app)
        .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()))
}

/// Save the Kotodama meta-chat sessions to disk.
pub fn save_kt_sessions(app: &AppHandle, sessions: &serde_json::Value) -> Result<(), String> {
    let path = kt_sessions_path(app)?;
    let json = serde_json::to_string(sessions).map_err(|e| e.to_string())?; // compact: can be large
    let _guard = io_lock().lock().unwrap();
    fs::write(path, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped shortcuts must follow the platform: macOS on Ctrl+Cmd (Option is a dead-key
    /// modifier there and Ctrl+Alt+Space is the system input-source switcher), everyone else on
    /// the historic Ctrl+Alt.
    #[test]
    fn defaults_follow_the_platform() {
        let r = default_recipe_hotkeys();
        if cfg!(target_os = "macos") {
            assert_eq!(DEFAULT_MOD, "Control+Super");
            assert_eq!(default_hotkey(), "Control+Super+KeyK");
            assert_eq!(r["key:rephrase"], "Control+Super+KeyC");
            assert_eq!(r["key:translate"], "Control+Super+KeyT");
        } else {
            assert_eq!(DEFAULT_MOD, "Control+Alt");
            assert_eq!(default_hotkey(), "Control+Alt+Space");
            assert_eq!(r["key:rephrase"], "Control+Alt+KeyC");
            assert_eq!(r["key:translate"], "Control+Alt+KeyT");
        }
    }

    /// The migration moves what the install merely inherited, and nothing the user chose. Off
    /// macOS it must not move anything at all.
    #[test]
    fn migration_moves_only_untouched_defaults() {
        let mut s = Settings::default();
        s.hotkey = "Control+Alt+Space".into();
        s.recipe_hotkeys
            .insert("key:rephrase".into(), "Control+Alt+KeyC".into());
        s.recipe_hotkeys
            .insert("key:translate".into(), "Control+Shift+F9".into()); // the user's own choice

        let changed = migrate_platform_hotkeys(&mut s);

        assert_eq!(changed, cfg!(target_os = "macos"));
        assert_eq!(s.hotkey, default_hotkey());
        assert_eq!(
            s.recipe_hotkeys["key:rephrase"],
            default_recipe_hotkeys()["key:rephrase"]
        );
        assert_eq!(s.recipe_hotkeys["key:translate"], "Control+Shift+F9");
    }
}
