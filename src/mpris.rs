//! MPRIS (D-Bus) integration: the KDE/GNOME media widget, lock screen, media keys and
//! album art; on macOS the same through Now Playing (media keys, Control Center). We publish a snapshot of the app state after each update and only touch
//! D-Bus for fields that actually changed, so this costs nothing while idle.

use tokio::sync::mpsc::UnboundedSender;

use crate::app::{App, Repeat};

/// Commands coming from the desktop (widget buttons, media keys, playerctl…).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only MPRIS sends them
pub enum Remote {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    /// Relative seek in ms.
    Seek(i64),
    /// Absolute position in ms.
    SetPosition(u32),
    Volume(u32),
    Shuffle(bool),
    Loop(Repeat),
}

#[cfg(target_os = "linux")]
pub use dbus::Mpris;
#[cfg(target_os = "macos")]
pub use mac::{Mpris, prepare};

/// Windows: no media-key integration yet; trill runs without it.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub struct Mpris;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Mpris {
    pub async fn new(_tx: UnboundedSender<Remote>) -> Option<Self> {
        None
    }

    pub async fn sync(&mut self, _app: &App) {}
}

#[cfg(target_os = "linux")]
mod dbus {
    use std::{rc::Rc, time::Instant};

    use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};

    use super::{App, Remote, Repeat, UnboundedSender};

    #[derive(Clone, Default, PartialEq)]
    struct Track {
        uri: String,
        title: String,
        artist: String,
        album: String,
        len_ms: u32,
        art: Option<String>,
    }

    pub struct Mpris {
        player: Rc<Player>,
        track: Track,
        status: Option<PlaybackStatus>,
        vol: Option<u32>,
        shuffle: bool,
        repeat: Repeat,
        pos_ms: u32,
        pos_at: Instant,
    }

    fn track_id(uri: &str) -> TrackId {
        // spotify:track:<base62>  ->  /org/trill/track/<base62>
        let id = uri.rsplit(':').next().unwrap_or("");
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return TrackId::NO_TRACK;
        }
        TrackId::try_from(format!("/org/trill/track/{id}")).unwrap_or(TrackId::NO_TRACK)
    }

    fn loop_status(r: Repeat) -> LoopStatus {
        match r {
            Repeat::Off => LoopStatus::None,
            Repeat::All => LoopStatus::Playlist,
            Repeat::One => LoopStatus::Track,
        }
    }

    impl Mpris {
        /// Registers `org.mpris.MediaPlayer2.trill`. Returns None (and the app carries on
        /// without desktop integration) if there's no session bus.
        pub async fn new(tx: UnboundedSender<Remote>) -> Option<Self> {
            let player = Player::builder("trill")
                .identity("trill")
                .desktop_entry("trill")
                .can_play(true)
                .can_pause(true)
                .can_go_next(true)
                .can_go_previous(true)
                .can_seek(true)
                .can_control(true)
                .build()
                .await
                .map_err(|e| log::warn!("MPRIS unavailable: {e}"))
                .ok()?;

            macro_rules! on {
                ($connect:ident, $f:expr) => {{
                    let tx = tx.clone();
                    let f = $f;
                    player.$connect(move |_| {
                        let _ = tx.send(f());
                    });
                }};
            }
            on!(connect_play_pause, || Remote::PlayPause);
            on!(connect_play, || Remote::Play);
            on!(connect_pause, || Remote::Pause);
            on!(connect_stop, || Remote::Stop);
            on!(connect_next, || Remote::Next);
            on!(connect_previous, || Remote::Prev);
            {
                let tx = tx.clone();
                player.connect_seek(move |_, off| {
                    let _ = tx.send(Remote::Seek(off.as_millis()));
                });
            }
            {
                let tx = tx.clone();
                player.connect_set_position(move |_, _id, pos| {
                    let _ = tx.send(Remote::SetPosition(pos.as_millis().max(0) as u32));
                });
            }
            {
                let tx = tx.clone();
                player.connect_set_volume(move |_, v| {
                    let _ = tx.send(Remote::Volume((v.clamp(0.0, 1.0) * 100.0).round() as u32));
                });
            }
            {
                let tx = tx.clone();
                player.connect_set_shuffle(move |_, on| {
                    let _ = tx.send(Remote::Shuffle(on));
                });
            }
            player.connect_set_loop_status(move |_, l| {
                let _ = tx.send(Remote::Loop(match l {
                    LoopStatus::None => Repeat::Off,
                    LoopStatus::Playlist => Repeat::All,
                    LoopStatus::Track => Repeat::One,
                }));
            });

            let player = Rc::new(player);
            tokio::task::spawn_local(player.run());
            Some(Mpris {
                player,
                track: Track::default(),
                status: None,
                vol: None,
                shuffle: false,
                repeat: Repeat::Off,
                pos_ms: 0,
                pos_at: Instant::now(),
            })
        }

        /// Push whatever changed since the last call.
        pub async fn sync(&mut self, app: &App) {
            let p = &self.player;
            let track = app
                .now
                .as_ref()
                .map(|r| Track {
                    uri: r.uri.clone(),
                    title: r.title.clone(),
                    artist: r.artist.clone(),
                    album: r.album.clone(),
                    len_ms: r.duration_ms,
                    art: r.art.clone(),
                })
                .unwrap_or_default();
            let new_track = track.uri != self.track.uri;
            if track != self.track {
                let mut m = Metadata::new();
                m.set_trackid(Some(track_id(&track.uri)));
                if !track.uri.is_empty() {
                    m.set_title(Some(track.title.clone()));
                    m.set_artist(Some(track.artist.split(", ").map(String::from).collect::<Vec<_>>()));
                    m.set_album(Some(track.album.clone()));
                    m.set_length(Some(Time::from_millis(track.len_ms as i64)));
                    m.set_art_url(track.art.clone());
                    if let Some(id) = track.uri.strip_prefix("spotify:track:") {
                        m.set_url(Some(format!("https://open.spotify.com/track/{id}")));
                    }
                }
                let _ = p.set_metadata(m).await;
                self.track = track;
            }

            let status = match (&app.now, app.playing) {
                (None, _) => PlaybackStatus::Stopped,
                (Some(_), true) => PlaybackStatus::Playing,
                (Some(_), false) => PlaybackStatus::Paused,
            };
            if self.status != Some(status) {
                let _ = p.set_playback_status(status).await;
                self.status = Some(status);
            }

            let vol = app.volume_pct();
            if self.vol != Some(vol) {
                let _ = p.set_volume(vol as f64 / 100.0).await;
                self.vol = Some(vol);
            }
            if self.shuffle != app.shuffle {
                let _ = p.set_shuffle(app.shuffle).await;
                self.shuffle = app.shuffle;
            }
            if self.repeat != app.repeat {
                let _ = p.set_loop_status(loop_status(app.repeat)).await;
                self.repeat = app.repeat;
            }

            // Clients read Position on demand and extrapolate while Playing; tell them about
            // jumps (seeks, track restarts) with the Seeked signal.
            let pos = app.position_ms();
            p.set_position(Time::from_millis(pos as i64));
            let expected = if status == PlaybackStatus::Playing {
                self.pos_ms as i64 + self.pos_at.elapsed().as_millis() as i64
            } else {
                self.pos_ms as i64
            };
            if !new_track && (pos as i64 - expected).abs() > 1500 {
                let _ = p.seeked(Time::from_millis(pos as i64)).await;
            }
            self.pos_ms = pos;
            self.pos_at = Instant::now();
        }
    }
}

