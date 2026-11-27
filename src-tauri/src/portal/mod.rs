//! XDG desktop portal integration for the Flatpak build on Wayland.
//!
//! Inside a Flatpak sandbox on Wayland, Footy cannot grab keys, run host
//! typing tools or write the clipboard while unfocused. This module replaces
//! those paths with portals the user consents to:
//!
//! - `shortcuts`: GlobalShortcuts session that feeds press/release events into
//!   the existing shortcut handler.
//! - `paste`: optional keyboard-only RemoteDesktop session with the Clipboard
//!   extension, used to place the transcript on the clipboard and send the
//!   paste key combination.
//! - `background`: Background portal for launch at login, plus desktop
//!   notifications for transcripts that could not be delivered.
//!
//! Native installs (and Flatpak on X11) keep the existing integration.

pub mod background;
pub mod keys;
pub mod paste;
mod request;
pub mod shortcuts;

use once_cell::sync::OnceCell;
use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

use crate::desktop_integration::{DesktopIntegrationStatus, PortalState};

/// GTK display backend detected after GTK initialisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBackend {
    Wayland,
    X11,
    Other,
}

static DISPLAY_BACKEND: OnceCell<DisplayBackend> = OnceCell::new();

/// True when running inside a Flatpak sandbox.
pub fn is_flatpak() -> bool {
    static FLATPAK: OnceCell<bool> = OnceCell::new();
    *FLATPAK.get_or_init(|| Path::new("/.flatpak-info").exists())
}

/// Record GTK's actual display backend. Must run on the GTK main thread
/// after GTK is initialised (Tauri's setup hook).
pub fn detect_display_backend() {
    let backend = gtk::gdk::Display::default()
        .map(|display| {
            use gtk::glib::prelude::ObjectExt;
            backend_from_type_name(display.type_().name())
        })
        .unwrap_or(DisplayBackend::Other);
    let _ = DISPLAY_BACKEND.set(backend);
    log::info!(
        "Display backend: {:?} (flatpak: {}, portal mode: {})",
        backend,
        is_flatpak(),
        portal_mode()
    );
}

fn backend_from_type_name(name: &str) -> DisplayBackend {
    match name {
        "GdkWaylandDisplay" => DisplayBackend::Wayland,
        "GdkX11Display" => DisplayBackend::X11,
        _ => DisplayBackend::Other,
    }
}

fn display_backend() -> DisplayBackend {
    if let Some(backend) = DISPLAY_BACKEND.get() {
        return *backend;
    }
    // Before GTK is up, use the same preference order GDK applies.
    let gdk_backend = std::env::var("GDK_BACKEND").unwrap_or_default();
    if gdk_backend.starts_with("x11") {
        DisplayBackend::X11
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        DisplayBackend::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        DisplayBackend::X11
    } else {
        DisplayBackend::Other
    }
}

fn wayland_session_env() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

/// Flatpak on Wayland: shortcuts, paste and background clipboard writes go
/// through portals. Independent of the persisted keyboard implementation.
/// This also covers GTK forced onto XWayland in a Wayland session, where X11
/// key grabs and synthetic input would only reach XWayland clients.
pub fn portal_mode() -> bool {
    is_flatpak() && (display_backend() == DisplayBackend::Wayland || wayland_session_env())
}

/// Shared, observable status for the settings UI.
pub struct IntegrationState(pub Mutex<DesktopIntegrationStatus>);

pub const STATUS_EVENT: &str = "desktop-integration-changed";

pub fn update_status(app: &AppHandle, f: impl FnOnce(&mut DesktopIntegrationStatus)) {
    let Some(state) = app.try_state::<IntegrationState>() else {
        return;
    };
    let snapshot = {
        let mut status = state.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut status);
        status.clone()
    };
    let _ = app.emit(STATUS_EVENT, snapshot);
}

/// Start portal sessions for the current environment. Called once from setup.
pub fn init(app: &AppHandle) {
    if !portal_mode() {
        return;
    }
    // Launch-at-login availability is independent of shortcut consent.
    background::probe(app);
    // Automatic paste only reconnects silently when a previous grant left a
    // restore token; otherwise it waits for the user to enable it.
    paste::restore_on_startup(app);
}

/// Close portal sessions before exit (bounded).
pub fn shutdown(app: &AppHandle) {
    if !portal_mode() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::block_on(async move {
        let wait = async {
            shortcuts::shutdown(&app).await;
            paste::shutdown(&app).await;
        };
        if tokio::time::timeout(std::time::Duration::from_millis(500), wait)
            .await
            .is_err()
        {
            log::warn!("Timed out closing portal sessions");
        }
    });
}

pub(crate) fn set_shortcut_state(app: &AppHandle, state: PortalState, error: Option<String>) {
    update_status(app, |s| {
        s.shortcuts_state = state;
        s.shortcuts_error = error;
    });
}

/// Extract the object path of an ashpd session (ashpd keeps it private but
/// serialises the session as its object path).
pub(crate) fn session_path<T: ashpd::desktop::SessionPortal>(
    session: &ashpd::desktop::Session<'_, T>,
) -> Option<ashpd::zvariant::OwnedObjectPath> {
    use ashpd::zvariant::{serialized::Context, to_bytes, LE};
    let ctxt = Context::new_dbus(LE, 0);
    let data = to_bytes(ctxt, session).ok()?;
    data.deserialize::<ashpd::zvariant::OwnedObjectPath>()
        .ok()
        .map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_gdk_display_types() {
        assert_eq!(
            backend_from_type_name("GdkWaylandDisplay"),
            DisplayBackend::Wayland
        );
        assert_eq!(backend_from_type_name("GdkX11Display"), DisplayBackend::X11);
        assert_eq!(
            backend_from_type_name("GdkBroadwayDisplay"),
            DisplayBackend::Other
        );
    }
}
