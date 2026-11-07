//! Playback engines.
//!
//! - **Soloist**: Spotify's official headless player, run as a child process and driven
//!   over its local WebSocket API. Spotify handles decryption, so it works on accounts where
//!   librespot's audio keys are refused (librespot issue #1649).
//! - **Librespot**: in-process decoding. Lighter, but only works while Spotify still hands
//!   librespot audio keys for your account.
//!
//! Track ordering, shuffle and the play queue stay in cachify for both engines.

use std::{
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use futures_util::{SinkExt, StreamExt};
use librespot_core::SpotifyUri;
use librespot_playback::{
    mixer::Mixer,
    player::{Player, PlayerEvent},
};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_tungstenite::tungstenite::Message;

use crate::api::{Kind, Row};

#[derive(Debug)]
pub enum EngineEvent {
    /// Position anchor; `playing: None` keeps the current play/pause state.
    Position { pos_ms: u32, playing: Option<bool> },
    Stopped,
    Ended,
    Unavailable,
    PreloadNext,
    /// Soloist reports the item now loaded (with its own metadata).
    TrackChanged(Row),
    /// Soloist went idle (nothing loaded / context finished).
    Idle,
    Volume(u32),
    Status(String),
    Exited(String),
}

pub enum Engine {
    Librespot { player: Arc<Player>, mixer: Arc<dyn Mixer> },
    Soloist { tx: UnboundedSender<String>, pid: Option<u32> },
}

impl Engine {
    /// Soloist's process id (None for librespot or an external daemon).
    pub fn pid(&self) -> Option<u32> {
        match self {
            Engine::Soloist { pid, .. } => *pid,
            Engine::Librespot { .. } => None,
        }
    }
}

fn cmd(command: &str) -> Value {
    json!({ "type": "command", "command": command })
}

impl Engine {
    pub fn is_soloist(&self) -> bool {
        matches!(self, Engine::Soloist { .. })
    }

    fn send(&self, v: Value) {
        if let Engine::Soloist { tx, .. } = self {
            let _ = tx.send(v.to_string());
        }
    }

    pub fn load(&self, uri: &str) {
        match self {
            Engine::Librespot { player, .. } => {
                if let Ok(u) = SpotifyUri::from_uri(uri) {
                    player.load(u, true, 0);
                }
            }
            Engine::Soloist { .. } => {
                let mut v = cmd("play");
                v["uri"] = uri.into();
                self.send(v);
            }
        }
    }

    pub fn preload(&self, uri: &str) {
        if let Engine::Librespot { player, .. } = self
            && let Ok(u) = SpotifyUri::from_uri(uri)
        {
            player.preload(u);
        }
    }

    pub fn resume(&self) {
        match self {
            Engine::Librespot { player, .. } => player.play(),
            Engine::Soloist { .. } => self.send(cmd("play")),
        }
    }

    pub fn pause(&self) {
        match self {
            Engine::Librespot { player, .. } => player.pause(),
            Engine::Soloist { .. } => self.send(cmd("pause")),
        }
    }

    pub fn stop(&self) {
        match self {
            Engine::Librespot { player, .. } => player.stop(),
            Engine::Soloist { .. } => self.send(cmd("pause")),
        }
    }

    pub fn seek(&self, ms: u32) {
        match self {
            Engine::Librespot { player, .. } => player.seek(ms),
            Engine::Soloist { .. } => {
                let mut v = cmd("seek");
                v["position_ms"] = ms.into();
                self.send(v);
            }
        }
    }

    pub fn set_volume(&self, pct: u32) {
        let pct = pct.min(100);
        match self {
            Engine::Librespot { mixer, .. } => mixer.set_volume((pct * u16::MAX as u32 / 100) as u16),
            Engine::Soloist { .. } => {
                let mut v = cmd("set_volume");
                v["volume"] = pct.into();
                self.send(v);
            }
        }
    }

    pub fn add_to_queue(&self, uri: &str) {
        let mut v = cmd("add_to_queue");
        v["uri"] = uri.into();
        self.send(v);
    }

    pub fn skip_next(&self) {
        self.send(cmd("skip_next"));
    }

    /// Soloist loops the track natively for repeat-one (no reload gap).
    pub fn set_repeat_track(&self, on: bool) {
        if self.is_soloist() {
            let mut v = cmd("set_repeat_track");
            v["enabled"] = on.into();
            self.send(v);
        }
    }

    pub fn shutdown(&self) {
        match self {
            Engine::Librespot { player, .. } => player.stop(),
            Engine::Soloist { pid, .. } => {
                if let Some(pid) = pid {
                    // SAFETY: plain signal to our own child.
                    unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
                }
            }
        }
    }
}

/// Maps librespot player events, dropping stale ones from superseded load requests.
#[derive(Default)]
pub struct LibrespotEvents {
    req_id: Option<u64>,
}

impl LibrespotEvents {
    pub fn map(&mut self, ev: PlayerEvent) -> Option<EngineEvent> {
        use PlayerEvent::*;
        let ours = |id: u64| self.req_id.is_none_or(|r| r == id);
        Some(match ev {
            PlayRequestIdChanged { play_request_id } => {
                self.req_id = Some(play_request_id);
                return None;
            }
            Playing { play_request_id, position_ms, .. } if ours(play_request_id) => {
                EngineEvent::Position { pos_ms: position_ms, playing: Some(true) }
            }
            Paused { play_request_id, position_ms, .. } if ours(play_request_id) => {
                EngineEvent::Position { pos_ms: position_ms, playing: Some(false) }
            }
            Seeked { play_request_id, position_ms, .. } | PositionCorrection { play_request_id, position_ms, .. }
                if ours(play_request_id) =>
            {
                EngineEvent::Position { pos_ms: position_ms, playing: None }
            }
            Stopped { play_request_id, .. } if ours(play_request_id) => EngineEvent::Stopped,
            TimeToPreloadNextTrack { play_request_id, .. } if ours(play_request_id) => EngineEvent::PreloadNext,
            EndOfTrack { play_request_id, .. } if ours(play_request_id) => EngineEvent::Ended,
            Unavailable { play_request_id, .. } if ours(play_request_id) => EngineEvent::Unavailable,
            _ => return None,
        })
    }
}

pub struct SoloistOpts {
    pub bin: PathBuf,
    pub api_key: String,
    pub device_name: String,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_mb: u64,
    /// Connect to an already-running daemon instead of spawning one.
    pub ws: Option<String>,
    /// PipeWire node to play into (the EQ), instead of the default output.
    pub pipewire_device: Option<String>,
    pub initial_volume: Option<u32>,
}

impl SoloistOpts {
    pub fn base_command(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(["-n", &self.device_name, "-k", &self.api_key])
            .arg("-D")
            .arg(&self.data_dir)
            .arg("-C")
            .arg(&self.cache_dir);
        if self.cache_mb > 0 {
            c.args(["-z", &self.cache_mb.max(100).to_string()]);
        }
        if let Some(d) = &self.pipewire_device {
            c.args(["-d", d]);
        }
        if let Some(v) = self.initial_volume {
            c.args(["-i", &v.min(100).to_string()]);
        }
        if std::env::var_os("CACHIFY_SOLOIST_VERBOSE").is_some() {
            c.arg("-v");
        }
        c
    }
}

fn exit_message(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(s) if s.code() == Some(10) => {
            "Soloist build expired (Spotify builds last 90 days). Quit and run: cachify --update-soloist".into()
        }
        Ok(s) if s.signal() == Some(libc::SIGTERM) => "Soloist stopped.".into(),
        Ok(s) => format!("Soloist exited ({s}); see ~/.cache/cachify/soloist.log"),
        Err(e) => format!("Soloist: {e}"),
    }
}

