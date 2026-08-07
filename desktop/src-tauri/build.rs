fn main() {
    // Declares EVERY app command so Tauri generates a scoped ACL permission for each
    // (`allow-<command>`). NOTE: declaring even ONE command here switches the whole app from
    // "custom commands unrestricted" to "every custom command needs an explicit grant" -- so once
    // we declare any, we must declare (and grant back) ALL of them, or the undeclared ones start
    // failing with "not allowed... Command not found" (this broke show_provider_tab once already).
    let commands: &[&str] = &[
        "show_provider_tab", "kotodama_broadcast", "kotodama_push",
        "provider_login_probe",
        "mark_provider_known", "kotodama_cancel", "get_kotodama_sessions", "save_kotodama_sessions",
        "inline_toast", "inline_finish", "inline_fail", "close_provider_view",
        "set_provider_top_extra", "provider_reload", "provider_back",
        "set_provider_menu_labels",
        "provider_suppress", "provider_dock", "open_download_path", "reveal_download_path",
        "accept_clipboard", "hide_toast", "app_write_clipboard", "show_main", "hide_main",
        "get_settings", "get_system_locale", "open_url", "set_settings", "save_ui_state",
        "set_tray_labels", "get_recipes", "save_recipes", "get_fields", "save_fields",
        "check_for_update", "install_update",
        "quit_app", "restart_app",
    ];
    let attrs = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(commands));
    tauri_build::try_build(attrs).expect("tauri-build failed");
}
