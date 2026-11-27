//! GlobalShortcuts portal session.
//!
//! Binds the two transcription actions once per session with stable IDs and
//! forwards Activated/Deactivated signals for *this* session to the shared
//! shortcut handler, preserving push-to-talk press/release semantics.

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut};
use ashpd::desktop::{ResponseError, Session};
use futures_util::{FutureExt, StreamExt};
use log::{debug, info, warn};
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, oneshot};

use super::{keys, session_path, set_shortcut_state, update_status};
use crate::desktop_integration::{PortalShortcut, PortalState};
use crate::settings::{self, get_settings};
use crate::shortcut::handle_shortcut_event;

/// Binding IDs offered to the compositor. The IDs are persisted by the
/// compositor, so they must not change.
const PORTAL_BINDINGS: [&str; 2] = ["transcribe", "transcribe_with_post_process"];

enum Command {
    Retry,
    Configure(oneshot::Sender<Result<(), String>>),
    Shutdown(oneshot::Sender<()>),
}

pub struct PortalShortcuts {
    tx: mpsc::UnboundedSender<Command>,
}

/// Start the portal shortcut session (idempotent).
pub fn start(app: &AppHandle) {
    if app.try_state::<PortalShortcuts>().is_some() {
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    app.manage(PortalShortcuts { tx });
    let app = app.clone();
    tauri::async_runtime::spawn(run(app, rx));
}

pub fn retry(app: &AppHandle) -> Result<(), String> {
    match app.try_state::<PortalShortcuts>() {
        Some(state) => state
            .tx
            .send(Command::Retry)
            .map_err(|_| "Shortcut portal task is not running".to_string()),
        None => {
            start(app);
            Ok(())
        }
    }
}

pub async fn configure(app: &AppHandle) -> Result<(), String> {
    let state = app
        .try_state::<PortalShortcuts>()
        .ok_or("Shortcuts are not initialised yet")?;
    let (reply, rx) = oneshot::channel();
    state
        .tx
        .send(Command::Configure(reply))
        .map_err(|_| "Shortcut portal task is not running".to_string())?;
    rx.await
        .map_err(|_| "Shortcut portal task stopped".to_string())?
}

pub async fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<PortalShortcuts>() {
        let (done, rx) = oneshot::channel();
        if state.tx.send(Command::Shutdown(done)).is_ok() {
            let _ = rx.await;
        }
    }
}

enum End {
    /// Session ended; wait for the user to retry.
    Idle,
    /// Start a new session right away.
    Restart,
    Shutdown,
}

async fn run(app: AppHandle, mut rx: mpsc::UnboundedReceiver<Command>) {
    loop {
        match run_session(&app, &mut rx).await {
            End::Shutdown => return,
            End::Restart => continue,
            End::Idle => loop {
                match rx.recv().await {
                    Some(Command::Retry) => break,
                    Some(Command::Configure(reply)) => {
                        let _ = reply.send(Err(
                            "Shortcuts are not connected. Use Retry first.".to_string()
                        ));
                    }
                    Some(Command::Shutdown(done)) => {
                        let _ = done.send(());
                        return;
                    }
                    None => return,
                }
            },
        }
    }
}

fn new_shortcuts(app: &AppHandle) -> Vec<NewShortcut> {
    let bindings = settings::get_bindings(app);
    let defaults = settings::get_default_settings().bindings;
    PORTAL_BINDINGS
        .iter()
        .filter_map(|id| {
            let binding = bindings.get(*id).or_else(|| defaults.get(*id))?;
            let trigger = keys::xdg_trigger(&binding.current_binding);
            Some(
                NewShortcut::new(*id, binding.description.clone())
                    .preferred_trigger(trigger.as_deref()),
            )
        })
        .collect()
}

fn to_status(shortcuts: &[Shortcut]) -> Vec<PortalShortcut> {
    shortcuts
        .iter()
        .filter(|s| PORTAL_BINDINGS.contains(&s.id()))
        .map(|s| PortalShortcut {
            id: s.id().to_string(),
            description: s.description().to_string(),
            trigger: s.trigger_description().to_string(),
        })
        .collect()
}

/// Whether an activation for `id` should reach the handler.
fn accepts(app: &AppHandle, id: &str) -> bool {
    match id {
        "transcribe" => true,
        "transcribe_with_post_process" => get_settings(app).post_process_enabled,
        _ => false,
    }
}

fn release_held(app: &AppHandle, held: &mut HashSet<String>, reason: &str) {
    for id in held.drain() {
        info!("Releasing portal shortcut '{id}' because {reason}");
        handle_shortcut_event(app, &id, "portal", false);
    }
}

