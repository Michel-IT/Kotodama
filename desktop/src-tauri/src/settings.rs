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
    /// Global accelerator, Tauri/W3C code format (e.g. "Control+Shift+Space" = Ctrl+Shift+Space).
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: "".into(), // auto: the frontend detects the OS language on first launch
            default_provider: "openai".into(),
            hotkey: "Control+Shift+Space".into(),
            monitor_enabled: true,
            autostart: true,
            theme: "sumi".into(),
            recipe: "key:neutral".into(),
            length: 0,
            tone: 0,
            resp_fmt: 1, // default "Solo testo": clean, answer-only output
            always_on_top: true,
            welcome_ack: false,
            // Default per-recipe shortcuts (fresh installs): Riformula = Ctrl+Alt+C,
            // Traduci = Ctrl+Alt+T. Applied via serde container-default when the field is
            // absent from settings.json; users who already set their own keep theirs.
            recipe_hotkeys: std::collections::HashMap::from([
                ("key:rephrase".to_string(), "Control+Alt+KeyC".to_string()),
                ("key:translate".to_string(), "Control+Alt+KeyT".to_string()),
            ]),
            kt_temp_chats: true,
            kt_temp_providers: std::collections::HashMap::new(),
            known_providers: std::collections::HashSet::new(),
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
