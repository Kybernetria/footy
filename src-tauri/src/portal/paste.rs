//! Optional automatic paste through the RemoteDesktop portal (keyboard only)
//! and its Clipboard extension.
//!
//! The user enables this explicitly. Footy then holds one session that:
//! - may set the clipboard selection and serve it (`SelectionTransfer`)
//!   for as long as Footy owns the selection,
//! - may send the configured paste key combination as keysyms, so the
//!   compositor resolves it through the user's keyboard layout.
//!
//! Footy never requests pointer, touch or screen access.
//!
//! Selection lifetime: Footy owns the selection from `SetSelection` until
//! another client copies something (`SelectionOwnerChanged`), the session
//! closes, or Footy exits. While it owns it, Footy serves the transcript, or
//! the restored previous clipboard. If the previous clipboard was empty or
//! held only formats Footy cannot copy, the transcript stays on the
//! clipboard after pasting.

use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};
use ashpd::zvariant::OwnedObjectPath;
use futures_util::{FutureExt, StreamExt};
use log::{debug, info, warn};
use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;
use tokio::sync::watch;

use super::keys::keysym;
use super::request::{
    call_with_response, new_token, options_with_token, RESPONSE_CANCELLED, RESPONSE_SUCCESS,
};
use super::{session_path, update_status};
use crate::desktop_integration::{CopyOutcome, PortalState};
use crate::settings::{
    self, get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod, SETTINGS_STORE_PATH,
};

const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
const RESTORE_TOKEN_KEY: &str = "portal_remote_desktop_restore_token";
const TRANSFER_TIMEOUT: Duration = Duration::from_millis(2000);
const READ_TIMEOUT: Duration = Duration::from_millis(1000);
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const KEYBOARD: u32 = 1;

/// Data Footy offers on the clipboard, keyed by MIME type.
#[derive(Clone, Default)]
pub(crate) struct Offer {
    data: Vec<(String, Arc<Vec<u8>>)>,
}

impl Offer {
    fn text(text: &str) -> Self {
        let bytes = Arc::new(text.as_bytes().to_vec());
        Self {
            data: TEXT_MIMES
                .iter()
                .map(|m| (m.to_string(), bytes.clone()))
                .collect(),
        }
    }

    fn mimes(&self) -> Vec<&str> {
        self.data.iter().map(|(m, _)| m.as_str()).collect()
    }

    fn get(&self, mime: &str) -> Option<Arc<Vec<u8>>> {
        self.data
            .iter()
            .find(|(m, _)| m == mime)
            .map(|(_, d)| d.clone())
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[derive(Default)]
struct Selection {
    /// What Footy serves while it owns the selection.
    owned: Option<Offer>,
    /// MIME types of the last foreign owner; `None` until the portal reports
    /// an owner change.
    foreign_mimes: Option<Vec<String>>,
    /// Incremented whenever another client takes the selection.
    foreign_generation: u64,
}

/// What a paste attempt achieved.
#[derive(Debug, PartialEq, Eq)]
pub enum PasteOutcome {
    /// The focused application fetched the transcript after the key combo.
    Pasted,
    /// The transcript is on the clipboard but no application fetched it.
    ClipboardOnly,
}

pub struct PasteSession {
    rd: RemoteDesktop<'static>,
    clipboard: Clipboard<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    path: OwnedObjectPath,
    selection: Mutex<Selection>,
    transfers: watch::Sender<u64>,
    alive: AtomicBool,
}

#[derive(Default)]
pub struct PasteState {
    session: Mutex<Option<Arc<PasteSession>>>,
    /// Serialises paste, copy and enable/disable operations.
    op: tokio::sync::Mutex<()>,
}

fn state(app: &AppHandle) -> tauri::State<'_, PasteState> {
    if app.try_state::<PasteState>().is_none() {
        app.manage(PasteState::default());
    }
    app.state::<PasteState>()
}

fn active_session(app: &AppHandle) -> Option<Arc<PasteSession>> {
    let state = state(app);
    let guard = state.session.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|s| s.alive.load(Ordering::SeqCst))
        .cloned()
}

fn set_paste_state(app: &AppHandle, state: PortalState, error: Option<String>) {
    update_status(app, |s| {
        s.paste_state = state;
        s.paste_error = error;
    });
}