async fn run_session(app: &AppHandle, rx: &mut mpsc::UnboundedReceiver<Command>) -> End {
    set_shortcut_state(app, PortalState::Connecting, None);

    let portal = match GlobalShortcuts::new().await {
        Ok(portal) => portal,
        Err(e) => {
            warn!("GlobalShortcuts portal unavailable: {e}");
            set_shortcut_state(
                app,
                PortalState::Unsupported,
                Some(format!("This desktop does not offer global shortcuts: {e}")),
            );
            return End::Idle;
        }
    };
    // `version` is a property of every portal interface; 1 lacks ConfigureShortcuts.
    let version = portal.get_property::<u32>("version").await.unwrap_or(1);
    update_status(app, |s| s.shortcuts_version = version);

    // Subscribe before binding so no activation can be missed.
    let (activated, deactivated, changed) = match futures_util::try_join!(
        portal.receive_activated(),
        portal.receive_deactivated(),
        portal.receive_shortcuts_changed()
    ) {
        Ok(streams) => streams,
        Err(e) => {
            set_shortcut_state(app, PortalState::Failed, Some(e.to_string()));
            return End::Idle;
        }
    };
    let mut activated = Box::pin(activated);
    let mut deactivated = Box::pin(deactivated);
    let mut changed = Box::pin(changed);

    let session: Session<'_, GlobalShortcuts<'_>> = match AssertUnwindSafe(portal.create_session())
        .catch_unwind()
        .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            set_shortcut_state(app, PortalState::Failed, Some(e.to_string()));
            return End::Idle;
        }
        Err(_) => {
            set_shortcut_state(
                app,
                PortalState::Failed,
                Some("The portal returned an unexpected session handle".into()),
            );
            return End::Idle;
        }
    };
    let Some(our_path) = session_path(&session) else {
        set_shortcut_state(app, PortalState::Failed, Some("Invalid session".into()));
        return End::Idle;
    };
    let mut closed = match super::session_closed(&portal, &our_path).await {
        Ok(stream) => Box::pin(stream),
        Err(e) => {
            set_shortcut_state(app, PortalState::Failed, Some(e));
            let _ = session.close().await;
            return End::Idle;
        }
    };

    let shortcuts = new_shortcuts(app);
    let bound = match portal.bind_shortcuts(&session, &shortcuts, None).await {
        Ok(request) => request.response(),
        Err(e) => Err(e),
    };
    match bound {
        Ok(bound) => {
            let triggers = to_status(bound.shortcuts());
            info!(
                "Portal shortcuts bound (v{version}): {:?}",
                triggers
                    .iter()
                    .map(|t| format!("{}={}", t.id, t.trigger))
                    .collect::<Vec<_>>()
            );
            update_status(app, |s| {
                s.shortcuts = triggers;
                s.shortcuts_state = PortalState::Active;
                s.shortcuts_error = None;
            });
        }
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => {
            info!("User declined the shortcut request");
            set_shortcut_state(app, PortalState::Denied, None);
            let _ = session.close().await;
            return End::Idle;
        }
        Err(e) => {
            warn!("BindShortcuts failed: {e}");
            set_shortcut_state(app, PortalState::Failed, Some(e.to_string()));
            let _ = session.close().await;
            return End::Idle;
        }
    }

    let mut held: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            event = activated.next() => {
                let Some(event) = event else {
                    release_held(app, &mut held, "the portal connection closed");
                    set_shortcut_state(app, PortalState::Closed, None);
                    return End::Idle;
                };
                if event.session_handle() != our_path.as_ref() {
                    debug!("Ignoring activation for another session");
                    continue;
                }
                let id = event.shortcut_id();
                if !accepts(app, id) {
                    continue;
                }
                // Every activation reaches the handler: in toggle mode each one
                // toggles, and some triggers never send Deactivated (KDE's
                // kglobalaccel invokeShortcut). In push-to-talk mode the
                // coordinator ignores presses while already recording. `held`
                // only tracks keys to release if the session closes.
                held.insert(id.to_string());
                handle_shortcut_event(app, id, "portal", true);
            }
            event = deactivated.next() => {
                let Some(event) = event else { continue };
                if event.session_handle() != our_path.as_ref() {
                    continue;
                }
                let id = event.shortcut_id();
                if held.remove(id) {
                    handle_shortcut_event(app, id, "portal", false);
                }
            }
            event = changed.next() => {
                if let Some(event) = event {
                    if event.session_handle() == our_path.as_ref() {
                        let triggers = to_status(event.shortcuts());
                        update_status(app, |s| s.shortcuts = triggers);
                    }
                }
            }
            _ = closed.next() => {
                info!("Portal shortcut session was closed by the desktop");
                release_held(app, &mut held, "the shortcut session closed");
                update_status(app, |s| {
                    s.shortcuts_state = PortalState::Closed;
                    s.shortcuts.clear();
                });
                return End::Idle;
            }
            command = rx.recv() => match command {
                Some(Command::Retry) => {
                    release_held(app, &mut held, "shortcuts are reconnecting");
                    let _ = session.close().await;
                    return End::Restart;
                }
                Some(Command::Configure(reply)) => {
                    let _ = reply.send(configure_session(&portal, &session, version).await);
                }
                Some(Command::Shutdown(done)) => {
                    release_held(app, &mut held, "Footy is exiting");
                    let _ = session.close().await;
                    let _ = done.send(());
                    return End::Shutdown;
                }
                None => {
                    let _ = session.close().await;
                    return End::Shutdown;
                }
            }
        }
    }
}

async fn configure_session(
    portal: &GlobalShortcuts<'_>,
    session: &Session<'_, GlobalShortcuts<'_>>,
    version: u32,
) -> Result<(), String> {
    if version < 2 {
        return Err(
            "This desktop cannot open shortcut settings for Footy. Change them in your system shortcut settings."
                .to_string(),
        );
    }
    let options: std::collections::HashMap<&str, ashpd::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    portal
        .call_method("ConfigureShortcuts", &(session, "", options))
        .await
        .map(|_| ())
        .map_err(|e| format!("Failed to open shortcut settings: {e}"))
}
