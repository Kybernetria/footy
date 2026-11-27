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

use super::{keys, session_path, update_status};
use crate::desktop_integration::{DesktopIntegrationStatus, PortalShortcut, PortalState};
use crate::settings::{self, get_settings};
use crate::shortcut::handle_shortcut_event;

/// Binding IDs offered to the compositor. The IDs are persisted by the
/// compositor, so they must not change.
const PORTAL_BINDINGS: [&str; 2] = ["transcribe", "transcribe_with_post_process"];

/// What the session loop needs from the app; implemented by `AppHandle`
/// and by a recording host in the mock-portal test.
pub(crate) trait Host: Send + Sync + 'static {
    fn shortcuts(&self) -> Vec<NewShortcut>;
    /// Whether an activation for `id` should reach the handler.
    fn accepts(&self, id: &str) -> bool;
    fn event(&self, id: &str, pressed: bool);
    fn update(&self, f: impl FnOnce(&mut DesktopIntegrationStatus));
}

impl Host for AppHandle {
    fn shortcuts(&self) -> Vec<NewShortcut> {
        new_shortcuts(self)
    }

    fn accepts(&self, id: &str) -> bool {
        match id {
            "transcribe" => true,
            "transcribe_with_post_process" => get_settings(self).post_process_enabled,
            _ => false,
        }
    }

    fn event(&self, id: &str, pressed: bool) {
        handle_shortcut_event(self, id, "portal", pressed);
    }

    fn update(&self, f: impl FnOnce(&mut DesktopIntegrationStatus)) {
        update_status(self, f);
    }
}

fn set_state<H: Host>(host: &H, state: PortalState, error: Option<String>) {
    host.update(|s| {
        s.shortcuts_state = state;
        s.shortcuts_error = error;
    });
}

