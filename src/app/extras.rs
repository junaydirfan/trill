//! Liking songs, playlist edits, lyrics and album art.
//!
//! Everything here is lazy: lyrics are only fetched while the lyrics view is open, art only
//! when there's room for it, and each is fetched once per track. Network and image decoding
//! never run on the UI path; results come back as `Msg`s.

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Size;
use ratatui_image::{FilterType, Resize, picker::Picker, protocol::Protocol};

use super::{App, Focus, Msg, SideKind};
use crate::{
    api::{self, Kind, Row},
    discover::{self, FOR_YOU},
    library::{self, Lyrics},
    theme,
};

pub enum LyricsState {
    Loading,
    Missing,
    Failed(String),
    Ready(Lyrics),
}

pub struct LyricsSlot {
    pub uri: String,
    pub state: LyricsState,
    /// First line shown, for lyrics without timing (scrolled by hand).
    pub scroll: usize,
}

pub struct Art {
    pub url: String,
    pub size: (u16, u16),
    /// None while it's being fetched / decoded.
    pub proto: Option<Protocol>,
    /// The downloaded cover, kept so a resize re-encodes it without fetching it again.
    pub bytes: Option<bytes::Bytes>,
    /// An encode is running; resizes wait for it instead of starting one each.
    pub busy: bool,
}

pub struct Chooser {
    pub track: Row,
    pub filter: String,
    pub sel: usize,
    /// Playlists that hold the song (✓); Enter on one removes it.
    pub holding: HashSet<String>,
    /// Still asking Spotify about the playlists not loaded yet.
    pub checking: bool,
}

impl Chooser {
    /// Editable playlists matching the filter: (uri, name).
    pub fn matches<'a>(&self, app: &'a App) -> Vec<(&'a str, &'a str)> {
        let f = self.filter.to_lowercase();
        app.side
            .iter()
            .filter(|s| s.kind == SideKind::Playlist && s.editable)
            .filter(|s| f.is_empty() || s.name.to_lowercase().contains(&f))
            .map(|s| (s.uri.as_str(), s.name.as_str()))
            .collect()
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Decode a cover and encode it for the terminal at `size` cells. Runs on a blocking thread.
/// Decode a cover once for both the picture and its accent colour.
fn encode_art(picker: &Picker, bytes: &[u8], size: (u16, u16)) -> Result<(Protocol, Option<[u8; 3]>), String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let tint = Some(theme::cover_tint(&img));
    // Fill the cells exactly (cropping the few pixels the cell grid isn't square by), so the
    // cover meets its frame on every side instead of leaving a gap on one.
    let fs = picker.font_size();
    let (w, h) = (size.0 as u32 * fs.width as u32, size.1 as u32 * fs.height as u32);
    let img = img.resize_to_fill(w.max(1), h.max(1), image::imageops::FilterType::Triangle);
    picker
        .new_protocol(img, Size::new(size.0, size.1), Resize::Fit(Some(FilterType::Triangle)))
        .map(|p| (p, tint))
        .map_err(|e| e.to_string())
}

impl App {
    pub(super) fn liked_uri(&self) -> String {
        format!("spotify:user:{}:collection", self.session.username())
    }

