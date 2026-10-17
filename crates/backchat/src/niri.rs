//! The niri IPC envelope, the family's hand-rolled client (copied
//! from kuma-platform's niri module: one request line, one reply
//! line, niri's Ok envelope unwrapped). It exists for one job here:
//! telling the titlebar when niri has tiled the window. niri's xdg
//! tiled states are a CSD hint only, so the IPC is the only voice
//! that answers (ADR-0003 in the family chose a hand-rolled client
//! over a crate; events, actions, and response types stay in the
//! apps, the envelope travels).

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// The socket's path: NIRI_SOCKET when the session set it, else the
/// well-known socket in XDG_RUNTIME_DIR.
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("NIRI_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    std::fs::read_dir(&runtime_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("niri.") && name.ends_with(".sock"))
        })
        .context("no niri IPC socket found in XDG_RUNTIME_DIR")
}

/// The same exchange under a read/write timeout: a caller on a
/// polling beat keeps a stuck socket from stalling its loop.
pub fn request_with_timeout(request: &Value, timeout: Duration) -> Result<Value> {
    exchange(request, Some(timeout))
}

fn exchange(request: &Value, timeout: Option<Duration>) -> Result<Value> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;
    if let Some(timeout) = timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }

    let mut writer = &stream;
    writer.write_all(request.to_string().as_bytes())?;
    writer.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    unwrap_envelope(&reply)
}

/// One reply line in, niri's Ok payload out. The `{"Err": ...}`
/// envelope (or anything that is not Ok) fails with niri's own text.
fn unwrap_envelope(reply: &str) -> Result<Value> {
    if !reply.starts_with("{\"Ok\"") {
        bail!("niri request failed: {reply}");
    }
    let envelope: Value =
        serde_json::from_str(reply.trim()).with_context(|| format!("niri reply not JSON: {reply}"))?;
    Ok(envelope["Ok"].clone())
}

/// One niri window, the fields the own-window check needs.
/// `is_floating` stays an Option twice over: a niri older than the
/// field parses as None, and None means unknown, which reads as not
/// tiled (the bar stays).
#[derive(Debug, Deserialize)]
pub struct OwnWindow {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub is_floating: Option<bool>,
}

/// The niri windows this process owns. Err when there is no niri to
/// ask (any other desktop): callers treat that as unknown and leave
/// their bars alone.
///
/// A window is ours when either its pid is ours (the host run) or it
/// wears our app_id (the container run, where niri sees the host pid
/// and cannot match ours; the Wayland `app_id` crosses that
/// boundary).
pub fn own_app_windows(app_id: &str) -> Result<Vec<OwnWindow>> {
    let payload = request_with_timeout(&serde_json::json!("Windows"), Duration::from_secs(2))?;
    let windows: Vec<OwnWindow> = serde_json::from_value(payload["Windows"].clone())
        .context("niri's Windows reply did not parse")?;
    Ok(windows_for(windows, app_id))
}

fn windows_for(windows: Vec<OwnWindow>, app_id: &str) -> Vec<OwnWindow> {
    let pid = std::process::id();
    windows
        .into_iter()
        .filter(|window| window.pid == Some(pid) || window.app_id.as_deref() == Some(app_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ok_envelope_unwraps_to_its_payload() {
        let reply = r#"{"Ok":{"Windows":[{"id":2,"pid":2298}]}}"#;
        let payload = unwrap_envelope(reply).unwrap();
        assert_eq!(payload, json!({"Windows": [{"id": 2, "pid": 2298}]}));
    }

    #[test]
    fn the_err_envelope_fails_with_niris_own_text() {
        let err = unwrap_envelope(r#"{"Err":"some mistake"}"#).unwrap_err();
        assert!(err.to_string().contains("some mistake"), "{err:#}");
        assert!(unwrap_envelope("not json at all").is_err());
        assert!(unwrap_envelope("").is_err());
    }

    #[test]
    fn own_app_windows_keeps_its_own_windows_only() {
        let mine = std::process::id();
        let payload = vec![
            OwnWindow { pid: Some(mine), app_id: None, is_floating: Some(false) },
            OwnWindow { pid: Some(2298), app_id: Some("other".into()), is_floating: Some(true) },
            OwnWindow { pid: None, app_id: Some("backchat".into()), is_floating: Some(true) },
            OwnWindow { pid: Some(2298), app_id: Some("backchat".into()), is_floating: Some(false) },
        ];
        let own = windows_for(payload, "backchat");
        // pid match (the host run) or app_id match (the container run);
        // the pid-2298 window wearing another app's id stays out
        assert_eq!(own.len(), 3);
    }

    #[test]
    fn own_app_windows_reads_a_reply_without_the_floating_field() {
        // an older niri: the field is absent, the parse holds, and the
        // unknown reads as not tiled
        let payload = serde_json::json!({"Windows": [
            {"id": 2, "pid": std::process::id()},
        ]});
        let parsed: Vec<OwnWindow> = serde_json::from_value(payload["Windows"].clone()).unwrap();
        let own = windows_for(parsed, "backchat");
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].is_floating, None);
    }
}
