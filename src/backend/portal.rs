//! Capture through `xdg-desktop-portal` over D-Bus.
//!
//! First in the chain: on GNOME and KDE under Wayland the compositor refuses
//! the direct protocols outright, so the portal is not merely preferred, it is
//! the only door. It is also the only backend that can offer the desktop's own
//! area/window picker.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use super::{Backend, Caps, Capture, Error, Probe, Request};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";

pub struct Portal {
    conn: Connection,
}

pub fn probe() -> Probe {
    let conn = match Connection::session() {
        Ok(c) => c,
        Err(e) => return Probe::unavailable(format!("no session bus: {e}")),
    };

    // Ask the bus whether anything actually implements the portal. Activatable
    // names count: the service may not be running yet but will start on demand.
    let dbus: Proxy = match Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    ) {
        Ok(p) => p,
        Err(e) => return Probe::unavailable(format!("cannot reach the bus daemon: {e}")),
    };

    let present = |method: &str| -> bool {
        dbus.call_method(method, &())
            .ok()
            .and_then(|m| m.body().deserialize::<Vec<String>>().ok())
            .is_some_and(|names| names.iter().any(|n| n == DESTINATION))
    };
    if !present("ListNames") && !present("ListActivatableNames") {
        return Probe::unavailable("xdg-desktop-portal is not running on the session bus");
    }

    Probe::Ready(Box::new(Portal { conn }))
}

impl Backend for Portal {
    fn name(&self) -> &'static str {
        "portal"
    }

    fn caps(&self) -> Caps {
        Caps { interactive: true }
    }

    fn describe(&self) -> String {
        format!("xdg-desktop-portal on the session bus ({DESTINATION})")
    }

    fn capture(&self, req: &Request) -> Result<Capture, Error> {
        let uri = self.screenshot(req.interactive)?;
        let path = uri
            .strip_prefix("file://")
            .map(percent_decode)
            .ok_or_else(|| Error::Other(format!("portal returned a non-file URI: {uri}")))?;
        Ok(Capture::EncodedFile(path.into()))
    }
}

impl Portal {
    /// Returns the URI of the image the portal wrote.
    fn screenshot(&self, interactive: bool) -> Result<String, Error> {
        // Results arrive on a Request object whose path the portal derives from
        // our bus name and handle_token. Subscribing before issuing the call is
        // what keeps a fast reply from racing past us.
        let token = format!("rustshot_{}", std::process::id());
        let sender = self
            .conn
            .unique_name()
            .ok_or_else(|| Error::Other("connection has no unique bus name".into()))?
            .trim_start_matches(':')
            .replace('.', "_");
        let request_path = ObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{token}"
        ))
        .map_err(|e| Error::Other(e.to_string()))?;

        let request: Proxy = Proxy::new(
            &self.conn,
            DESTINATION,
            request_path,
            "org.freedesktop.portal.Request",
        )
        .map_err(|e| Error::Other(e.to_string()))?;
        let mut responses = request
            .receive_signal("Response")
            .map_err(|e| Error::Other(e.to_string()))?;

        let screenshot: Proxy = Proxy::new(
            &self.conn,
            DESTINATION,
            OBJECT,
            "org.freedesktop.portal.Screenshot",
        )
        .map_err(|e| Error::Other(e.to_string()))?;
        let options: HashMap<&str, Value> = HashMap::from([
            ("handle_token", Value::from(token.as_str())),
            ("interactive", Value::from(interactive)),
        ]);
        screenshot
            .call_method("Screenshot", &("", options))
            .map_err(|e| Error::Other(format!("Screenshot call failed: {e}")))?;

        let response = responses
            .next()
            .ok_or_else(|| Error::Other("bus closed while awaiting the portal".into()))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = response
            .body()
            .deserialize()
            .map_err(|e| Error::Other(e.to_string()))?;
        match code {
            0 => {}
            1 => return Err(Error::Cancelled),
            n => return Err(Error::Other(format!("portal request failed (code {n})"))),
        }

        let uri = results
            .get("uri")
            .ok_or_else(|| Error::Other("portal response carried no uri".into()))?;
        uri.try_clone()
            .map_err(|e| Error::Other(e.to_string()))
            .and_then(|v| String::try_from(v).map_err(|e| Error::Other(e.to_string())))
    }
}

/// Minimal percent-decoding for `file://` URIs (`%20` → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