// ---------------------------------------------------------------------------
// Restore token (kept out of AppSettings so it never reaches the webview)
// ---------------------------------------------------------------------------

fn load_token(app: &AppHandle) -> Option<String> {
    let store = app
        .store(crate::portable::store_path(SETTINGS_STORE_PATH))
        .ok()?;
    store
        .get(RESTORE_TOKEN_KEY)
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|t| !t.is_empty())
}

fn save_token(app: &AppHandle, token: Option<&str>) {
    let Ok(store) = app.store(crate::portable::store_path(SETTINGS_STORE_PATH)) else {
        return;
    };
    match token {
        Some(token) => store.set(RESTORE_TOKEN_KEY, serde_json::Value::from(token)),
        None => {
            store.delete(RESTORE_TOKEN_KEY);
        }
    }
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

pub fn restore_on_startup(app: &AppHandle) {
    let _ = state(app);
    let enabled = get_settings(app).portal_paste_enabled;
    update_status(app, |s| s.paste_enabled = enabled);
    if !enabled {
        set_paste_state(app, PortalState::Inactive, None);
        return;
    }
    if load_token(app).is_none() {
        set_paste_state(
            app,
            PortalState::Closed,
            Some("Reconnect automatic paste to allow it again.".into()),
        );
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = connect(&app, false).await {
            warn!("Automatic paste did not reconnect: {e}");
        }
    });
}

/// Turn automatic paste on (asks for consent) or off.
pub async fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let mut s = get_settings(app);
    s.portal_paste_enabled = enabled;
    settings::write_settings(app, s);
    update_status(app, |s| s.paste_enabled = enabled);

    if enabled {
        connect(app, true).await
    } else {
        disconnect(app, true).await;
        set_paste_state(app, PortalState::Inactive, None);
        Ok(())
    }
}

pub async fn shutdown(app: &AppHandle) {
    disconnect(app, false).await;
}

async fn disconnect(app: &AppHandle, forget_grant: bool) {
    let state = state(app);
    let _op = state.op.lock().await;
    let session = state
        .session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(session) = session {
        session.alive.store(false, Ordering::SeqCst);
        let _ = session.session.close().await;
    }
    if forget_grant {
        save_token(app, None);
    }
}

async fn connect(app: &AppHandle, interactive: bool) -> Result<(), String> {
    disconnect(app, false).await;
    let state = state(app);
    let _op = state.op.lock().await;
    set_paste_state(app, PortalState::Connecting, None);

    let token = load_token(app);
    if token.is_none() && !interactive {
        let message = "Automatic paste needs permission.".to_string();
        set_paste_state(app, PortalState::Inactive, Some(message.clone()));
        return Err(message);
    }

    match start_session(token.as_deref()).await {
        Ok((session, new_token)) => {
            // Restore tokens are single-use: always keep the new one (or none).
            save_token(app, new_token.as_deref());
            let session = Arc::new(session);
            *state.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
            let app_for_close = app.clone();
            tauri::async_runtime::spawn(listen(session, move |state, message| {
                set_paste_state(&app_for_close, state, Some(message));
            }));
            set_paste_state(app, PortalState::Active, None);
            info!("Automatic paste session started");
            Ok(())
        }
        Err(failure) => {
            // A stale or revoked restore token must not be offered again.
            if failure.token_rejected {
                save_token(app, None);
            }
            set_paste_state(app, failure.state, Some(failure.message.clone()));
            Err(failure.message)
        }
    }
}

pub(crate) struct StartFailure {
    state: PortalState,
    message: String,
    /// The offered restore token was not accepted and should be forgotten.
    token_rejected: bool,
}

