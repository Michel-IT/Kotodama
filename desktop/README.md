# Kotodama • Ai Prompt Builder — Desktop (Tauri v2)

App desktop nativa cross-platform. Frontend statico in `ui/`, shell Rust in `src-tauri/`.

## Sviluppo
```bash
npm install
npm run tauri dev      # avvia in sviluppo
npm run tauri build    # genera .exe + installer NSIS/MSI
```
Prerequisiti Windows: Rust (toolchain MSVC), Node.js, VS Build Tools (C++), runtime WebView2.

## Struttura
- `ui/index.html` — builder dei prompt (UI principale).
- `ui/toast.html` — finestra di notifica (monitor appunti).
- `src-tauri/src/` — `lib.rs` (setup/comandi/tray), `browser.rs` (provider in-app),
  `clipboard.rs` (monitor), `toast.rs` (notifica), `settings.rs` (persistenza).
- `src-tauri/tauri.conf.json` — config (finestre, bundle, `frontendDist: "../ui"`).

Il frontend usa `window.__TAURI__` (`withGlobalTauri`), senza bundler.
