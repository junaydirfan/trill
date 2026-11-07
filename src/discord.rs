//! Discord Rich Presence over Discord's local IPC socket: "Listening to <song>" with the
//! cover, a progress bar and a Spotify link, like Spotify's own integration.
//!
//! A sleeping thread does the socket work; it only wakes when the song, pause state or
//! position actually changes (a few times per song), never on the per-second redraw.

use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::{api::Row, app::App};

pub struct Discord {
    tx: mpsc::Sender<Option<Value>>,
    /// What was last sent: (track uri, playing, start time in unix ms).
    last: Option<(String, bool, u64)>,
}

impl Discord {
    pub fn new(app_id: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new().name("discord".into()).spawn(move || run(&app_id, rx));
        Discord { tx, last: None }
    }

    pub fn sync(&mut self, app: &App) {
        let start = unix_ms().saturating_sub(app.position_ms() as u64);
        let same = match (&self.last, &app.now) {
            (None, None) => true,
            // a jump of more than 2 s means a seek; playback itself keeps `start` steady
            (Some((uri, playing, at)), Some(r)) => {
                *uri == r.uri && *playing == app.playing && (!app.playing || at.abs_diff(start) < 2000)
            }
            _ => false,
        };
        if same {
            return;
        }
        self.last = app.now.as_ref().map(|r| (r.uri.clone(), app.playing, start));
        // Paused or stopped: clear it, as Spotify does.
        let _ = self.tx.send(app.now.as_ref().filter(|_| app.playing).map(|r| activity(r, start)));
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Discord rejects the whole activity if a text field is empty, 1 char or over 128 chars.
fn text(s: &str) -> Option<String> {
    let s: String = s.chars().take(128).collect();
    match s.chars().count() {
        0 => None,
        1 => Some(format!("{s} ")),
        _ => Some(s),
    }
}

fn activity(r: &Row, start: u64) -> Value {
    let mut a = json!({
        "type": 2, // "Listening to"
        "status_display_type": 2, // member list shows the song rather than the app's name
        "details": text(&r.title),
        "state": text(&r.artist),
        "timestamps": { "start": start, "end": start + r.duration_ms as u64 },
    });
    if let Some(url) = &r.art {
        a["assets"] = json!({ "large_image": url, "large_text": text(&r.album) });
    }
    if let Some(id) = r.uri.strip_prefix("spotify:track:") {
        a["buttons"] = json!([{ "label": "Play on Spotify", "url": format!("https://open.spotify.com/track/{id}") }]);
    }
    // send no nulls: an empty artist or album is left out rather than sent empty
    if let Some(o) = a["assets"].as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    if let Some(o) = a.as_object_mut() {
        o.retain(|_, v| !v.is_null()); // also drops the null "assets" the line above can insert
    }
    a
}

fn run(app_id: &str, rx: mpsc::Receiver<Option<Value>>) {
    let mut sock: Option<UnixStream> = None;
    let mut nonce = 0u64;
    while let Ok(mut act) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            act = newer; // only the latest state matters
        }
        // Two tries: the first write after Discord restarts fails on the stale socket.
        for _ in 0..2 {
            if sock.is_none() {
                sock = connect(app_id);
            }
            let Some(s) = sock.as_mut() else { break };
            nonce += 1;
            let msg = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": std::process::id(), "activity": act }, "nonce": nonce.to_string() });
            match send(s, 1, &msg).and_then(|_| recv(s)) {
                Ok(v) => {
                    if v["evt"] == "ERROR" {
                        log::warn!("discord: {}", v["data"]["message"]);
                    }
                    break;
                }
                Err(_) => sock = None,
            }
        }
    }
}

/// Discord's socket, also where the Flatpak and Snap builds put it.
fn connect(app_id: &str) -> Option<UnixStream> {
    let run = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    for dir in [run.clone(), run.join("app/com.discordapp.Discord"), run.join("snap.discord")] {
        for i in 0..10 {
            let Ok(mut s) = UnixStream::connect(dir.join(format!("discord-ipc-{i}"))) else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
            match send(&mut s, 0, &json!({ "v": 1, "client_id": app_id })).and_then(|_| recv(&mut s)) {
                Ok(v) if v["evt"] == "READY" => return Some(s),
                Ok(v) => log::warn!("discord handshake: {v}"),
                Err(e) => log::warn!("discord handshake: {e}"),
            }
        }
    }
    None
}

/// Frame: op (u32 LE), length (u32 LE), JSON.
fn send(s: &mut UnixStream, op: u32, v: &Value) -> io::Result<()> {
    let body = v.to_string();
    let mut buf = Vec::with_capacity(8 + body.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body.as_bytes());
    s.write_all(&buf)
}

fn recv(s: &mut UnixStream) -> io::Result<Value> {
    let mut h = [0u8; 8];
    s.read_exact(&mut h)?;
    let op = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
    if len > 1 << 20 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; len];
    s.read_exact(&mut body)?;
    let v: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    if op == 2 {
        // CLOSE, e.g. an unknown application id
        return Err(io::Error::other(v.to_string()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_fields_stay_within_discord_limits() {
        let r = Row {
            uri: "spotify:track:abc".into(),
            title: "X".into(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 1000,
            kind: crate::api::Kind::Track,
            art: None,
            added_at: 0,
            members: String::new(),
        };
        let a = activity(&r, 5);
        assert_eq!(a["details"], "X ");
        assert!(a.get("state").is_none());
        assert_eq!(a["timestamps"]["end"], 1005);
        assert_eq!(a["buttons"][0]["url"], "https://open.spotify.com/track/abc");
        assert_eq!(text(&"a".repeat(200)).unwrap().len(), 128);
    }
}
