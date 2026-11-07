//! cachify — a lean terminal Spotify player.
//!
//! Design notes (why it's light):
//! - single-threaded tokio runtime for UI + network
//! - one blocking thread for terminal input (zero CPU while idle)
//! - redraws only on input / player events / 1 Hz while playing; nothing polls Spotify
//! - browsing uses librespot's session (batched metadata), not the rate-limited Web API
//! - playback: Spotify's official Soloist daemon (works where librespot keys are refused),
//!   or in-process librespot (1-worker runtime, PulseAudio protocol straight to PipeWire)

mod api;
mod app;
mod audio;
mod auth;
mod config;
mod engine;
mod library;
mod discord;
mod mpris;
mod ui;

use std::{env, fs, process::Command, sync::Arc, time::Duration};

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

use crate::{
    app::App,
    auth::WebToken,
    config::Config,
    engine::{Engine, EngineEvent, LibrespotEvents, SoloistOpts},
};

const SOLOIST_URL: &str = "https://soloist-builds.spotifycdn.com/soloist_release_x86_64.tar.gz";

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

    let has = |flag: &str| env::args().any(|a| a == flag);
    if has("-h") || has("--help") {
        println!("cachify — lean terminal Spotify player (Premium required)\n");
        println!("  cachify                   start (first run opens a browser to log in)");
        println!("  cachify --update-soloist  download/refresh Spotify's Soloist player (every <90 days)");
        println!("  cachify --pair            pair Soloist once via the Spotify app on your phone");
        println!("  cachify --logout          forget saved logins\n");
        println!("Config: ~/.config/cachify/config   Logs: CACHIFY_LOG=debug cachify");
        return;
    }

    let cfg = Config::load();
    config::init_logger(&cfg.cache_dir);

    if has("--logout") {
        let _ = fs::remove_file(cfg.cache_dir.join("credentials.json"));
        let _ = fs::remove_file(cfg.cache_dir.join("webapi_refresh_token"));
        println!("Logged out of cachify. (Soloist pairing kept; re-pair with --pair.)");
        return;
    }
    if has("--update-soloist") {
        std::process::exit(update_soloist(&cfg));
    }
    if has("--pair") {
        std::process::exit(pair_soloist(&cfg));
    }
    // Hidden diagnostics: which image protocol / font size this terminal reports.
    if has("--debug-term") {
        let t0 = std::time::Instant::now();
        match query_terminal_images() {
            Some(p) => eprintln!("{:?} font {:?} in {} ms", p.protocol_type(), p.font_size(), t0.elapsed().as_millis()),
            None => eprintln!("no picker in {} ms", t0.elapsed().as_millis()),
        }
        return;
    }

    // Every helper thread gets its own malloc arena that is never handed back; with a few
    // short-lived DNS/file threads that's megabytes of RSS for nothing. Two arenas is plenty.
    // SAFETY: called before any other threads exist.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // DNS lookups and file I/O run here; parallel requests shouldn't mean a thread each.
        .max_blocking_threads(4)
        .build()
        .expect("tokio runtime");
    // LocalSet: the MPRIS server is single-threaded (!Send), like the rest of the app.
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, run(cfg));
    std::process::exit(code);
}

fn soloist_opts(cfg: &Config) -> Result<SoloistOpts, String> {
    let api_key = cfg.soloist_api_key().ok_or_else(|| {
        format!(
            "No Soloist API key. Create one at https://developer.spotify.com/dashboard (Soloist API Key page)\n\
             and save it to {}  (chmod 600)",
            cfg.config_dir.join("soloist_api_key").display()
        )
    })?;
    let bin = cfg
        .soloist_bin()
        .ok_or("Soloist isn't installed. Run: cachify --update-soloist")?;
    Ok(SoloistOpts {
        bin,
        api_key,
        device_name: cfg.device_name.clone(),
        data_dir: cfg.data_dir.join("soloist-data"),
        cache_dir: cfg.cache_dir.join("soloist"),
        cache_mb: cfg.soloist_cache_mb,
        ws: cfg.soloist_ws.clone(),
        pipewire_device: None,
        initial_volume: None,
    })
}

