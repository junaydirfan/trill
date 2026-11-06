//! cachify — a lean terminal Spotify player.
//!
//! Design notes (why it's light):
//! - single-threaded tokio runtime for UI + network; librespot's player thread gets
//!   a 1-worker runtime (TOKIO_WORKER_THREADS) instead of one per CPU core
//! - one blocking thread for terminal input (zero CPU while idle)
//! - redraws only on input / player events / 1 Hz while playing; nothing polls Spotify
//! - audio goes straight to PipeWire via the PulseAudio protocol, decoded in-process
//! - streamed audio is buffered on disk (cache dir), not in /tmp (tmpfs = RAM on CachyOS)

mod api;
mod app;
mod auth;
mod config;
mod ui;

use std::{env, fs, sync::Arc, time::Duration};

use crossterm::event::Event;
use librespot_core::{Session, SessionConfig, cache::Cache};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig},
    mixer::{self, MixerConfig},
    player::Player,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::{MissedTickBehavior, interval, timeout},
};

use crate::{app::App, auth::WebToken, config::Config};

fn main() {
    // SAFETY: no other threads exist yet.
    unsafe {
        for (k, v) in [
            ("TOKIO_WORKER_THREADS", "1"),
            ("PULSE_PROP_application.name", "cachify"),
            ("PULSE_PROP_stream.description", "Music"),
            ("PULSE_PROP_media.role", "music"),
            ("PULSE_PROP_application.icon_name", "audio-x-generic"),
        ] {
            if env::var_os(k).is_none() {
                env::set_var(k, v);
            }
        }
    }

    if env::args().any(|a| a == "-h" || a == "--help") {
        println!("cachify — lean terminal Spotify player (Premium required)\n");
        println!("  cachify            start (first run opens a browser to log in)");
        println!("  cachify --logout   forget saved login\n");
        println!("Config: ~/.config/cachify/config   Logs: CACHIFY_LOG=debug cachify");
        return;
    }

    let cfg = Config::load();
    config::init_logger(&cfg.cache_dir);

    if env::args().any(|a| a == "--logout") {
        let _ = fs::remove_file(cfg.cache_dir.join("credentials.json"));
        let _ = fs::remove_file(cfg.cache_dir.join("webapi_refresh_token"));
        println!("Logged out.");
        return;
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(run(cfg));
    std::process::exit(code);
}

async fn run(cfg: Config) -> i32 {
    let audio_dir = (cfg.audio_cache_mb > 0).then(|| cfg.cache_dir.join("audio"));
    let limit = (cfg.audio_cache_mb > 0).then(|| cfg.audio_cache_mb * 1024 * 1024);
    let cache = match Cache::new(Some(&cfg.cache_dir), Some(&cfg.cache_dir), audio_dir.as_ref(), limit) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cache dir {}: {e}", cfg.cache_dir.display());
            return 1;
        }
    };

    // Stream buffers go to disk; clear leftovers from a previous crash.
    let tmp_dir = cfg.cache_dir.join("tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    let _ = fs::create_dir_all(&tmp_dir);
    let session_cfg = SessionConfig { tmp_dir, ..SessionConfig::default() };

    let session = Session::new(session_cfg.clone(), Some(cache.clone()));
    if let Err(e) = auth::connect(&session, &cache).await {
        eprintln!("Login failed: {e}");
        return 1;
    }

    let web = match &cfg.client_id {
        Some(id) => match WebToken::load(id, &cfg.cache_dir).await {
            Ok(t) => Some(Arc::new(Mutex::new(t))),
            Err(e) => {
                eprintln!("Own client_id auth failed ({e}); search will use the fallback.");
                None
            }
        },
        None => None,
    };

    let mixer = match mixer::find(None).map(|m| m(MixerConfig::default())) {
        Some(Ok(m)) => m,
        _ => {
            eprintln!("couldn't create volume mixer");
            return 1;
        }
    };
    mixer.set_volume(cache.volume().unwrap_or(u16::MAX / 2));

    let Some(backend) = audio_backend::find(Some("pulseaudio".into())) else {
        eprintln!("pulseaudio backend missing from build");
        return 1;
    };
    let player_cfg = PlayerConfig {
        bitrate: cfg.bitrate,
        normalisation: cfg.normalise,
        ..PlayerConfig::default()
    };
    let player = Player::new(player_cfg, session.clone(), mixer.get_soft_volume(), move || {
        backend(None, AudioFormat::default())
    });
    let mut player_events = player.get_player_event_channel();

    // Terminal input on one blocking thread: sleeps in read() until a key arrives.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    let _ = std::thread::Builder::new().name("input".into()).spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut term = ratatui::init();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(session, player.clone(), mixer, web, tx);

    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut health = interval(Duration::from_secs(15));
    health.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        if dirty {
            let _ = term.draw(|f| ui::draw(f, &mut app));
            dirty = false;
        }
        if app.quit {
            break;
        }
        tokio::select! {
            Some(ev) = key_rx.recv() => match ev {
                Event::Key(k) => { app.on_key(k); dirty = true; }
                Event::Resize(..) => dirty = true,
                _ => {}
            },
            Some(ev) = player_events.recv() => dirty |= app.on_player(ev),
            Some(msg) = rx.recv() => { app.on_msg(msg); dirty = true; }
            _ = tick.tick(), if app.playing => dirty = true,
            _ = health.tick() => {
                if app.session.is_invalid() {
                    app.status = "Connection lost, reconnecting…".into();
                    let _ = term.draw(|f| ui::draw(f, &mut app));
                    let s = Session::new(session_cfg.clone(), Some(cache.clone()));
                    match timeout(Duration::from_secs(10), auth::reconnect(&s, &cache)).await {
                        Ok(Ok(())) => {
                            player.set_session(s.clone());
                            app.session = s;
                            app.status = "Reconnected.".into();
                        }
                        Ok(Err(e)) => app.status = format!("Reconnect failed: {e} (retrying)"),
                        Err(_) => app.status = "Reconnect timed out (retrying)".into(),
                    }
                    dirty = true;
                }
            }
        }
    }

    player.stop();
    app.session.shutdown();
    ratatui::restore();
    0
}