/// Create and start a keyboard-only RemoteDesktop session with clipboard
/// access. Returns the session and the new restore token.
pub(crate) async fn start_session(
    token: Option<&str>,
) -> Result<(PasteSession, Option<String>), StartFailure> {
    let fail = |state: PortalState, message: String, token_rejected: bool| StartFailure {
        state,
        message,
        token_rejected,
    };
    let unsupported = |e: String| fail(PortalState::Unsupported, e, false);
    let failed = |e: String| fail(PortalState::Failed, e, false);

    let rd = RemoteDesktop::new().await.map_err(|e| {
        unsupported(format!(
            "This desktop does not offer remote keyboard input: {e}"
        ))
    })?;
    let available = rd
        .available_device_types()
        .await
        .map_err(|e| unsupported(e.to_string()))?;
    if !available.contains(DeviceType::Keyboard) {
        return Err(unsupported(
            "This desktop does not allow keyboard input through the portal.".into(),
        ));
    }
    let clipboard = Clipboard::new().await.map_err(|e| {
        unsupported(format!(
            "This desktop does not offer clipboard sharing: {e}"
        ))
    })?;

    let session = match AssertUnwindSafe(rd.create_session()).catch_unwind().await {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => return Err(failed(e.to_string())),
        Err(_) => return Err(failed("Unexpected session handle from portal".into())),
    };
    let path = session_path(&session).ok_or_else(|| failed("Invalid session".into()))?;

    let session_ref = &session;
    let token_offered = token.is_some();
    let close_with = |state: PortalState, message: String| async move {
        let _ = session_ref.close().await;
        Err::<(PasteSession, Option<String>), _>(fail(state, message, false))
    };
    let close_rejecting_token = |state: PortalState, message: String| async move {
        let _ = session_ref.close().await;
        Err::<(PasteSession, Option<String>), _>(fail(state, message, token_offered))
    };

    // Clipboard access has to be requested before the session starts.
    if let Err(e) = clipboard.request(&session).await {
        return close_with(
            PortalState::Unsupported,
            format!("Clipboard request failed: {e}"),
        )
        .await;
    }

    let select = rd
        .select_devices(
            &session,
            DeviceType::Keyboard.into(),
            token,
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .and_then(|r| r.response());
    if let Err(e) = select {
        return close_rejecting_token(
            PortalState::Failed,
            format!("Selecting the keyboard failed: {e}"),
        )
        .await;
    }

    let start_token = new_token();
    let response = call_with_response(
        &rd,
        "Start",
        &start_token,
        &(&session, "", options_with_token(&start_token)),
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => return close_with(PortalState::Failed, e).await,
    };

    if response.code != RESPONSE_SUCCESS {
        return if response.code == RESPONSE_CANCELLED {
            close_rejecting_token(PortalState::Denied, "Permission was declined.".into()).await
        } else {
            close_rejecting_token(
                PortalState::Failed,
                "The desktop could not start the session.".into(),
            )
            .await
        };
    }

    let new_token = response.string("restore_token");
    let devices = response.u32("devices").unwrap_or(0);
    let clipboard_enabled = response.bool("clipboard_enabled").unwrap_or(false);
    if devices & KEYBOARD == 0 {
        return close_with(
            PortalState::Denied,
            "Keyboard access was not granted.".into(),
        )
        .await;
    }
    if !clipboard_enabled {
        return close_with(
            PortalState::Denied,
            "Clipboard access was not granted.".into(),
        )
        .await;
    }

    let (transfers, _) = watch::channel(0u64);
    Ok((
        PasteSession {
            rd,
            clipboard,
            session,
            path,
            selection: Mutex::new(Selection::default()),
            transfers,
            alive: AtomicBool::new(true),
        },
        new_token,
    ))
}

/// Serve clipboard transfers and track selection ownership until the
/// session closes.
pub(crate) async fn listen(
    s: Arc<PasteSession>,
    on_end: impl FnOnce(PortalState, String) + Send + 'static,
) {
    let streams = futures_util::try_join!(
        s.clipboard.receive_selection_transfer(),
        s.clipboard.receive_selection_owner_changed(),
        async {
            super::session_closed(&s.rd, &s.path)
                .await
                .map_err(|e| ashpd::Error::Zbus(ashpd::zbus::Error::Failure(e)))
        }
    );
    let (transfers, owners, closed) = match streams {
        Ok(streams) => streams,
        Err(e) => {
            warn!("Failed to subscribe to clipboard signals: {e}");
            s.alive.store(false, Ordering::SeqCst);
            on_end(PortalState::Failed, e.to_string());
            return;
        }
    };
    let mut transfers = Box::pin(transfers);
    let mut owners = Box::pin(owners);
    let mut closed = Box::pin(closed);

    while s.alive.load(Ordering::SeqCst) {
        tokio::select! {
            transfer = transfers.next() => {
                let Some((session, mime, serial)) = transfer else { break };
                if session_path(&session).as_ref() != Some(&s.path) {
                    continue;
                }
                let offer = s.selection.lock().unwrap_or_else(|e| e.into_inner()).owned.clone();
                let s = s.clone();
                tauri::async_runtime::spawn(async move { serve(&s, offer, &mime, serial).await });
            }
            owner = owners.next() => {
                let Some((session, change)) = owner else { break };
                if session_path(&session).as_ref() != Some(&s.path) {
                    continue;
                }
                if change.session_is_owner() == Some(true) {
                    debug!("Footy owns the clipboard selection");
                } else {
                    debug!("Clipboard owner changed: {:?}", change.mime_types());
                    let mut sel = s.selection.lock().unwrap_or_else(|e| e.into_inner());
                    sel.owned = None;
                    sel.foreign_mimes = Some(change.mime_types());
                    sel.foreign_generation += 1;
                }
            }
            _ = closed.next() => break,
        }
    }

    if s.alive.swap(false, Ordering::SeqCst) {
        info!("Automatic paste session closed by the desktop");
        on_end(
            PortalState::Closed,
            "The desktop ended automatic paste. Reconnect to use it again.".into(),
        );
    }
}

async fn serve(s: &PasteSession, offer: Option<Offer>, mime: &str, serial: u32) {
    let data = offer.and_then(|o| o.get(mime));
    let Some(data) = data else {
        let _ = s
            .clipboard
            .selection_write_done(&s.session, serial, false)
            .await;
        return;
    };
    let ok = match s.clipboard.selection_write(&s.session, serial).await {
        Ok(fd) => {
            let fd: std::os::fd::OwnedFd = fd.into();
            tauri::async_runtime::spawn_blocking(move || {
                let mut file = std::fs::File::from(fd);
                file.write_all(&data).and_then(|_| file.flush()).is_ok()
            })
            .await
            .unwrap_or(false)
        }
        Err(e) => {
            warn!("SelectionWrite failed: {e}");
            false
        }
    };
    let _ = s
        .clipboard
        .selection_write_done(&s.session, serial, ok)
        .await;
    if ok {
        s.transfers.send_modify(|n| *n += 1);
    }
}

// ---------------------------------------------------------------------------
// Clipboard and keyboard operations
// ---------------------------------------------------------------------------

impl PasteSession {
    async fn offer(&self, offer: Offer) -> Result<(), String> {
        let mimes: Vec<String> = offer.mimes().iter().map(|m| m.to_string()).collect();
        self.selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owned = Some(offer);
        let mimes: Vec<&str> = mimes.iter().map(String::as_str).collect();
        self.clipboard
            .set_selection(&self.session, &mimes)
            .await
            .map_err(|e| format!("Failed to set the clipboard: {e}"))
    }

    async fn read(&self, mime: &str) -> Option<Vec<u8>> {
        let fd = self
            .clipboard
            .selection_read(&self.session, mime)
            .await
            .ok()?;
        let fd: std::os::fd::OwnedFd = fd.into();
        let read = tauri::async_runtime::spawn_blocking(move || {
            let mut buf = Vec::new();
            std::fs::File::from(fd)
                .take(MAX_SNAPSHOT_BYTES as u64 + 1)
                .read_to_end(&mut buf)
                .ok()
                .filter(|_| buf.len() <= MAX_SNAPSHOT_BYTES)
                .map(|_| buf)
        });
        tokio::time::timeout(READ_TIMEOUT, read).await.ok()?.ok()?
    }

    /// Capture the current clipboard so it can be restored after pasting.
    /// Returns `None` when nothing restorable is available.
    async fn snapshot(&self) -> Option<Offer> {
        let (owned, foreign) = {
            let sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
            (sel.owned.clone(), sel.foreign_mimes.clone())
        };
        if let Some(owned) = owned {
            return Some(owned);
        }
        let advertised = foreign.unwrap_or_else(|| vec![TEXT_MIMES[0].to_string()]);
        let mut offer = Offer::default();

        let text_mimes: Vec<&String> = advertised
            .iter()
            .filter(|m| TEXT_MIMES.contains(&m.as_str()) || m.starts_with("text/plain"))
            .collect();
        if let Some(source) = TEXT_MIMES
            .iter()
            .find(|m| text_mimes.iter().any(|t| t == *m))
            .map(|m| m.to_string())
            .or_else(|| text_mimes.first().map(|m| m.to_string()))
        {
            if let Some(bytes) = self.read(&source).await {
                let bytes = Arc::new(bytes);
                for mime in &text_mimes {
                    offer.data.push((mime.to_string(), bytes.clone()));
                }
            }
        }
        if let Some(image) = advertised
            .iter()
            .find(|m| m.as_str() == "image/png")
            .or_else(|| advertised.iter().find(|m| m.starts_with("image/")))
        {
            if let Some(bytes) = self.read(image).await {
                offer.data.push((image.clone(), Arc::new(bytes)));
            }
        }
        (!offer.is_empty()).then_some(offer)
    }

    async fn keys(&self, combo: &[i32]) -> Result<(), String> {
        let mut pressed = Vec::new();
        let mut result = Ok(());
        for key in combo {
            if let Err(e) = self
                .rd
                .notify_keyboard_keysym(&self.session, *key, KeyState::Pressed)
                .await
            {
                result = Err(format!("Failed to send key: {e}"));
                break;
            }
            pressed.push(*key);
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
        for key in pressed.iter().rev() {
            if let Err(e) = self
                .rd
                .notify_keyboard_keysym(&self.session, *key, KeyState::Released)
                .await
            {
                result = result.and(Err(format!("Failed to release key: {e}")));
            }
        }
        result
    }

    async fn paste(
        &self,
        text: &str,
        combo: &[i32],
        delay: Duration,
        submit: Option<&[i32]>,
        restore: bool,
    ) -> Result<PasteOutcome, String> {
        let previous = if restore { self.snapshot().await } else { None };

        self.offer(Offer::text(text)).await?;
        let generation = self
            .selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .foreign_generation;

        // Let the compositor announce the new selection before pasting.
        tokio::time::sleep(delay).await;

        let mut transfers = self.transfers.subscribe();
        transfers.mark_unchanged();
        self.keys(combo).await?;

        let fetched = tokio::time::timeout(TRANSFER_TIMEOUT, transfers.changed())
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
        if !fetched {
            warn!("No application fetched the transcript after the paste keys");
            return Ok(PasteOutcome::ClipboardOnly);
        }

        if let Some(submit) = submit {
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.keys(submit).await?;
        }

        if let Some(previous) = previous {
            // Give the target a moment to request further formats.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let still_ours = self
                .selection
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .foreign_generation
                == generation;
            if still_ours {
                if let Err(e) = self.offer(previous).await {
                    warn!("Failed to restore the previous clipboard: {e}");
                }
            } else {
                debug!("Clipboard changed during paste; not restoring");
            }
        }
        Ok(PasteOutcome::Pasted)
    }
}

pub fn paste_combo(method: PasteMethod) -> &'static [i32] {
    match method {
        PasteMethod::CtrlShiftV => &[keysym::CONTROL_L, keysym::SHIFT_L, keysym::LOWER_V],
        PasteMethod::ShiftInsert => &[keysym::SHIFT_L, keysym::INSERT],
        // Direct typing and external scripts are unavailable in the sandbox,
        // so they paste with Ctrl+V.
        PasteMethod::CtrlV
        | PasteMethod::Direct
        | PasteMethod::ExternalScript
        | PasteMethod::None => &[keysym::CONTROL_L, keysym::LOWER_V],
    }
}

pub fn submit_combo(key: AutoSubmitKey) -> &'static [i32] {
    match key {
        AutoSubmitKey::Enter => &[keysym::RETURN],
        AutoSubmitKey::CtrlEnter => &[keysym::CONTROL_L, keysym::RETURN],
        AutoSubmitKey::CmdEnter => &[keysym::SUPER_L, keysym::RETURN],
    }
}