fn update_soloist(cfg: &Config) -> i32 {
    let dir = cfg.soloist_install_dir();
    let staging = dir.join("staging");
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = fs::create_dir_all(&staging) {
        eprintln!("{}: {e}", staging.display());
        return 1;
    }
    let archive = staging.join("soloist.tar.gz");
    println!("Downloading Soloist from Spotify ({SOLOIST_URL})…");
    let ok = Command::new("curl")
        .args(["-fL", "--progress-bar", "-o"])
        .arg(&archive)
        .arg(SOLOIST_URL)
        .status()
        .is_ok_and(|s| s.success())
        && Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .status()
            .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("Download or extract failed.");
        return 1;
    }
    let found = walk_find(&staging, "soloist");
    let Some(bin) = found else {
        eprintln!("No `soloist` executable in the archive; contents left in {}", staging.display());
        return 1;
    };
    let target = dir.join("soloist");
    if let Err(e) = fs::rename(&bin, &target) {
        eprintln!("install to {}: {e}", target.display());
        return 1;
    }
    // keep bundled licence notices next to the binary, drop the rest
    let _ = fs::remove_dir_all(&staging);
    let _ = Command::new(&target).arg("--version").status();
    println!("Installed {}", target.display());
    0
}

fn walk_find(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for e in fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = walk_find(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

fn pair_soloist(cfg: &Config) -> i32 {
    let opts = match soloist_opts(cfg) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let _ = fs::create_dir_all(&opts.data_dir);
    let _ = fs::create_dir_all(&opts.cache_dir);
    println!("Pairing: open Spotify on your phone (same Wi-Fi), tap the devices icon,");
    println!("and pick “{}”. This window finishes by itself once paired.\n", opts.device_name);
    match opts.base_command().arg("-p").status() {
        Ok(s) if s.success() => {
            println!("\nPaired. Run `cachify`.");
            0
        }
        Ok(s) => {
            eprintln!("Soloist exited with {s}");
            s.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("can't run {}: {e}", opts.bin.display());
            1
        }
    }
}

/// Which image protocol the terminal supports. Terminals answer in a few ms; if one doesn't,
/// ratatui-image gives up but leaves a thread blocked reading stdin, which would eat the
/// first keypresses. A Device Status Report query (which every terminal, and tmux itself,
/// answers) completes that read so the thread exits before we start reading keys.
fn query_terminal_images() -> Option<ratatui_image::picker::Picker> {
    use ratatui_image::picker::{Picker, cap_parser::QueryStdioOptions};
    use std::io::Write;
    const TIMEOUT: Duration = Duration::from_millis(400);
    let t0 = std::time::Instant::now();
    let mut picker = Picker::from_query_stdio_with_options(QueryStdioOptions { timeout: TIMEOUT, ..Default::default() }).ok();
    // Konsole draws iTerm2 inline images (its sixel/kitty support is buggy, so ratatui-image
    // rules those out) but has no env var announcing it, so ratatui-image falls back to
    // half-blocks. Use iTerm2 there (not through tmux, which would need passthrough).
    if let Some(p) = picker.as_mut()
        && env::var_os("KONSOLE_VERSION").is_some()
        && !p.tmux_detected()
        && p.protocol_type() == ratatui_image::picker::ProtocolType::Halfblocks
    {
        p.set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
    }
    // VS Code is guessed to be iTerm2 from TERM_PROGRAM alone, but it only draws images with
    // `terminal.integrated.enableImages` on, and then it also answers the query with sixel.
    // A guess that the query didn't back up would draw nothing, so use half-blocks there.
    if let Some(p) = picker.as_mut()
        && env::var("TERM_PROGRAM").is_ok_and(|t| t == "vscode")
        && p.protocol_type() == ratatui_image::picker::ProtocolType::Iterm2
    {
        p.set_protocol_type(ratatui_image::picker::ProtocolType::Halfblocks);
    }
    if t0.elapsed() >= TIMEOUT {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[5n");
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    picker
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

    let (eng_tx, mut eng_rx) = mpsc::unbounded_channel::<EngineEvent>();
    let saved_vol = cache.volume().map(|v| (v as u32 * 100 + 32767) / 65535).unwrap_or(50);

    let audio_settings = audio::AudioSettings::load(&cfg.data_dir);
    // The EQ only runs when it would change the sound (Flat = no extra process at all).
    let eq = if cfg.eq && !audio_settings.is_flat() {
        match audio::Eq::start(&cfg.cache_dir, &audio_settings.gains()) {
            Ok(eq) => Some(eq),
            Err(e) => {
                log::warn!("EQ disabled: {e}");
                None
            }
        }
    } else {
        None
    };
    let eq_sink = eq.as_ref().map(|_| audio::EQ_NODE.to_string());

    // Soloist doesn't need our librespot session, so start it while we log in (~0.4 s saved).
    let mut soloist_cfg = None;
    if cfg.use_soloist() {
        match soloist_opts(&cfg) {
            Ok(mut o) => {
                o.pipewire_device = eq_sink.clone();
                o.initial_volume = Some(saved_vol);
                soloist_cfg = Some(o);
            }
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    }
    // Ask the terminal which image protocol it speaks (kitty / sixel / iTerm2, else
    // half-block characters) while we log in. Must finish before we read key events.
    let picker_query = (cfg.art && std::io::IsTerminal::is_terminal(&std::io::stdout())).then(|| {
        std::thread::Builder::new()
            .name("term-query".into())
            .spawn(query_terminal_images)
    });
    let session = Session::new(session_cfg.clone(), Some(cache.clone()));
    let cached_user = cache.credentials().and_then(|c| c.username);
    let early = soloist_cfg.clone().zip(cached_user).map(|(o, user)| {
        audio::write_soloist_prefs(&o.data_dir, &user, audio_settings.crossfade_s, cfg.bitrate);
        let tx = eng_tx.clone();
        async move { engine::start_soloist(&o, tx).await }
    });
    let (login, early) = match early {
        Some(start) => {
            eprintln!("Connecting and starting Soloist…");
            let (l, e) = tokio::join!(auth::connect(&session, &cache), start);
            (l, Some(e))
        }
        None => (auth::connect(&session, &cache).await, None),
    };
    if let Err(e) = login {
        if let Some(Ok(e)) = early {
            e.shutdown();
        }
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

    let mut soloist = None;
    let engine = if let Some(opts) = soloist_cfg {
        let started = match early {
            Some(r) => r,
            None => {
                audio::write_soloist_prefs(&opts.data_dir, &session.username(), audio_settings.crossfade_s, cfg.bitrate);
                eprintln!("Starting Soloist…");
                engine::start_soloist(&opts, eng_tx.clone()).await
            }
        };
        let e = match started {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        soloist = Some(opts);
        e
    } else {
        let mixer = match mixer::find(None).map(|m| m(MixerConfig::default())) {
            Some(Ok(m)) => m,
            _ => {
                eprintln!("couldn't create volume mixer");
                return 1;
            }
        };
        mixer.set_volume((saved_vol * u16::MAX as u32 / 100) as u16);
        let Some(backend) = audio_backend::find(Some("pulseaudio".into())) else {
            eprintln!("pulseaudio backend missing from build");
            return 1;
        };
        let player_cfg = PlayerConfig {
            bitrate: cfg.bitrate,
            normalisation: cfg.normalise,
            ..PlayerConfig::default()
        };
        let sink = eq_sink.clone();
        let player = Player::new(player_cfg, session.clone(), mixer.get_soft_volume(), move || {
            backend(sink, AudioFormat::default())
        });
        let mut rx = player.get_player_event_channel();
        let tx = eng_tx.clone();
        tokio::spawn(async move {
            let mut map = LibrespotEvents::default();
            while let Some(ev) = rx.recv().await {
                if let Some(ev) = map.map(ev)
                    && tx.send(ev).is_err()
                {
                    break;
                }
            }
        });
        Engine::Librespot { player, mixer }
    };

    // Hidden diagnostics: `cachify --debug-lib <action> <args…>` exercises library writes.
    if let Some(i) = env::args().position(|a| a == "--debug-lib") {
        let a: Vec<String> = env::args().skip(i + 1).collect();
        let t0 = std::time::Instant::now();
        let res = match a.first().map(String::as_str) {
            Some("like") => library::set_liked(&session, &a[1], true).await,
            Some("unlike") => library::set_liked(&session, &a[1], false).await,
            Some("add") => library::playlist_add(&session, &a[1], &a[2]).await,
            Some("rm") => library::playlist_remove(&session, &a[1], &a[2]).await,
            Some("isliked") => {
                let liked = format!("spotify:user:{}:collection", session.username());
                api::context_tracks(&session, &liked).await.map(|t| {
                    let n = t.iter().filter(|t| a[1..].contains(&t.uri)).count();
                    eprintln!("{} liked; {n} of {} given are liked", t.len(), a.len() - 1);
                })
            }
            Some("pins") => library::pins(&session).await.map(|p| p.iter().for_each(|u| eprintln!("{u}"))),
            Some("lyrics") => library::lyrics(&session, &a[1]).await.map(|l| match l {
                Some(l) => eprintln!("synced={} lines={} provider={}", l.synced, l.lines.len(), l.provider),
                None => eprintln!("no lyrics"),
            }),
            _ => match api::playlists(&session).await {
                Ok(p) => {
                    for (uri, name, editable, pinned) in &p {
                        eprintln!("{} {} {uri} {name}", if *editable { "EDIT" } else { "    " }, if *pinned { "PIN" } else { "   " });
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            },
        };
        eprintln!("{res:?} in {} ms", t0.elapsed().as_millis());
        engine.shutdown();
        return 0;
    }

    // Hidden diagnostics: `cachify --debug-get <spclient endpoint | url>` prints a raw response.
    if let Some(i) = env::args().position(|a| a == "--debug-get") {
        let target = env::args().nth(i + 1).unwrap_or_default();
        let t0 = std::time::Instant::now();
        let res = if target.starts_with("http") {
            session.spclient().request_url(&target).await
        } else {
            session.spclient().request_as_json(&http::Method::GET, &target, None, None).await
        };
        match res {
            Ok(b) => {
                eprintln!("ok in {} ms, {} bytes", t0.elapsed().as_millis(), b.len());
                println!("{}", String::from_utf8_lossy(&b));
            }
            Err(e) => eprintln!("error in {} ms: {e}", t0.elapsed().as_millis()),
        }
        engine.shutdown();
        return 0;
    }

    // Hidden diagnostics: `cachify --debug-context <uri>` prints raw context-resolve track metadata.
    if let Some(i) = env::args().position(|a| a == "--debug-context") {
        let mut uri = env::args()
            .nth(i + 1)
            .unwrap_or_else(|| format!("spotify:user:{}:collection", session.username()));
        if uri == "playlist" {
            let lists = api::playlists(&session).await.unwrap_or_default();
            eprintln!("first playlists: {:?}", lists.iter().take(3).collect::<Vec<_>>());
            uri = lists.first().map(|l| l.0.clone()).unwrap_or_default();
        }
        match session.spclient().get_context(&uri).await {
            Ok(ctx) => {
                eprintln!("context metadata: {:?}", ctx.metadata);
                for (pi, page) in ctx.pages.iter().enumerate().take(3) {
                    eprintln!("page {pi}: {} tracks, page_url={:?} next={:?} meta={:?}", page.tracks.len(), page.page_url(), page.next_page_url(), page.metadata);
                    for t in page.tracks.iter().take(3) {
                        eprintln!("  {} uid={} meta={:?}", t.uri(), t.uid(), t.metadata);
                    }
                    if let Some(t) = page.tracks.last() {
                        eprintln!("  … last: {} meta={:?}", t.uri(), t.metadata);
                    }
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
        engine.shutdown();
        return 0;
    }

    // Hidden diagnostics: `cachify --debug-play [uri]` plays one track headless and prints events.
    if let Some(i) = env::args().position(|a| a == "--debug-play") {
        let uri = match env::args().nth(i + 1) {
            Some(u) => u,
            None => {
                let liked = format!("spotify:user:{}:collection", session.username());
                match api::context_track_uris(&session, &liked).await {
                    Ok(u) if !u.is_empty() => u[0].clone(),
                    other => {
                        eprintln!("couldn't resolve liked songs: {other:?}");
                        return 1;
                    }
                }
            }
        };
        let secs: u64 = env::var("CACHIFY_DEBUG_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
        eprintln!("loading {uri}");
        engine.load(&uri);
        // CACHIFY_DEBUG_SEEK_END=1: jump to 6 s before the end to observe end-of-track events.
        let seek_lead: Option<u32> = env::var("CACHIFY_DEBUG_SEEK_END").ok().map(|v| v.parse().unwrap_or(6000));
        let mut seek_end = seek_lead.is_some();
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        let t0 = std::time::Instant::now();
        while let Ok(Some(ev)) = tokio::time::timeout_at(end, eng_rx.recv()).await {
            eprintln!("[{:>6} ms] event: {ev:?}", t0.elapsed().as_millis());
            if seek_end
                && let EngineEvent::Position { playing: Some(true), .. } = ev
            {
                seek_end = false;
                // duration comes from our own metadata so it works on both engines
                let rows = api::track_rows(&session, vec![uri.clone()]).await;
                if let Some(r) = rows.first() {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let lead = seek_lead.unwrap_or(6000);
                    eprintln!("debug: seeking to {} of {}", r.duration_ms.saturating_sub(lead), r.duration_ms);
                    engine.seek(r.duration_ms.saturating_sub(lead));
                }
            }
        }
        engine.stop();
        tokio::time::sleep(Duration::from_millis(300)).await;
        engine.shutdown();
        return 0;
    }

    // Terminal input on one blocking thread: sleeps in read() until a key arrives.
    let img_picker = picker_query.and_then(|t| t.ok()).and_then(|t| t.join().ok()).flatten();
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
    let mut app = App::new(session, engine, saved_vol, web, tx, audio_settings, eq);
    app.eq_allowed = cfg.eq;
    app.cache_dir = Some(cfg.cache_dir.clone());
    app.art_on = img_picker.is_some();
    app.img_picker = img_picker;
    app.load_playlists();
    app.load_liked();
    let (remote_tx, mut remote_rx) = mpsc::unbounded_channel();
    let mut desktop = mpris::Mpris::new(remote_tx).await;
    let mut presence = cfg.discord_app_id.clone().map(discord::Discord::new);

    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut health = interval(Duration::from_secs(15));
    health.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        if dirty {
            let _ = term.draw(|f| ui::draw(f, &mut app));
            // after drawing: the UI has said how much room the cover gets
            app.refresh_media();
            if let Some(d) = desktop.as_mut() {
                d.sync(&app).await;
            }
            if let Some(p) = presence.as_mut() {
                p.sync(&app);
            }
            dirty = false;
        }
        if app.quit {
            break;
        }
        if app.restart_due() {
            if let Some(opts) = soloist.as_mut() {
                if app.eq_wanted() {
                    match audio::Eq::start(&cfg.cache_dir, &app.audio.gains()) {
                        Ok(eq) => {
                            app.eq = Some(eq);
                            opts.pipewire_device = Some(audio::EQ_NODE.into());
                        }
                        Err(e) => app.status = format!("EQ failed to start: {e}"),
                    }
                }
                let _ = term.draw(|f| ui::draw(f, &mut app));
                restart_soloist(&mut app, opts, &cfg, &eng_tx).await;
            }
            dirty = true;
        }
        let deadline = [app.end_deadline(), app.restart_at, app.lyrics_deadline()].into_iter().flatten().min();
        tokio::select! {
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d.into()).await,
                    None => std::future::pending().await,
                }
            }, if deadline.is_some() => { app.on_end_deadline(); dirty = true; }
            Some(ev) = key_rx.recv() => match ev {
                Event::Key(k) => { app.on_key(k); dirty = true; }
                Event::Resize(..) => dirty = true,
                _ => {}
            },
            Some(ev) = eng_rx.recv() => dirty |= app.on_engine(ev),
            Some(msg) = rx.recv() => { app.on_msg(msg); dirty = true; }
            Some(cmd) = remote_rx.recv() => { app.on_remote(cmd); dirty = true; }
            _ = tick.tick(), if app.playing => dirty = true,
            _ = health.tick() => {
                if app.session.is_invalid() {
                    app.status = "Connection lost, reconnecting…".into();
                    let _ = term.draw(|f| ui::draw(f, &mut app));
                    let s = Session::new(session_cfg.clone(), Some(cache.clone()));
                    match timeout(Duration::from_secs(10), auth::reconnect(&s, &cache)).await {
                        Ok(Ok(())) => {
                            if let Engine::Librespot { player, .. } = &app.engine {
                                player.set_session(s.clone());
                            }
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

    app.engine.stop();
    app.engine.shutdown();
    if let Some(eq) = &app.eq {
        eq.stop();
    }
    app.session.shutdown();
    ratatui::restore();
    0
}

/// Crossfade lives in Soloist's prefs, read at startup: stop it, rewrite prefs, start it
/// again and resume the current track where it was.
async fn restart_soloist(app: &mut App, opts: &mut SoloistOpts, cfg: &Config, eng_tx: &mpsc::UnboundedSender<EngineEvent>) {
    let resume = app.begin_restart();
    if let Some(pid) = app.engine.pid() {
        app.engine.shutdown();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only checks whether the process still exists.
        while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    opts.initial_volume = Some(app.volume_pct());
    audio::write_soloist_prefs(&opts.data_dir, &app.session.username(), app.audio.crossfade_s, cfg.bitrate);
    match engine::start_soloist(opts, eng_tx.clone()).await {
        Ok(e) => app.finish_restart(e, resume),
        Err(e) => app.status = format!("Couldn't restart Soloist: {e}"),
    }
}
