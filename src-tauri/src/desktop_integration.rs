//! Platform-neutral entry points for desktop integration (shortcuts, output,
//! autostart, updates). On Flatpak + Wayland these route to `crate::portal`;
//! everywhere else they keep the native behaviour.

use log::{debug, error, warn};
use serde::Serialize;
use specta::Type;
use std::time::Instant;
use tauri::AppHandle;
#[cfg(target_os = "linux")]
use tauri::Manager;

#[derive(Clone, Copy, Debug, Serialize, Type, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PortalState {
    /// The portal path is not used in this environment.
    NotApplicable,
    /// The desktop does not provide the portal.
    Unsupported,
    /// Available but not started (for example, waiting for the user).
    Inactive,
    Connecting,
    Active,
    /// The user declined the request.
    Denied,
    /// The desktop closed the session.
    Closed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct PortalShortcut {
    pub id: String,
    pub description: String,
    /// Human-readable trigger chosen in the desktop's shortcut settings.
    pub trigger: String,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct DesktopIntegrationStatus {
    /// Running inside a Flatpak sandbox.
    pub sandboxed: bool,
    /// Flatpak on Wayland: shortcuts, paste and background clipboard use
    /// XDG desktop portals.
    pub portal_mode: bool,
    pub shortcuts_state: PortalState,
    pub shortcuts_version: u32,
    pub shortcuts: Vec<PortalShortcut>,
    pub shortcuts_error: Option<String>,
    /// User setting: automatic paste through the RemoteDesktop portal.
    pub paste_enabled: bool,
    pub paste_state: PortalState,
    pub paste_error: Option<String>,
    pub autostart_supported: bool,
    /// The built-in updater is available (not for Flatpak installs).
    pub native_updates: bool,
    /// Command that toggles dictation from a desktop-managed shortcut.
    pub toggle_command: String,
}

impl DesktopIntegrationStatus {
    pub fn initial() -> Self {
        let sandboxed = sandboxed();
        let portal = portal_mode();
        let initial_state = if portal {
            PortalState::Inactive
        } else {
            PortalState::NotApplicable
        };
        Self {
            sandboxed,
            portal_mode: portal,
            shortcuts_state: initial_state,
            shortcuts_version: 0,
            shortcuts: Vec::new(),
            shortcuts_error: None,
            paste_enabled: false,
            paste_state: initial_state,
            paste_error: None,
            // Flatpak needs the Background portal; probed at startup.
            autostart_supported: !sandboxed,
            native_updates: !sandboxed,
            toggle_command: if sandboxed {
                "flatpak run computer.footy.app --toggle-transcription".to_string()
            } else {
                "footy --toggle-transcription".to_string()
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    /// Footy could not write the clipboard in the background.
    NotFocused,
}

pub fn sandboxed() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::portal::is_flatpak()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub fn portal_mode() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::portal::portal_mode()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Put text on the clipboard for import, tray copy and fallbacks.
pub async fn copy_text(app: &AppHandle, text: &str) -> Result<CopyOutcome, String> {
    #[cfg(target_os = "linux")]
    if portal_mode() {
        return crate::portal::paste::copy_text(app, text).await;
    }
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard()
        .write_text(text)
        .map(|_| CopyOutcome::Copied)
        .map_err(|e| format!("Failed to copy to clipboard: {e}"))
}

/// Deliver a finished dictation. Resolves only after output (including any
/// clipboard restoration) is complete, so the caller can keep the pipeline
/// busy until then.
pub async fn deliver_transcript(app: &AppHandle, text: String) {
    let started = Instant::now();

    #[cfg(target_os = "linux")]
    if portal_mode() {
        deliver_portal(app, text).await;
        debug!("Transcript delivered in {:?}", started.elapsed());
        return;
    }

    // Native: paste on the main thread as before, but wait for completion.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let app_for_paste = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        let _ = tx.send(crate::utils::paste(text, app_for_paste));
    }) {
        error!("Failed to run paste on main thread: {:?}", e);
        return;
    }
    match rx.await {
        Ok(Ok(())) => debug!("Text pasted successfully in {:?}", started.elapsed()),
        Ok(Err(e)) => error!("Failed to paste transcription: {}", e),
        Err(_) => error!("Paste task ended without a result"),
    }
}

#[cfg(target_os = "linux")]
async fn deliver_portal(app: &AppHandle, text: String) {
    use crate::portal::{background::notify, paste};
    use crate::settings::{get_settings, ClipboardHandling, PasteMethod};

    let settings = get_settings(app);
    let text = crate::clipboard::with_trailing_space(settings.append_trailing_space, text);
    let wants_paste = settings.paste_method != PasteMethod::None;

    match paste::try_paste(app, &text).await {
        Some(Ok(paste::PasteOutcome::Pasted)) => return,
        Some(Ok(paste::PasteOutcome::ClipboardOnly)) => {
            notify(
                "Transcript copied",
                "No app accepted the paste. Press Ctrl+V to paste it.",
            );
            return;
        }
        Some(Ok(paste::PasteOutcome::Superseded)) => {
            notify(
                "Transcript saved to History",
                "Another app changed the clipboard before Footy could paste. Open History in Footy to copy it.",
            );
            return;
        }
        Some(Err(e)) => warn!("Automatic paste failed: {e}"),
        None => {}
    }

    if !wants_paste && settings.clipboard_handling == ClipboardHandling::DontModify {
        // Output is configured as History only.
        return;
    }

    match copy_text(app, &text).await {
        Ok(CopyOutcome::Copied) => {
            if wants_paste {
                notify(
                    "Transcript copied",
                    "Press Ctrl+V to paste it. Turn on automatic paste in Footy to skip this step.",
                );
            }
        }
        Ok(CopyOutcome::NotFocused) | Err(_) => notify(
            "Transcript saved to History",
            "Footy can't use the clipboard in the background. Open History in Footy to copy it, or turn on automatic paste.",
        ),
    }
}

/// Tray "Copy last transcript". Returns after the copy attempt.
pub async fn copy_from_tray(app: &AppHandle, text: String) {
    match copy_text(app, &text).await {
        Ok(CopyOutcome::Copied) => log::info!("Copied last transcript to clipboard via tray."),
        Ok(CopyOutcome::NotFocused) => {
            #[cfg(target_os = "linux")]
            {
                // Bring Footy forward; GTK may write the clipboard once the
                // compositor gives the window focus.
                crate::show_main_window(app);
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if crate::portal::paste::main_window_focused(app) {
                        if let Ok(CopyOutcome::Copied) = copy_text(app, &text).await {
                            log::info!("Copied last transcript after focusing Footy.");
                            return;
                        }
                        break;
                    }
                }
                crate::portal::background::notify(
                    "Couldn't copy the transcript",
                    "Open History in Footy and copy it from there.",
                );
            }
        }
        Err(e) => error!("Failed to copy last transcript to clipboard: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub fn get_desktop_integration_status(app: AppHandle) -> DesktopIntegrationStatus {
    #[cfg(target_os = "linux")]
    if let Some(state) = app.try_state::<crate::portal::IntegrationState>() {
        return state.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    }
    let _ = app;
    DesktopIntegrationStatus::initial()
}

#[tauri::command]
#[specta::specta]
pub fn retry_portal_shortcuts(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if portal_mode() {
        return crate::portal::shortcuts::retry(&app);
    }
    let _ = app;
    Err("Global shortcut portal is not used here".to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn configure_portal_shortcuts(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if portal_mode() {
        return crate::portal::shortcuts::configure(&app).await;
    }
    let _ = app;
    Err("Global shortcut portal is not used here".to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn set_portal_paste_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if portal_mode() {
        return crate::portal::paste::set_enabled(&app, enabled).await;
    }
    let _ = (app, enabled);
    Err("Automatic paste through the portal is only used in the Flatpak on Wayland".to_string())
}