pub(crate) enum Command {
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

pub(crate) async fn run<H: Host>(app: H, mut rx: mpsc::UnboundedReceiver<Command>) {
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

fn release_held<H: Host>(app: &H, held: &mut HashSet<String>, reason: &str) {
    for id in held.drain() {
        info!("Releasing portal shortcut '{id}' because {reason}");
        app.event(&id, false);
    }
}

async fn run_session<H: Host>(app: &H, rx: &mut mpsc::UnboundedReceiver<Command>) -> End {
    set_state(app, PortalState::Connecting, None);

    let portal = match GlobalShortcuts::new().await {
        Ok(portal) => portal,
        Err(e) => {
            warn!("GlobalShortcuts portal unavailable: {e}");
            set_state(
                app,
                PortalState::Unsupported,
                Some(format!("This desktop does not offer global shortcuts: {e}")),
            );
            return End::Idle;
        }
    };
    // `version` is a property of every portal interface; 1 lacks ConfigureShortcuts.
    let version = portal.get_property::<u32>("version").await.unwrap_or(1);
    app.update(|s| s.shortcuts_version = version);

    // Subscribe before binding so no activation can be missed.
    let (activated, deactivated, changed) = match futures_util::try_join!(
        portal.receive_activated(),
        portal.receive_deactivated(),
        portal.receive_shortcuts_changed()
    ) {
        Ok(streams) => streams,
        Err(e) => {
            set_state(app, PortalState::Failed, Some(e.to_string()));
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
            set_state(app, PortalState::Failed, Some(e.to_string()));
            return End::Idle;
        }
        Err(_) => {
            set_state(
                app,
                PortalState::Failed,
                Some("The portal returned an unexpected session handle".into()),
            );
            return End::Idle;
        }
    };
    let Some(our_path) = session_path(&session) else {
        set_state(app, PortalState::Failed, Some("Invalid session".into()));
        return End::Idle;
    };
    let mut closed = match super::session_closed(&portal, &our_path).await {
        Ok(stream) => Box::pin(stream),
        Err(e) => {
            set_state(app, PortalState::Failed, Some(e));
            let _ = session.close().await;
            return End::Idle;
        }
    };

    let shortcuts = app.shortcuts();
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
            app.update(|s| {
                s.shortcuts = triggers;
                s.shortcuts_state = PortalState::Active;
                s.shortcuts_error = None;
            });
        }
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => {
            info!("User declined the shortcut request");
            set_state(app, PortalState::Denied, None);
            let _ = session.close().await;
            return End::Idle;
        }
        Err(e) => {
            warn!("BindShortcuts failed: {e}");
            set_state(app, PortalState::Failed, Some(e.to_string()));
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
                    set_state(app, PortalState::Closed, None);
                    return End::Idle;
                };
                if event.session_handle() != our_path.as_ref() {
                    debug!("Ignoring activation for another session");
                    continue;
                }
                let id = event.shortcut_id();
                if !app.accepts(id) {
                    continue;
                }
                // Every activation reaches the handler: in toggle mode each one
                // toggles, and some triggers never send Deactivated (KDE's
                // kglobalaccel invokeShortcut). In push-to-talk mode the
                // coordinator ignores presses while already recording. `held`
                // only tracks keys to release if the session closes.
                held.insert(id.to_string());
                app.event(id, true);
            }
            event = deactivated.next() => {
                let Some(event) = event else { continue };
                if event.session_handle() != our_path.as_ref() {
                    continue;
                }
                let id = event.shortcut_id();
                if held.remove(id) {
                    app.event(id, false);
                }
            }
            event = changed.next() => {
                if let Some(event) = event {
                    if event.session_handle() == our_path.as_ref() {
                        let triggers = to_status(event.shortcuts());
                        app.update(|s| s.shortcuts = triggers);
                    }
                }
            }
            _ = closed.next() => {
                info!("Portal shortcut session was closed by the desktop");
                release_held(app, &mut held, "the shortcut session closed");
                app.update(|s| {
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

/// Session lifecycle against `tests/portal-mock/mock_portal.py` on a private
/// session bus. Run with `tests/portal-mock/run.sh`.
#[cfg(test)]
mod mock_portal {
    use super::*;
    use ashpd::zbus;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    struct TestHost {
        events: Arc<Mutex<Vec<(String, bool)>>>,
        status: Arc<Mutex<DesktopIntegrationStatus>>,
    }

    impl Host for TestHost {
        fn shortcuts(&self) -> Vec<NewShortcut> {
            vec![
                NewShortcut::new("transcribe", "Transcribe").preferred_trigger("CTRL+space"),
                NewShortcut::new("transcribe_with_post_process", "Post-process")
                    .preferred_trigger("CTRL+SHIFT+space"),
            ]
        }
        fn accepts(&self, id: &str) -> bool {
            // Post-processing disabled in this test.
            id == "transcribe"
        }
        fn event(&self, id: &str, pressed: bool) {
            self.events.lock().unwrap().push((id.to_string(), pressed));
        }
        fn update(&self, f: impl FnOnce(&mut DesktopIntegrationStatus)) {
            f(&mut self.status.lock().unwrap());
        }
    }

    impl TestHost {
        fn state(&self) -> PortalState {
            self.status.lock().unwrap().shortcuts_state
        }
        fn take_events(&self) -> Vec<(String, bool)> {
            std::mem::take(&mut self.events.lock().unwrap())
        }
        async fn wait_state(&self, want: PortalState) {
            for _ in 0..100 {
                if self.state() == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("state stayed {:?}, wanted {:?}", self.state(), want);
        }
    }

    async fn mock() -> zbus::Proxy<'static> {
        let conn = zbus::Connection::session().await.unwrap();
        zbus::Proxy::new(&conn, "org.footy.Mock", "/org/footy/Mock", "org.footy.Mock")
            .await
            .unwrap()
    }

    async fn gs_sessions(m: &zbus::Proxy<'_>) -> Vec<String> {
        let (json,): (String,) = m.call("Log", &()).await.unwrap();
        let log: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        log.into_iter()
            .filter(|e| e["event"] == "SessionCreated" && e["kind"] == "gs")
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect()
    }

    async fn emit(m: &zbus::Proxy<'_>, kind: &str, which: &str, id: &str) {
        let _: () = m.call("EmitShortcut", &(kind, which, id)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs mock_portal.py on a private session bus"]
    async fn session_lifecycle() {
        let m = mock().await;
        let host = TestHost {
            events: Default::default(),
            status: Arc::new(Mutex::new(DesktopIntegrationStatus::initial())),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _: () = m.call("SetDenyBind", &(true,)).await.unwrap();
        let task = tokio::spawn(run(host.clone(), rx));

        // Denied, then retry after the user changes their mind.
        host.wait_state(PortalState::Denied).await;
        let _: () = m.call("SetDenyBind", &(false,)).await.unwrap();
        tx.send(Command::Retry).unwrap();
        host.wait_state(PortalState::Active).await;
        {
            let status = host.status.lock().unwrap();
            assert_eq!(status.shortcuts_version, 2);
            let triggers: Vec<_> = status
                .shortcuts
                .iter()
                .map(|s| (s.id.as_str(), s.trigger.as_str()))
                .collect();
            assert_eq!(
                triggers,
                [
                    ("transcribe", "Ctrl+space"),
                    ("transcribe_with_post_process", "Ctrl+SHIFT+space")
                ]
            );
        }
        let first = gs_sessions(&m).await.last().cloned().unwrap();

        // Press/release on our session; foreign and gated IDs are ignored.
        emit(&m, "Activated", "ours", "transcribe").await;
        emit(&m, "Deactivated", "ours", "transcribe").await;
        emit(&m, "Activated", "foreign", "transcribe").await;
        emit(&m, "Activated", "ours", "transcribe_with_post_process").await;
        emit(&m, "Deactivated", "ours", "transcribe_with_post_process").await;
        assert_eq!(
            host.take_events(),
            [
                ("transcribe".to_string(), true),
                ("transcribe".to_string(), false)
            ]
        );

        // Toggle-style triggers without Deactivated still reach the handler.
        emit(&m, "Activated", "ours", "transcribe").await;
        emit(&m, "Activated", "ours", "transcribe").await;
        assert_eq!(host.take_events().len(), 2);

        // ConfigureShortcuts on v2.
        let (reply, done) = oneshot::channel();
        tx.send(Command::Configure(reply)).unwrap();
        assert_eq!(done.await.unwrap(), Ok(()));

        // Revocation while held: release is delivered, state is Closed.
        let _: () = m.call("CloseSession", &("gs",)).await.unwrap();
        host.wait_state(PortalState::Closed).await;
        assert_eq!(host.take_events(), [("transcribe".to_string(), false)]);
        emit(&m, "Activated", &first, "transcribe").await;
        assert!(
            host.take_events().is_empty(),
            "nothing accepted while closed"
        );

        // Retry creates a new session; the old session's signals are ignored.
        tx.send(Command::Retry).unwrap();
        host.wait_state(PortalState::Active).await;
        let second = gs_sessions(&m).await.last().cloned().unwrap();
        assert_ne!(first, second);
        emit(&m, "Activated", &first, "transcribe").await;
        assert!(host.take_events().is_empty(), "stale session ignored");
        emit(&m, "Activated", "ours", "transcribe").await;
        emit(&m, "Deactivated", "ours", "transcribe").await;
        assert_eq!(host.take_events().len(), 2);

        // Shutdown closes the session.
        let (done, wait) = oneshot::channel();
        tx.send(Command::Shutdown(done)).unwrap();
        wait.await.unwrap();
        task.await.unwrap();
        let (json,): (String,) = m.call("Log", &()).await.unwrap();
        assert!(
            json.contains(&format!(
                "\"Session.Close\", \"kind\": \"gs\", \"path\": \"{second}\""
            )),
            "session closed on shutdown"
        );
    }
}
