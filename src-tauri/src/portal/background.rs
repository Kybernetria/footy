//! Background portal (launch at login) and notification portal helpers.

use ashpd::desktop::background::{Background, BackgroundProxy};
use ashpd::desktop::notification::{Notification, NotificationProxy, Priority};
use log::{info, warn};
use tauri::AppHandle;

use super::update_status;

/// Check once whether the Background portal exists so the UI can say whether
/// launch at login is available.
pub fn probe(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let supported = BackgroundProxy::new().await.is_ok();
        info!("Background portal available: {supported}");
        update_status(&app, |s| s.autostart_supported = supported);
    });
}

fn launch_command() -> Vec<String> {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "footy".to_string());
    vec![binary]
}

/// Ask the desktop to (not) start Footy at login. Returns the autostart
/// state the desktop actually granted.
pub async fn request_autostart(enabled: bool) -> Result<bool, String> {
    let response = Background::request()
        .reason("Start Footy when you log in so dictation shortcuts are ready.")
        .auto_start(enabled)
        .command(launch_command())
        .dbus_activatable(false)
        .send()
        .await
        .and_then(|request| request.response());
    match response {
        Ok(granted) => Ok(granted.auto_start()),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
            Err("Launch at login was not allowed.".to_string())
        }
        Err(e) => Err(format!("The desktop could not set up launch at login: {e}")),
    }
}

/// Show a desktop notification; used when a transcript cannot be delivered
/// to the focused application, since Footy's own window may be hidden.
pub fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    tauri::async_runtime::spawn(async move {
        let result = async {
            let proxy = NotificationProxy::new().await?;
            proxy
                .add_notification(
                    "transcript",
                    Notification::new(&title)
                        .body(body.as_str())
                        .priority(Priority::Normal),
                )
                .await
        }
        .await;
        if let Err(e) = result {
            warn!("Failed to show notification: {e}");
        }
    });
}

/// Background portal request against `tests/portal-mock/mock_portal.py`.
/// Run with `tests/portal-mock/run.sh`.
#[cfg(test)]
mod mock_portal {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs mock_portal.py on a private session bus"]
    async fn autostart_request() {
        assert_eq!(request_autostart(true).await, Ok(true));
        assert_eq!(request_autostart(false).await, Ok(false));
        let conn = ashpd::zbus::Connection::session().await.unwrap();
        let mock =
            ashpd::zbus::Proxy::new(&conn, "org.footy.Mock", "/org/footy/Mock", "org.footy.Mock")
                .await
                .unwrap();
        let (json,): (String,) = mock.call("Log", &()).await.unwrap();
        let log: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        let requests: Vec<&serde_json::Value> = log
            .iter()
            .filter(|e| e["event"] == "Background.RequestBackground")
            .collect();
        assert_eq!(requests.len(), 2);
        let options = &requests[0]["args"][1];
        assert_eq!(options["autostart"], true);
        assert_eq!(options["dbus-activatable"], false);
        assert!(options["commandline"][0]
            .as_str()
            .unwrap()
            .starts_with("footy"));
    }
}