/// macOS Now Playing: media keys, Control Center and the lock screen. macOS only hands
/// media keys to a process with an NSApplication ([`prepare`]); the commands arrive on the
/// run loop of the thread that registered for them, which sleeps in it until then.
#[cfg(target_os = "macos")]
mod mac {
    use std::{ptr::NonNull, time::{Duration, Instant}};

    use block2::RcBlock;
    use objc2::{
        msg_send,
        rc::autoreleasepool,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSMutableDictionary, NSNumber, NSRunLoop, NSString};
    use objc2_media_player::{
        MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
        MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
        MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState,
        MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
    };

    use super::{App, Remote, UnboundedSender};

    // NSApplication is in AppKit, which nothing else links.
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    /// Make trill an NSApplication, on the main thread (AppKit wants it there) before any
    /// other threads start. Skipped over SSH, where there's no desktop to talk to.
    pub fn prepare() {
        if std::env::var_os("SSH_CONNECTION").is_some() {
            return;
        }
        // SAFETY: main thread; plain AppKit calls.
        unsafe {
            if let Some(cls) = AnyClass::get(c"NSApplication") {
                let app: *mut AnyObject = msg_send![cls, sharedApplication];
                // background-only: no Dock icon, no menu bar (NSApplicationActivationPolicyProhibited)
                let _: bool = msg_send![app, setActivationPolicy: 2isize];
            }
        }
    }