    /// Look for a newer trill release (at most once a day) for the title bar.
    pub fn check_update(&self) {
        let Some(dir) = self.cache_dir.clone() else { return };
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Some(tag) = crate::update::check(&s, &dir).await {
                let _ = tx.send(Msg::Update(tag));
            }
        });
    }

    /// Load the liked songs, saved albums and followed artists in the background (uris only).
    pub fn load_liked(&self) {
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            // Liked songs and saved albums share one set; followed artists have their own.
            let (lib, artists) = tokio::join!(library::collection(&s, "collection"), library::collection(&s, "artist"));
            let res = lib.map(|mut v| {
                v.extend(artists.unwrap_or_default());
                v.into_iter().collect()
            });
            let _ = tx.send(Msg::Liked(res));
        });
    }

    /// `P`: play every track in the open list, opening the albums and playlists in it
    /// (an artist page plays the whole discography).
    pub(super) fn play_all(&mut self) {
        let rows = self.view.rows.clone();
        if rows.iter().all(|r| r.kind == Kind::Track) {
            if !rows.is_empty() {
                self.play_list(&rows, 0);
            }
            return;
        }
        self.status = format!("Loading everything in {}…", self.view.title);
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::PlayAll(api::expand(&s, &rows).await));
        });
    }

    /// The track a library action applies to: the highlighted row in the main list, else
    /// whatever is playing.
    fn target_track(&self) -> Option<Row> {
        if self.focus == Focus::Main && !self.lyrics_open
            && let Some(r) = self.view.state.selected().and_then(|i| self.view.rows.get(i))
                && r.kind == Kind::Track
            {
                return Some(r.clone());
            }
        self.now.clone()
    }

    /// Change a cached list (and the visible one, if it's showing) in place.
    fn edit_cached(&mut self, uri: &str, f: impl FnOnce(&mut Vec<Row>)) {
        let Some(rows) = self.cache.get_mut(uri) else { return };
        f(Arc::make_mut(rows));
        if let Some(d) = self.cache_dir.as_deref() {
            api::write_list_cache(d, uri, rows);
        }
        if self.view.uri == uri {
            let sel = self.view.state.selected();
            let (u, t, rows) = (self.view.uri.clone(), self.view.title.clone(), rows.clone());
            let offset = self.view.state.offset();
            self.show(u, t, rows);
            self.view.reselect(sel);
            *self.view.state.offset_mut() = offset;
        }
    }

    /// Swap the open list's rows for a fresh copy, keeping the highlighted song (by uri, as
    /// rows may have moved) and the scroll position.
    pub(super) fn replace_view_rows(&mut self, rows: api::Rows) {
        let keep = self.view.state.selected().and_then(|i| self.view.rows.get(i)).map(|r| r.uri.clone());
        let (sel, offset) = (self.view.state.selected(), self.view.state.offset());
        let (u, t) = (self.view.uri.clone(), self.view.title.clone());
        self.show(u, t, rows);
        match keep.and_then(|k| self.view.rows.iter().position(|r| r.uri == k)) {
            Some(i) => self.view.state.select(Some(i)),
            None => self.view.reselect(sel),
        }
        *self.view.state.offset_mut() = offset;
    }

    // ---------------------------------------------------------------- private session

    /// A private session trill started, until Spotify ends it (it can't read the real state).
    pub fn private_on(&self) -> bool {
        self.audio.private_until > super::unix_now()
    }

    /// `I`: the account's private session on / off. Optimistic, undone if Spotify says no.
    /// On Linux with Soloist it also hides Soloist from Discord and other Connect observers:
    /// the first time by restarting Soloist through the relay (relay.rs), after that by just
    /// flipping the shared flag and nudging Soloist to announce itself again.
    pub(super) fn toggle_private(&mut self) {
        if self.going_private {
            return;
        }
        let on = !self.private_on();
        let hide = cfg!(target_os = "linux") && self.engine.is_soloist();
        // Discord only notices a change of song or a pause: mid-song it would keep showing the
        // song after going private (once marked, nothing reaches it) and keep hiding it after
        // coming back. So pause while the flag says what Discord should see, then resume.
        if hide && self.playing && (on || self.relay_on) {
            if !on {
                self.apply_private();
            }
            self.going_private = true;
            self.engine.pause();
            if on {
                self.status = "Going private…".into();
            }
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let _ = tx.send(Msg::PrivateGo { apply: on });
            });
            return;
        }
        self.apply_private();
    }

    /// The private-session switch itself (after the pause, see `toggle_private`).
    pub(super) fn apply_private(&mut self) {
        let (on, was) = (!self.private_on(), self.audio.private_until);
        self.set_private_until(if on { super::unix_now() + library::PRIVATE_SECS } else { 0 });
        let hide = cfg!(target_os = "linux") && self.engine.is_soloist();
        self.status = match (on, hide, self.relay_on) {
            (true, true, false) => {
                self.want_relay = true;
                "Private session on for 6 h, hidden from friends and Discord (Soloist restarts once, ~3 s).".into()
            }
            (true, true, true) => "Private session on for 6 h, hidden from friends and Discord.".into(),
            (true, false, _) => "Private session on for 6 h, hidden from friends (Discord can still see this player).".into(),
            (false, ..) => "Private session off.".into(),
        };
        if hide && self.relay_on {
            self.nudge_soloist();
        }
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = library::set_private_session(&s, on).await;
            let _ = tx.send(Msg::Private { on, was, res });
        });
    }

    fn set_private_until(&mut self, until: u64) {
        self.audio.private_until = until;
        self.private_shared.store(until, std::sync::atomic::Ordering::Relaxed);
        self.audio.save();
    }

    /// Make Soloist announce its Connect state now, so the relay's flag (or its absence)
    /// reaches Discord without waiting for the next play / pause / song change. A seek to
    /// where it already is changes nothing you can hear.
    fn nudge_soloist(&mut self) {
        if self.now.is_some() {
            let at = self.position_ms();
            self.seek_to(at);
        }
    }

    // ---------------------------------------------------------------- links

    /// `y`: copy the link of the highlighted song, album, playlist or artist (else the song
    /// playing); `Y`: of the open list. OSC 52, so it also works over SSH.
    pub(super) fn copy_link(&mut self, open_list: bool) {
        let (uri, name) = if open_list {
            (Some(self.view.uri.clone()), self.view.title.clone())
        } else {
            let hit = match self.focus {
                Focus::Side => self.side_state.selected().and_then(|i| self.side.get(i)).map(|s| (s.uri.clone(), s.name.clone())),
                Focus::Main if !self.lyrics_open => {
                    self.view.state.selected().and_then(|i| self.view.rows.get(i)).map(|r| (r.uri.clone(), r.title.clone()))
                }
                _ => None,
            };
            match hit.or_else(|| self.now.as_ref().map(|r| (r.uri.clone(), r.title.clone()))) {
                Some((u, n)) => (Some(u), n),
                None => (None, String::new()),
            }
        };
        let Some(url) = uri.as_deref().and_then(web_link) else {
            self.status = "No Spotify link for this.".into();
            return;
        };
        let copied = crossterm::execute!(std::io::stdout(), crossterm::clipboard::CopyToClipboard::to_clipboard_from(&url));
        self.status = match copied {
            Ok(()) => format!("Copied link to “{name}”"),
            Err(e) => format!("Couldn't copy: {e}"),
        };
    }

    /// `u` / `U`: open the album / artist of the highlighted song, else of the one playing.
    pub(super) fn open_home(&mut self, artist: bool) {
        let hit = match self.focus {
            Focus::Main if !self.lyrics_open => self.view.state.selected().and_then(|i| self.view.rows.get(i)).filter(|r| r.kind == Kind::Track),
            _ => None,
        };
        let Some(uri) = hit.or(self.now.as_ref()).map(|r| r.uri.clone()) else {
            self.status = "No song to open the album or artist of.".into();
            return;
        };
        self.status = format!("Opening the {}…", if artist { "artist" } else { "album" });
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::Home(api::track_home(&s, &uri, artist).await));
        });
    }

    // ---------------------------------------------------------------- liking

    /// `f`: like a song, save an album or follow an artist (the highlighted one, else the song playing).
    pub(super) fn toggle_like(&mut self) {
        let highlighted = (self.focus == Focus::Main && !self.lyrics_open)
            .then(|| self.view.state.selected().and_then(|i| self.view.rows.get(i)))
            .flatten()
            .filter(|r| r.kind != Kind::Playlist)
            .cloned();
        let Some(row) = highlighted.or_else(|| self.now.clone()) else {
            self.status = "Nothing to like.".into();
            return;
        };
        let liked = !self.liked.contains(&row.uri);
        // Optimistic: the ♥ flips immediately; undone if Spotify says no.
        self.apply_like(&row, liked);
        let name = &row.title;
        self.status = match (row.kind, liked) {
            (Kind::Album, true) => format!("♥ Saved “{name}” to your albums"),
            (Kind::Album, false) => format!("Removed “{name}” from your albums"),
            (Kind::Artist, true) => format!("♥ Following {name}"),
            (Kind::Artist, false) => format!("Unfollowed {name}"),
            (_, true) => format!("♥ Liked “{name}”"),
            _ => format!("Removed “{name}” from Liked Songs"),
        };
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = library::set_liked(&s, &row.uri, liked).await;
            let _ = tx.send(Msg::LikeDone { row, liked, res });
        });
    }

    fn apply_like(&mut self, row: &Row, liked: bool) {
        let liked_uri = match row.kind {
            Kind::Album => api::SAVED_ALBUMS.to_string(),
            Kind::Artist => api::FOLLOWED_ARTISTS.to_string(),
            _ => self.liked_uri(),
        };
        if liked {
            self.liked.insert(row.uri.clone());
            let mut r = row.clone();
            r.added_at = now_ms();
            self.edit_cached(&liked_uri, |v| {
                if !v.iter().any(|x| x.uri == r.uri) {
                    v.insert(0, r);
                }
            });
        } else {
            self.liked.remove(&row.uri);
            self.edit_cached(&liked_uri, |v| v.retain(|x| x.uri != row.uri));
        }
    }

    // ---------------------------------------------------------------- playlists

    pub(super) fn open_chooser(&mut self) {
        let Some(track) = self.target_track() else {
            self.status = "Pick a song first.".into();
            return;
        };
        if !self.side.iter().any(|s| s.kind == SideKind::Playlist && s.editable) {
            self.status = "None of your playlists can be edited.".into();
            return;
        }
        // Loaded playlists answer right away; the others are fetched (uris only).
        let mut holding = HashSet::new();
        let mut ask = Vec::new();
        for s in self.side.iter().filter(|s| s.kind == SideKind::Playlist && s.editable) {
            match self.cache.get(&s.uri) {
                Some(rows) if rows.iter().any(|r| r.uri == track.uri) => {
                    holding.insert(s.uri.clone());
                }
                Some(_) => {}
                None => ask.push(s.uri.clone()),
            }
        }
        let checking = !ask.is_empty();
        if checking {
            let (s, tx, t) = (self.session.clone(), self.tx.clone(), track.uri.clone());
            tokio::spawn(async move {
                let playlists = library::playlists_with(s, ask, t.clone()).await;
                let _ = tx.send(Msg::Holding { track: t, playlists });
            });
        }
        // A song playing outside its list (a recommendation, say) most likely belongs in that list.
        let mut c = Chooser { track, filter: String::new(), sel: 0, holding, checking };
        if !self.ctx.iter().any(|r| r.uri == c.track.uri)
            && let Some(i) = c.matches(self).iter().position(|(u, _)| *u == self.ctx_src.0)
        {
            c.sel = i;
        }
        self.chooser = Some(c);
    }

    pub(super) fn on_chooser_key(&mut self, k: KeyEvent) {
        let Some(mut c) = self.chooser.take() else { return };
        let n = c.matches(self).len();
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Up => c.sel = c.sel.saturating_sub(1),
            KeyCode::Down => c.sel = (c.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => c.sel = c.sel.saturating_sub(10),
            KeyCode::PageDown => c.sel = (c.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Backspace => {
                c.filter.pop();
                c.sel = 0;
            }
            KeyCode::Char(ch) => {
                c.filter.push(ch);
                c.sel = 0;
            }
            KeyCode::Enter => {
                if let Some((uri, name)) = c.matches(self).get(c.sel).map(|(u, n)| (u.to_string(), n.to_string())) {
                    // Toggle, like the ✓ boxes in the official "Save in" dialog; stays open for more.
                    let add = !c.holding.contains(&uri);
                    if add {
                        c.holding.insert(uri.clone());
                    } else {
                        c.holding.remove(&uri);
                    }
                    self.status = format!("{} “{}” {} {name}…", if add { "Adding" } else { "Removing" }, c.track.title, if add { "to" } else { "from" });
                    let (s, tx, row) = (self.session.clone(), self.tx.clone(), c.track.clone());
                    tokio::spawn(async move {
                        let res = if add {
                            library::playlist_add(&s, &uri, &row.uri).await
                        } else {
                            library::playlist_remove(&s, &uri, &row.uri).await
                        };
                        let _ = tx.send(Msg::PlaylistEdit { playlist: uri, name, row, add, res });
                    });
                }
            }
            _ => {}
        }
        self.chooser = Some(c);
    }

    pub(super) fn remove_from_playlist(&mut self) {
        if self.view.uri == "trill:queue" {
            // the songs you queued with `a`; the rest of the queue is just the list playing
            match self.view.state.selected().and_then(|i| self.queue_src.get(i)).copied() {
                Some(super::QSrc::Manual(q)) => {
                    if let Some(r) = self.queue.remove(q) {
                        self.status = format!("Removed “{}” from the queue", r.title);
                    }
                    self.refresh_queue_view();
                }
                _ => self.status = "Only songs you queued (a) can be removed here.".into(),
            }
            return;
        }
        let Some(side) = self.side.iter().find(|s| s.uri == self.view.uri && s.kind == SideKind::Playlist) else {
            self.status = "Open one of your playlists to remove songs from it.".into();
            return;
        };
        if !side.editable {
            self.status = format!("{} isn't yours to edit.", side.name);
            return;
        }
        let (playlist, name) = (side.uri.clone(), side.name.clone());
        let Some(row) = self.view.state.selected().and_then(|i| self.view.rows.get(i)).cloned() else { return };
        let confirmed = self
            .confirm_remove
            .take()
            .is_some_and(|(u, at)| u == row.uri && at.elapsed() < Duration::from_secs(4));
        if !confirmed {
            self.status = format!("Remove “{}” from {name}? Press D again.", row.title);
            self.confirm_remove = Some((row.uri, Instant::now()));
            return;
        }
        self.status = format!("Removing “{}”…", row.title);
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = library::playlist_remove(&s, &playlist, &row.uri).await;
            let _ = tx.send(Msg::PlaylistEdit { playlist, name, row, add: false, res });
        });
    }

    /// `X` on a For You song or a ✦ smart-shuffle song: never suggest it again (kept in
    /// `not_interested`). It leaves For You at once; a ✦ song leaves the queue (and is skipped
    /// if it's playing).
    pub(super) fn mark_not_interested(&mut self) {
        let row = match self.focus {
            Focus::Main if !self.lyrics_open => self.view.state.selected().and_then(|i| self.view.rows.get(i)).cloned(),
            _ => None,
        }
        .filter(|r| r.kind == Kind::Track && (self.view.uri == FOR_YOU || self.is_rec(&r.uri)))
        .or_else(|| self.now.clone().filter(|r| self.is_rec(&r.uri)));
        let Some(row) = row else {
            self.status = "X is for suggestions: a song in For You, or a ✦ song from smart shuffle.".into();
            return;
        };
        if self.not_interested.insert(row.uri.clone())
            && let Some(d) = self.data_dir.as_deref()
        {
            discover::add_not_interested(d, &row.uri);
        }
        self.edit_cached(FOR_YOU, |v| v.retain(|r| r.uri != row.uri));
        if let Some(i) = self.recs.iter().position(|r| r.uri == row.uri) {
            let idx = self.ctx.len() + i;
            if self.now.as_ref().is_some_and(|n| n.uri == row.uri) {
                self.next(false);
            }
            // later in the play order: take it out (the entry at the cursor is what plays now)
            if let Some(p) = self.order.iter().position(|&o| o == idx).filter(|&p| p > self.cursor) {
                self.order.remove(p);
            }
            self.refresh_queue_view();
        }
        self.status = format!("Not interested in “{}”: it won't be suggested again.", row.title);
    }

    // ---------------------------------------------------------------- lyrics

    pub(super) fn toggle_lyrics(&mut self) {
        self.lyrics_open = !self.lyrics_open;
        if self.lyrics_open && self.now.is_none() {
            self.status = "Play something to see its lyrics.".into();
        }
    }

    /// Keys while the lyrics view is open. Returns true if handled.
    pub(super) fn on_lyrics_key(&mut self, k: KeyEvent) -> bool {
        let step: isize = match k.code {
            KeyCode::Esc | KeyCode::Char('L') => {
                self.lyrics_open = false;
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Char('h') | KeyCode::Char('l') => return true,
            _ => return false,
        };
        if let Some(slot) = self.lyrics.as_mut()
            && let LyricsState::Ready(l) = &slot.state
        {
            slot.scroll = (slot.scroll as isize + step).clamp(0, l.lines.len().saturating_sub(1) as isize) as usize;
        }
        true
    }

    /// A click on a synced lyrics line: when that line starts.
    pub(super) fn lyric_at(&self, pos: (u16, u16)) -> Option<u32> {
        let slot = self.lyrics.as_ref().filter(|l| self.now.as_ref().is_some_and(|n| n.uri == l.uri))?;
        let LyricsState::Ready(l) = &slot.state else { return None };
        let i = super::popup_row(self.lyrics_rows, l.lines.len(), pos).filter(|_| l.synced)?;
        Some(l.lines[i].0)
    }

    /// When the highlighted lyric line next changes (so we redraw exactly then).
    pub fn lyrics_deadline(&self) -> Option<Instant> {
        if !self.lyrics_open || !self.playing {
            return None;
        }
        let slot = self.lyrics.as_ref()?;
        let LyricsState::Ready(l) = &slot.state else { return None };
        if self.now.as_ref()?.uri != slot.uri {
            return None;
        }
        let pos = self.position_ms();
        let next = l.next_change(pos)?;
        Some(Instant::now() + Duration::from_millis((next - pos) as u64))
    }

    // ---------------------------------------------------------------- art

    pub(super) fn toggle_art(&mut self) {
        if self.img_picker.is_none() {
            self.status = "Album art is off (art = off in config).".into();
            return;
        }
        self.art_on = !self.art_on;
        self.save_prefs();
        if !self.art_on {
            self.art = None; // free the encoded image
            self.info_art = None;
        }
    }

    /// Zooming changes the font's pixel size, which iTerm2 and sixel images are drawn in:
    /// pick up the new size and re-encode the cover. Takes one ioctl per resize.
    /// shortcut: terminals that report no pixel size (ws_xpixel = 0) keep the startup size; re-query them if one matters.
    pub fn on_resize(&mut self) {
        let (Some(p), Ok(ws)) = (self.img_picker.as_ref(), crossterm::terminal::window_size()) else { return };
        if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
            return;
        }
        let (w, h) = (ws.width / ws.columns, ws.height / ws.rows);
        let old = p.font_size();
        if (w, h) == (old.width, old.height) {
            return;
        }
        // The query-built picker can't change its font size; from_fontsize is the only way.
        #[allow(deprecated)]
        let mut np = Picker::from_fontsize(ratatui_image::FontSize::new(w, h));
        np.set_protocol_type(p.protocol_type());
        self.img_picker = Some(np);
        for a in [&mut self.art, &mut self.info_art].into_iter().flatten() {
            a.size = (0, 0); // no longer matches art_want, so refresh_media re-encodes
            // The old picture is sized in old pixels: it would spill over the borders (zoom out) or
            // sit small in the corner (zoom in) and leave bits behind, so show none until the new one.
            a.proto = None;
        }
    }

    /// Start whatever lyrics / art fetches the current state needs. Cheap when nothing changed.
    pub fn refresh_media(&mut self) {
        self.refresh_info();
        let Some(now) = self.now.as_ref() else {
            self.art = None;
            return;
        };
        if self.lyrics_open && self.lyrics.as_ref().is_none_or(|l| l.uri != now.uri) {
            let uri = now.uri.clone();
            self.lyrics = Some(LyricsSlot { uri: uri.clone(), state: LyricsState::Loading, scroll: 0 });
            let (s, tx) = (self.session.clone(), self.tx.clone());
            tokio::spawn(async move {
                let res = library::lyrics(&s, &uri).await;
                let _ = tx.send(Msg::Lyrics { uri, res });
            });
        }
        let Some(url) = now.art.clone() else { return };
        let art_needed = self.art_on
            && self.img_picker.is_some()
            && self.art_want.is_some_and(|size| self.art.as_ref().is_none_or(|a| a.url != url || a.size != size));
        // The cover theme needs the cover's colour even when the picture isn't shown; when it
        // is, the picture's decode works it out too, so the image is fetched once.
        let art_has_url = art_needed || self.art.as_ref().is_some_and(|a| a.url == url);
        if self.theme == theme::COVER && self.tint.as_ref().is_none_or(|t| t.0 != url) {
            self.tint = Some((url.clone(), self.tint.take().and_then(|t| t.1))); // keep old colour meanwhile
            if !art_has_url {
                let (s, tx, url) = (self.session.clone(), self.tx.clone(), url.clone());
                tokio::spawn(async move {
                    let Ok(bytes) = s.spclient().request_url(&url).await else { return };
                    let tint = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes).ok().map(|i| theme::cover_tint(&i)));
                    let _ = tx.send(Msg::Tint { url, tint: tint.await.ok().flatten() });
                });
            }
        }
        let (Some(picker), Some(size), true) = (self.img_picker.as_ref(), self.art_want, art_needed) else {
            return;
        };
        let same = self.art.as_ref().filter(|a| a.url == url);
        if same.is_some_and(|a| a.busy) {
            return; // the next draw after it finishes picks up the newest size
        }
        let bytes = same
            .and_then(|a| a.bytes.clone())
            .or_else(|| self.next_art.take().filter(|(u, _)| *u == url).map(|(_, b)| b));
        // Keep showing the old cover until the new one is ready (no flicker); the url
        // check in on_extra_msg drops results that arrive after another change.
        let keep = self.art.take().and_then(|a| a.proto);
        self.art = Some(Art { url: url.clone(), size, proto: keep, bytes: bytes.clone(), busy: true });
        self.spawn_art(picker.clone(), url, size, bytes, false);
    }

    /// The open album's or artist's header details, then its picture once there's room for it.
    fn refresh_info(&mut self) {
        let uri = &self.view.uri;
        if !uri.starts_with("spotify:album:") && !uri.starts_with("spotify:artist:") {
            return;
        }
        if !self.infos.contains_key(uri) {
            self.infos.insert(uri.clone(), api::Info::default());
            let (s, tx, uri) = (self.session.clone(), self.tx.clone(), uri.clone());
            tokio::spawn(async move {
                let res = api::info(&s, &uri).await;
                let _ = tx.send(Msg::Info { uri, res });
            });
            return;
        }
        let url = self.infos.get(uri).and_then(|i| i.image.clone());
        let (Some(picker), Some(size), Some(url), true) = (self.img_picker.as_ref(), self.info_want, url, self.art_on) else { return };
        let same = self.info_art.as_ref().filter(|a| a.url == url);
        if same.is_some_and(|a| a.busy || a.size == size) {
            return;
        }
        let bytes = same.and_then(|a| a.bytes.clone());
        self.info_art = Some(Art { url: url.clone(), size, proto: None, bytes: bytes.clone(), busy: true });
        self.spawn_art(picker.clone(), url, size, bytes, true);
    }

    /// Fetch (unless `bytes` are known) and encode a picture; comes back as `Msg::Art`.
    fn spawn_art(&self, picker: Picker, url: String, size: (u16, u16), bytes: Option<bytes::Bytes>, header: bool) {
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let bytes = match bytes {
                Some(b) => Ok(b),
                None => s.spclient().request_url(&url).await.map_err(|e| e.to_string()),
            };
            let res = match bytes {
                Ok(bytes) => tokio::task::spawn_blocking(move || encode_art(&picker, &bytes, size).map(|r| (r, bytes)))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e),
            };
            let res = res.map(|((proto, tint), bytes)| {
                if !header {
                    let _ = tx.send(Msg::Tint { url: url.clone(), tint });
                }
                (proto, bytes)
            });
            let _ = tx.send(Msg::Art { url, size, header, res });
        });
    }

    /// Download the next song's cover ahead of time (it's queued ~8 s before this one ends),
    /// so the switch shows it at once instead of ~0.2 s later.
    pub(super) fn prefetch_art(&mut self, next: &Row) {
        let Some(url) = next.art.clone() else { return };
        if !self.art_on || self.img_picker.is_none() || self.art.as_ref().is_some_and(|a| a.url == url) {
            return;
        }
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            if let Ok(bytes) = s.spclient().request_url(&url).await {
                let _ = tx.send(Msg::NextArt { url, bytes });
            }
        });
    }

    // ---------------------------------------------------------------- results

    pub(super) fn on_extra_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Liked(Ok(set)) => {
                self.liked = set;
                // The liked-songs context is a few MB of protobuf, freed by now.
                crate::trim_heap();
            }
            Msg::Liked(Err(e)) => log::warn!("liked songs: {e}"),
            Msg::LikeDone { row, liked, res: Err(e) } => {
                self.apply_like(&row, !liked);
                self.status = format!("Couldn't update your library: {e}");
            }
            Msg::LikeDone { .. } => {}
            Msg::PlayAll(Ok(rows)) if rows.is_empty() => self.status = "Nothing to play here.".into(),
            Msg::PlayAll(Err(e)) => self.status = format!("Couldn't load everything: {e}"),
            Msg::PlayAll(Ok(rows)) => {
                let n = rows.len();
                self.play_list(&Arc::new(rows), 0);
                self.status = format!("Playing all {n} tracks");
            }
            Msg::Lyrics { uri, res } => {
                if let Some(slot) = self.lyrics.as_mut()
                    && slot.uri == uri
                {
                    slot.state = match res {
                        Ok(Some(l)) => LyricsState::Ready(l),
                        Ok(None) => LyricsState::Missing,
                        Err(e) => LyricsState::Failed(e),
                    };
                }
            }
            Msg::Update(tag) => self.update = Some(tag),
            Msg::Tint { url, tint } => {
                // kept for any theme (switching to `cover` later needs no refetch), but only
                // for the song that's still playing
                if self.now.as_ref().and_then(|n| n.art.as_ref()) == Some(&url) {
                    self.tint = Some((url, tint));
                }
            }
            Msg::NextArt { url, bytes } => self.next_art = Some((url, bytes)),
            Msg::PrivateGo { apply } => {
                self.going_private = false;
                if apply {
                    self.apply_private();
                }
                if !self.playing && self.now.is_some() {
                    if self.want_relay {
                        self.playing = true; // the restart through the relay resumes it playing
                    } else {
                        self.engine.resume();
                    }
                }
            }
            Msg::Home(Ok((uri, title))) => self.open(uri, title, true, false),
            Msg::Home(Err(e)) => self.status = format!("Couldn't open it: {e}"),
            Msg::Private { res: Ok(()), .. } => {}
            Msg::Private { on, was, res: Err(e) } => {
                self.set_private_until(was);
                if self.relay_on {
                    self.nudge_soloist();
                }
                self.status = format!("Couldn't turn the private session {}: {e}", if on { "on" } else { "off" });
            }
            Msg::Info { uri, res } => {
                let info = res.unwrap_or_else(|e| {
                    log::warn!("info for {uri}: {e}");
                    api::Info::default()
                });
                self.infos.insert(uri, info);
            }
            Msg::Art { url, size, header, res } => {
                if let Some(a) = if header { self.info_art.as_mut() } else { self.art.as_mut() }
                    && a.url == url
                {
                    a.busy = false;
                    // Decoding a 640 px cover allocates a few MB that's freed right away.
                    crate::trim_heap();
                    match res {
                        Ok((p, bytes)) => {
                            a.bytes = Some(bytes);
                            if a.size == size {
                                a.proto = Some(p);
                            }
                        }
                        Err(e) if a.size == size => {
                            log::warn!("album art: {e}");
                            a.proto = None;
                        }
                        Err(_) => {}
                    }
                }
            }
            Msg::PlaylistEdit { playlist, name, row, add, res } => match res {
                Ok(()) if add => {
                    self.status = format!("Added “{}” to {name}", row.title);
                    let mut r = row;
                    r.added_at = now_ms();
                    self.edit_cached(&playlist, |v| v.push(r));
                }
                Ok(()) => {
                    self.status = format!("Removed “{}” from {name}", row.title);
                    self.edit_cached(&playlist, |v| {
                        if let Some(i) = v.iter().position(|x| x.uri == row.uri) {
                            v.remove(i);
                        }
                    });
                }
                Err(e) => {
                    self.status = format!("Couldn't change {name}: {e}");
                    // undo the ✓ we showed straight away
                    if let Some(c) = self.chooser.as_mut().filter(|c| c.track.uri == row.uri) {
                        if add {
                            c.holding.remove(&playlist);
                        } else {
                            c.holding.insert(playlist);
                        }
                    }
                }
            },
            Msg::Recs { list, rows } => {
                if !self.smart || list != self.ctx_src.0 || !self.recs.is_empty() {
                    return;
                }
                if rows.is_empty() {
                    self.status = "Spotify has no recommendations for this list.".into();
                    return;
                }
                let have: HashSet<&str> = self.ctx.iter().map(|r| r.uri.as_str()).collect();
                let mut seen = HashSet::new();
                let recs: Vec<Row> = rows
                    .into_iter()
                    .filter(|r| !have.contains(r.uri.as_str()) && !self.not_interested.contains(&r.uri) && seen.insert(r.uri.clone()))
                    .collect();
                self.recs = recs;
                self.weave();
                self.refresh_queue_view();
            }
            Msg::Holding { track, playlists } => {
                if let Some(c) = self.chooser.as_mut().filter(|c| c.track.uri == track) {
                    c.holding.extend(playlists);
                    c.checking = false;
                }
            }
            _ => {}
        }
    }
}

/// `spotify:album:ID` → `https://open.spotify.com/album/ID`; None for Liked Songs, local files
/// and trill's own views, which have no public page.
fn web_link(uri: &str) -> Option<String> {
    let mut p = uri.split(':');
    let (Some("spotify"), Some(kind), Some(id), None) = (p.next(), p.next(), p.next(), p.next()) else { return None };
    matches!(kind, "track" | "album" | "playlist" | "artist").then(|| format!("https://open.spotify.com/{kind}/{id}"))
}

#[test]
fn web_links() {
    assert_eq!(web_link("spotify:track:4uLU6hMCjMI75M1A2tKUQC").as_deref(), Some("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"));
    assert_eq!(web_link("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M").as_deref(), Some("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"));
    assert_eq!(web_link("spotify:user:me:collection"), None);
    assert_eq!(web_link("spotify:local:a:b:c:180"), None);
    assert_eq!(web_link("trill:search"), None);
}