/// Spawn Soloist (unless `ws` is set), connect to its WebSocket and start pumping events.
pub async fn start_soloist(opts: &SoloistOpts, ev_tx: UnboundedSender<EngineEvent>) -> Result<Engine, String> {
    let (addr, pid) = match &opts.ws {
        Some(a) => (a.clone(), None),
        None => {
            let _ = fs::create_dir_all(&opts.data_dir);
            let _ = fs::create_dir_all(&opts.cache_dir);
            let port_file = opts.data_dir.join("ws.port");
            let addr_file = opts.data_dir.join("ws.addr");
            let _ = fs::remove_file(&port_file);
            let _ = fs::remove_file(&addr_file);

            let log = fs::File::create(opts.cache_dir.join("../soloist.log")).map_err(|e| e.to_string())?;
            let mut c = opts.base_command();
            c.args(["-w", "127.0.0.1:0"])
                .stdin(Stdio::null())
                .stdout(log.try_clone().map_err(|e| e.to_string())?)
                .stderr(log);
            // SAFETY: prctl is async-signal-safe; makes Soloist exit if cachify dies.
            unsafe {
                c.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let mut child = c
                .spawn()
                .map_err(|e| format!("can't start {}: {e}", opts.bin.display()))?;
            let pid = child.id();
            let tx = ev_tx.clone();
            let _ = std::thread::Builder::new().name("soloist-wait".into()).spawn(move || {
                let _ = tx.send(EngineEvent::Exited(exit_message(child.wait())));
            });

            let deadline = Instant::now() + Duration::from_secs(20);
            let port = loop {
                if let Some(p) = fs::read_to_string(&port_file).ok().and_then(|s| s.trim().parse::<u16>().ok()) {
                    break p;
                }
                if Instant::now() > deadline {
                    return Err("Soloist didn't open its WebSocket in 20s; see ~/.cache/cachify/soloist.log".into());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            let host = fs::read_to_string(&addr_file)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "127.0.0.1".into());
            let addr = if host.contains(':') && host.rsplit(':').next().is_some_and(|p| p.parse::<u16>().is_ok()) {
                host
            } else {
                format!("{host}:{port}")
            };
            (addr, Some(pid))
        }
    };

    let url = format!("ws://{addr}");
    let mut tries = 0;
    let ws = loop {
        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => break ws,
            Err(e) if tries >= 30 => return Err(format!("can't connect to Soloist at {url}: {e}")),
            Err(_) => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = unbounded_channel::<String>();

    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(Message::text(m)).await.is_err() {
                break;
            }
        }
    });

    // Soloist reports logged_in=false for a moment while it restores its session, so only
    // warn if it's still not signed in after a few seconds.
    let logged_in = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (logged_in, ev_tx) = (logged_in.clone(), ev_tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            if !logged_in.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = ev_tx.send(EngineEvent::Status(
                    "Soloist isn't signed in. Quit (q) and run: cachify --pair".into(),
                ));
            }
        });
    }

    let ready = logged_in.clone();
    tokio::spawn(async move {
        let logged_in = ready;
        let mut anchor = Anchor::default();
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(t) = msg
                && let Ok(v) = serde_json::from_str::<Value>(t.as_str())
            {
                if v["type"] == "auth_state" && v["logged_in"] == true {
                    logged_in.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                for ev in anchor.parse(&v) {
                    if ev_tx.send(ev).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = ev_tx.send(EngineEvent::Exited("Lost connection to Soloist.".into()));
    });

    // Don't hand out the engine until Soloist has restored its session: commands sent
    // before that are rejected ("command requires authentication").
    let deadline = Instant::now() + Duration::from_secs(10);
    while !logged_in.load(std::sync::atomic::Ordering::Relaxed) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(Engine::Soloist { tx, pid })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Tracks Soloist's position anchor so we can report "position now".
#[derive(Default)]
struct Anchor {
    pos_ms: i64,
    ts_ms: i64,
    speed: f64,
}

impl Anchor {
    fn current(&self) -> u32 {
        let elapsed = (now_ms() - self.ts_ms).max(0) as f64 * self.speed;
        (self.pos_ms as f64 + elapsed).max(0.0) as u32
    }

    fn set(&mut self, p: &Value) {
        self.pos_ms = p["position_ms"].as_i64().unwrap_or(0);
        self.ts_ms = p["timestamp_ms"].as_i64().unwrap_or_else(now_ms);
        self.speed = p["speed"].as_f64().unwrap_or(0.0);
    }

    fn status(&self, s: &str, out: &mut Vec<EngineEvent>) {
        match s {
            "playing" => out.push(EngineEvent::Position { pos_ms: self.current(), playing: Some(true) }),
            "paused" => out.push(EngineEvent::Position { pos_ms: self.current(), playing: Some(false) }),
            "idle" => out.push(EngineEvent::Idle),
            _ => {} // buffering
        }
    }

    fn parse(&mut self, v: &Value) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or("") {
            "auth_state" => {}
            "playback_state" => {
                if let Some(row) = entity_row(&v["item"]) {
                    out.push(EngineEvent::TrackChanged(row));
                }
                self.set(&v["position"]);
                if let Some(vol) = v["volume"].as_u64() {
                    out.push(EngineEvent::Volume(vol as u32));
                }
                self.status(v["status"].as_str().unwrap_or(""), &mut out);
            }
            "track_changed" => {
                if let Some(row) = entity_row(&v["item"]) {
                    out.push(EngineEvent::TrackChanged(row));
                }
            }
            "playback_changed" => self.status(v["status"].as_str().unwrap_or(""), &mut out),
            "position_sync" => {
                self.set(&v["position"]);
                out.push(EngineEvent::Position { pos_ms: self.current(), playing: Some(self.speed > 0.0) });
            }
            "volume_changed" => {
                if let Some(vol) = v["volume"].as_u64() {
                    out.push(EngineEvent::Volume(vol as u32));
                }
            }
            "error" => out.push(EngineEvent::Status(format!(
                "Soloist: {}",
                v["message"].as_str().unwrap_or("error")
            ))),
            _ => {}
        }
        out
    }
}

fn entity_row(item: &Value) -> Option<Row> {
    let uri = item["uri"].as_str().filter(|u| !u.is_empty())?;
    let d = &item["decorations"];
    let artist = d["creators"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["entity"]["decorations"]["identity"]["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some(Row {
        uri: uri.to_string(),
        title: d["identity"]["name"].as_str().unwrap_or("").to_string(),
        artist,
        album: d["parent"]["entity"]["decorations"]["identity"]["name"].as_str().unwrap_or("").to_string(),
        duration_ms: d["playback"]["duration_ms"].as_u64().unwrap_or(0) as u32,
        kind: Kind::Track,
        art: cover(&d["visual_identity"]["cover"]),
        added_at: 0,
    })
}

fn cover(v: &Value) -> Option<String> {
    let covers = v.as_array()?;
    ["large", "default", "xlarge", "small"]
        .iter()
        .find_map(|size| covers.iter().find(|c| c["size"] == *size))
        .and_then(|c| c["url"].as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_soloist_events() {
        let mut a = Anchor::default();
        let st: Value = serde_json::from_str(r#"{"type":"playback_state","status":"paused",
          "item":{"uri":"spotify:track:x","entity_type":"track","decorations":{"identity":{"name":"Song"},
            "creators":[{"entity":{"decorations":{"identity":{"name":"Art"}}}}],
            "parent":{"entity":{"decorations":{"identity":{"name":"Alb"}}}},
            "playback":{"duration_ms":210000}}},
          "position":{"position_ms":45000,"timestamp_ms":1,"speed":0.0},"volume":65}"#).unwrap();
        let evs = a.parse(&st);
        assert!(matches!(&evs[0], EngineEvent::TrackChanged(r) if r.title == "Song" && r.artist == "Art" && r.album == "Alb" && r.duration_ms == 210000));
        assert!(matches!(evs[1], EngineEvent::Volume(65)));
        assert!(matches!(evs[2], EngineEvent::Position { pos_ms: 45000, playing: Some(false) }));

        let idle: Value = serde_json::from_str(r#"{"type":"playback_changed","status":"idle"}"#).unwrap();
        assert!(matches!(a.parse(&idle)[0], EngineEvent::Idle));
        let empty: Value = serde_json::from_str(r#"{"type":"track_changed","item":{"uri":""}}"#).unwrap();
        assert!(a.parse(&empty).is_empty());
    }
}