/// Paste through the portal session if automatic paste is active.
/// Returns `None` when no session is available.
pub async fn try_paste(app: &AppHandle, text: &str) -> Option<Result<PasteOutcome, String>> {
    let settings = get_settings(app);
    if !settings.portal_paste_enabled || settings.paste_method == PasteMethod::None {
        return None;
    }
    let session = active_session(app)?;
    let state = state(app);
    let _op = state.op.lock().await;
    let submit = settings
        .auto_submit
        .then(|| submit_combo(settings.auto_submit_key));
    let restore = settings.clipboard_handling == ClipboardHandling::DontModify;
    let delay = Duration::from_millis(settings.paste_delay_ms.clamp(20, 1000));
    Some(
        session
            .paste(
                text,
                paste_combo(settings.paste_method),
                delay,
                submit,
                restore,
            )
            .await,
    )
}

/// Put `text` on the clipboard: through the portal session when available,
/// otherwise through GTK while Footy's window has focus.
pub async fn copy_text(app: &AppHandle, text: &str) -> Result<CopyOutcome, String> {
    if let Some(session) = active_session(app) {
        let state = state(app);
        let _op = state.op.lock().await;
        match session.offer(Offer::text(text)).await {
            Ok(()) => return Ok(CopyOutcome::Copied),
            Err(e) => warn!("Portal clipboard write failed, trying GTK: {e}"),
        }
    }
    if !main_window_focused(app) {
        return Ok(CopyOutcome::NotFocused);
    }
    gtk_copy(app, text.to_string()).await?;
    Ok(CopyOutcome::Copied)
}

