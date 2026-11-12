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
    // Shrink with a cheap filter first: the cover is 640 px, the target is ~a few hundred.
    let fs = picker.font_size();
    let (w, h) = (size.0 as u32 * fs.width as u32, size.1 as u32 * fs.height as u32);
    let img = if img.width() > w * 2 || img.height() > h * 2 { img.thumbnail(w * 2, h * 2) } else { img };
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
        if self.view.uri == uri {
            let sel = self.view.state.selected();
            let (u, t, rows) = (self.view.uri.clone(), self.view.title.clone(), rows.clone());
            let offset = self.view.state.offset();
            self.show(u, t, rows);
            self.view.reselect(sel);
            *self.view.state.offset_mut() = offset;
        }
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
        if !self.ctx[..self.base].iter().any(|r| r.uri == c.track.uri)
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
        if let Some(a) = self.art.as_mut() {
            a.size = (0, 0); // no longer matches art_want, so refresh_media re-encodes (old cover stays up meanwhile)
        }
    }

    /// Start whatever lyrics / art fetches the current state needs. Cheap when nothing changed.
    pub fn refresh_media(&mut self) {
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
        // Keep showing the old cover until the new one is ready (no flicker); the url
        // check in on_extra_msg drops results that arrive after another change.
        let keep = self.art.take().and_then(|a| a.proto);
        self.art = Some(Art { url: url.clone(), size, proto: keep });
        let (s, tx, picker, url) = (self.session.clone(), self.tx.clone(), picker.clone(), url.clone());
        tokio::spawn(async move {
            let res = match s.spclient().request_url(&url).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || encode_art(&picker, &bytes, size))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e.to_string()),
            };
            let res = res.map(|(proto, tint)| {
                let _ = tx.send(Msg::Tint { url: url.clone(), tint });
                proto
            });
            let _ = tx.send(Msg::Art { url, size, res });
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
            Msg::PlayAll(rows) if rows.is_empty() => self.status = "Nothing to play here.".into(),
            Msg::PlayAll(rows) => {
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
            Msg::Art { url, size, res } => {
                if let Some(a) = self.art.as_mut()
                    && a.url == url
                    && a.size == size
                {
                    // Decoding a 640 px cover allocates a few MB that's freed right away.
                    crate::trim_heap();
                    match res {
                        Ok(p) => a.proto = Some(p),
                        Err(e) => {
                            log::warn!("album art: {e}");
                            a.proto = None;
                        }
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
                if !self.smart || list != self.ctx_src.0 || self.ctx.len() != self.base {
                    return;
                }
                if rows.is_empty() {
                    self.status = "Spotify has no recommendations for this list.".into();
                    return;
                }
                let mut have: HashSet<String> = self.ctx.iter().map(|r| r.uri.clone()).collect();
                let mut ctx: Vec<Row> = self.ctx.to_vec();
                ctx.extend(rows.into_iter().filter(|r| have.insert(r.uri.clone())));
                self.ctx = Arc::new(ctx);
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
