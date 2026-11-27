//! Minimal `org.freedesktop.portal.Request` helper for calls whose results
//! ashpd does not expose (RemoteDesktop.Start's `clipboard_enabled`).

use ashpd::zbus::{self, zvariant};
use futures_util::StreamExt;
use std::collections::HashMap;
use zvariant::{OwnedObjectPath, OwnedValue, Value};

pub const RESPONSE_SUCCESS: u32 = 0;
pub const RESPONSE_CANCELLED: u32 = 1;

pub struct PortalResponse {
    pub code: u32,
    pub results: HashMap<String, OwnedValue>,
}

impl PortalResponse {
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.results
            .get(key)
            .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
    }

    pub fn u32(&self, key: &str) -> Option<u32> {
        self.results
            .get(key)
            .and_then(|v| u32::try_from(v.try_clone().ok()?).ok())
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.results
            .get(key)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
    }
}

/// Request object path the portal will use for `token` on this connection.
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

pub fn new_token() -> String {
    format!("footy_{}", uuid::Uuid::new_v4().simple())
}

/// Call `method` on `proxy` with `(leading..., options)` where `options`
/// already contains `handle_token = token`. Subscribes to the Response
/// signal before calling so a fast reply cannot be missed, and checks the
/// returned handle matches the subscribed path.
pub async fn call_with_response<B>(
    proxy: &zbus::Proxy<'_>,
    method: &str,
    token: &str,
    body: &B,
) -> Result<PortalResponse, String>
where
    B: serde::Serialize + zvariant::DynamicType,
{
    let conn = proxy.connection();
    let unique = conn
        .unique_name()
        .ok_or("D-Bus connection has no unique name")?
        .to_string();
    let path = request_path(&unique, token);
    let request = zbus::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(|e| format!("Failed to watch portal request: {e}"))?;
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(|e| format!("Failed to subscribe to portal response: {e}"))?;

    let reply = proxy
        .call_method(method, body)
        .await
        .map_err(|e| format!("{method} failed: {e}"))?;
    let handle: OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|e| format!("{method} returned an invalid handle: {e}"))?;
    if handle.as_str() != path {
        return Err(format!(
            "{method} returned unexpected request handle {}",
            handle.as_str()
        ));
    }

    let message = responses
        .next()
        .await
        .ok_or_else(|| format!("{method}: portal closed without a response"))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|e| format!("{method}: invalid response: {e}"))?;
    Ok(PortalResponse { code, results })
}

pub fn options_with_token(token: &str) -> HashMap<&'static str, Value<'_>> {
    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(token));
    options
}

#[cfg(test)]
mod tests {
    use super::request_path;

    #[test]
    fn request_path_follows_portal_convention() {
        assert_eq!(
            request_path(":1.42", "footy_abc"),
            "/org/freedesktop/portal/desktop/request/1_42/footy_abc"
        );
    }
}