    pub struct Mpris {
        uri: String,
        playing: bool,
        pos_ms: u32,
        pos_at: Instant,
    }

    impl Mpris {
        pub async fn new(tx: UnboundedSender<Remote>) -> Option<Self> {
            std::thread::Builder::new()
                .name("now-playing".into())
                .spawn(move || listen(tx))
                .map_err(|e| log::warn!("Now Playing unavailable: {e}"))
                .ok()?;
            Some(Mpris { uri: String::new(), playing: false, pos_ms: 0, pos_at: Instant::now() })
        }

        /// Publish when the song, play/pause or position (a seek) changed; macOS moves
        /// the position along by itself in between.
        pub async fn sync(&mut self, app: &App) {
            let uri = app.now.as_ref().map_or("", |r| r.uri.as_str());
            let pos = app.position_ms();
            let expected = self.pos_ms + if self.playing { self.pos_at.elapsed().as_millis() as u32 } else { 0 };
            if uri == self.uri && app.playing == self.playing && pos.abs_diff(expected) < 1500 {
                return;
            }
            (self.playing, self.pos_ms, self.pos_at) = (app.playing, pos, Instant::now());
            self.uri = uri.to_string();
            // SAFETY: plain calls into MediaPlayer, which may be used from any thread.
            autoreleasepool(|_| unsafe { publish(app, pos) });
        }
    }

    unsafe fn publish(app: &App, pos_ms: u32) {
        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        let Some(r) = app.now.as_ref() else {
            unsafe {
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            }
            return;
        };
        // shortcut: no cover in Now Playing; add an MPMediaItemArtwork from the art bytes if it's missed.
        let info = NSMutableDictionary::<NSString, AnyObject>::new();
        let text = |k: &NSString, v: &str| info.insert(k, NSString::from_str(v).as_ref());
        let num = |k: &NSString, v: f64| info.insert(k, NSNumber::new_f64(v).as_ref());
        unsafe {
            text(MPMediaItemPropertyTitle, &r.title);
            text(MPMediaItemPropertyArtist, &r.artist);
            text(MPMediaItemPropertyAlbumTitle, &r.album);
            num(MPMediaItemPropertyPlaybackDuration, r.duration_ms as f64 / 1000.0);
            num(MPNowPlayingInfoPropertyElapsedPlaybackTime, pos_ms as f64 / 1000.0);
            num(MPNowPlayingInfoPropertyPlaybackRate, if app.playing { 1.0 } else { 0.0 });
            center.setNowPlayingInfo(Some(&info));
            center.setPlaybackState(if app.playing { MPNowPlayingPlaybackState::Playing } else { MPNowPlayingPlaybackState::Paused });
        }
    }

    /// Runs on its own thread for the life of the app.
    fn listen(tx: UnboundedSender<Remote>) {
        autoreleasepool(|_| unsafe {
            let cc = MPRemoteCommandCenter::sharedCommandCenter();
            let on = |cmd: &MPRemoteCommand, f: fn(&MPRemoteCommandEvent) -> Remote| {
                let tx = tx.clone();
                let block = RcBlock::new(move |e: NonNull<MPRemoteCommandEvent>| {
                    let _ = tx.send(f(e.as_ref()));
                    MPRemoteCommandHandlerStatus::Success
                });
                cmd.setEnabled(true);
                cmd.addTargetWithHandler(&block);
            };
            on(&cc.togglePlayPauseCommand(), |_| Remote::PlayPause);
            on(&cc.playCommand(), |_| Remote::Play);
            on(&cc.pauseCommand(), |_| Remote::Pause);
            on(&cc.stopCommand(), |_| Remote::Stop);
            on(&cc.nextTrackCommand(), |_| Remote::Next);
            on(&cc.previousTrackCommand(), |_| Remote::Prev);
            on(&cc.changePlaybackPositionCommand(), |e| {
                let e = &*(e as *const MPRemoteCommandEvent as *const MPChangePlaybackPositionCommandEvent);
                Remote::SetPosition((e.positionTime().max(0.0) * 1000.0) as u32)
            });
        });
        let rl = NSRunLoop::currentRunLoop();
        loop {
            // Sleeps until a command arrives. Returns false at once if the run loop has no
            // sources (macOS gave us none): then look in now and then instead of spinning.
            let waited = autoreleasepool(|_| unsafe { rl.runMode_beforeDate(NSDefaultRunLoopMode, &NSDate::distantFuture()) });
            if !waited {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}