pub fn main_window_focused(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false)
}

/// GTK clipboard writes only reach Wayland while the window has focus, so
/// callers check focus first. Runs on the GTK main thread.
async fn gtk_copy(app: &AppHandle, text: String) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        clipboard.set_text(&text);
        clipboard.store();
        let _ = tx.send(());
    })
    .map_err(|e| format!("Failed to reach the GTK main thread: {e}"))?;
    rx.await
        .map_err(|_| "GTK clipboard write did not complete".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_offer_covers_common_text_targets() {
        let offer = Offer::text("hallo wörld");
        for mime in TEXT_MIMES {
            assert_eq!(
                offer.get(mime).as_deref().map(Vec::as_slice),
                Some("hallo wörld".as_bytes())
            );
        }
        assert!(offer.get("image/png").is_none());
    }

    #[test]
    fn paste_combos_match_paste_methods() {
        assert_eq!(
            paste_combo(PasteMethod::CtrlV),
            &[keysym::CONTROL_L, keysym::LOWER_V]
        );
        assert_eq!(
            paste_combo(PasteMethod::Direct),
            paste_combo(PasteMethod::CtrlV)
        );
        assert_eq!(
            paste_combo(PasteMethod::ShiftInsert),
            &[keysym::SHIFT_L, keysym::INSERT]
        );
        assert_eq!(
            paste_combo(PasteMethod::CtrlShiftV),
            &[keysym::CONTROL_L, keysym::SHIFT_L, keysym::LOWER_V]
        );
        assert_eq!(
            submit_combo(AutoSubmitKey::CtrlEnter),
            &[keysym::CONTROL_L, keysym::RETURN]
        );
    }
}

