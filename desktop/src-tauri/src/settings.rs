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
    /// Ricetta "Neutra": invio automatico al provider (true) oppure solo incolla per editare (false).
    pub neutral_autosend: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: "".into(), // auto: the frontend detects the OS language on first launch
            default_provider: "openai".into(),
            hotkey: "Control+Shift+Space".into(),
            monitor_enabled: true,
            autostart: false,
            theme: "sumi".into(),
            recipe: "key:neutral".into(),
            length: 0,
            tone: 0,
            neutral_autosend: true,
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