/// Protocol test against `mock_portal.py` on a private session bus (see the
/// Flatpak test harness). Run with:
/// `cargo test --lib portal::paste::mock_portal -- --ignored`
#[cfg(test)]
mod mock_portal {
    use super::*;
    use ashpd::zbus;

    async fn mock() -> zbus::Proxy<'static> {
        let conn = zbus::Connection::session().await.expect("session bus");
        zbus::Proxy::new(&conn, "org.footy.Mock", "/org/footy/Mock", "org.footy.Mock")
            .await
            .expect("mock proxy")
    }

    async fn log(m: &zbus::Proxy<'_>) -> Vec<serde_json::Value> {
        let (json,): (String,) = m.call("Log", &()).await.expect("Log");
        serde_json::from_str(&json).expect("log json")
    }

    async fn pasted(m: &zbus::Proxy<'_>) -> Vec<(Option<String>, bool)> {
        log(m)
            .await
            .into_iter()
            .filter(|e| e["event"] == "PastedText")
            .map(|e| (e["text"].as_str().map(str::to_string), e["ok"] == true))
            .collect()
    }

    async fn transfer(m: &zbus::Proxy<'_>, mime: &str) -> (Option<String>, bool) {
        let before = pasted(m).await.len();
        let _: () = m.call("RequestTransfer", &(mime,)).await.unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let all = pasted(m).await;
            if all.len() > before {
                return all.last().cloned().unwrap();
            }
        }
        panic!("no transfer completed for {mime}");
    }

    async fn foreign(m: &zbus::Proxy<'_>, mimes: &[&str], text: &str) {
        let _: () = m.call("SetForeignSelection", &(mimes, text)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs mock_portal.py on a private session bus"]
    async fn paste_flow() {
        let m = mock().await;
        let (session, token) = match start_session(Some("seed-token")).await {
            Ok(started) => started,
            Err(f) => panic!("start failed: {:?} {}", f.state, f.message),
        };
        assert!(token.unwrap().starts_with("tok-"), "new restore token");
        let s = Arc::new(session);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_flag = closed.clone();
        tokio::spawn(listen(s.clone(), move |state, _| {
            assert_eq!(state, PortalState::Closed);
            closed_flag.store(true, Ordering::SeqCst);
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ctrl_v = paste_combo(PasteMethod::CtrlV);
        let delay = Duration::from_millis(30);

        // 1. Paste restores the previous text clipboard afterwards.
        foreign(
            &m,
            &["text/plain;charset=utf-8", "UTF8_STRING"],
            "previous clip",
        )
        .await;
        let outcome = s.paste("dictated ü", ctrl_v, delay, None, true).await;
        assert_eq!(outcome, Ok(PasteOutcome::Pasted));
        assert_eq!(
            pasted(&m).await.last().cloned(),
            Some((Some("dictated ü".into()), true)),
            "the target app received the transcript through the fd"
        );
        assert_eq!(
            transfer(&m, "UTF8_STRING").await,
            (Some("previous clip".into()), true),
            "previous clipboard restored and served"
        );

        // 2. CopyToClipboard mode (restore = false) leaves the transcript.
        let outcome = s.paste("second", ctrl_v, delay, None, false).await;
        assert_eq!(outcome, Ok(PasteOutcome::Pasted));
        assert_eq!(
            transfer(&m, "text/plain;charset=utf-8").await,
            (Some("second".into()), true)
        );

        // 3. If the user copies something while Footy pastes, Footy does not
        //    overwrite it during restoration.
        foreign(&m, &["text/plain;charset=utf-8"], "older").await;
        let m2 = mock().await;
        let racer = tokio::spawn(async move {
            for _ in 0..100 {
                if pasted(&m2)
                    .await
                    .last()
                    .and_then(|p| p.0.clone())
                    .as_deref()
                    == Some("third")
                {
                    foreign(&m2, &["text/plain;charset=utf-8"], "user copied meanwhile").await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let outcome = s.paste("third", ctrl_v, delay, None, true).await;
        racer.await.unwrap();
        assert_eq!(outcome, Ok(PasteOutcome::Pasted));
        assert_eq!(
            transfer(&m, "text/plain;charset=utf-8").await,
            (None, false),
            "Footy no longer serves anything after another client took the selection"
        );

        // 4. No app fetches the data (the mock only reacts to Ctrl+V):
        //    the transcript stays on the clipboard and is reported as such.
        let outcome = s
            .paste(
                "unclaimed",
                paste_combo(PasteMethod::ShiftInsert),
                delay,
                None,
                true,
            )
            .await;
        assert_eq!(outcome, Ok(PasteOutcome::ClipboardOnly));
        assert_eq!(
            transfer(&m, "text/plain").await,
            (Some("unclaimed".into()), true)
        );

        // 5. Auto-submit sends Return after the transfer.
        let outcome = s
            .paste(
                "submit me",
                ctrl_v,
                delay,
                Some(submit_combo(AutoSubmitKey::Enter)),
                false,
            )
            .await;
        assert_eq!(outcome, Ok(PasteOutcome::Pasted));
        let keys: Vec<String> = log(&m)
            .await
            .into_iter()
            .filter(|e| e["event"] == "RemoteDesktop.NotifyKeyboardKeysym")
            .map(|e| format!("{}:{}", e["keysym"].as_str().unwrap(), e["state"]))
            .collect();
        let tail: Vec<&str> = keys
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            ["0xffe3:1", "0x76:1", "0x76:0", "0xffe3:0", "0xff0d:1", "0xff0d:0"],
            "Ctrl+V pressed and released in order, then Return"
        );

        // 6. Closure from the desktop ends the session.
        let _: () = m.call("CloseSession", &("rd",)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(closed.load(Ordering::SeqCst), "Closed(a{{sv}}) observed");
        assert!(!s.alive.load(Ordering::SeqCst));
    }
}
