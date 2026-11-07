//! Application state, key handling and playback queue logic.

mod extras;

pub use extras::{Art, Chooser, LyricsSlot, LyricsState};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Instant, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use librespot_core::Session;
use ratatui::widgets::{ListState, TableState};
use tokio::sync::{Mutex, mpsc::UnboundedSender};

use crate::{
    api::{self, Kind, Row, Rows},
    audio::{AudioSettings, BANDS, Eq},
    auth::WebToken,
    engine::{Engine, EngineEvent},
    mpris::Remote,
};

pub enum Msg {
    Playlists(Result<Vec<(String, String, bool, bool)>, String>),
    Rows { uri: String, title: String, rows: Result<Vec<Row>, String> },
    Search { query: String, rows: Result<(Vec<Row>, &'static str), String> },
    /// Every liked track uri (for the ♥ markers).
    Liked(Result<HashSet<String>, String>),
    LikeDone { row: Row, liked: bool, res: Result<(), String> },
    Lyrics { uri: String, res: Result<Option<crate::library::Lyrics>, String> },
    Art { url: String, size: (u16, u16), res: Result<ratatui_image::protocol::Protocol, String> },
    /// Accent colour picked from a cover (`None`: black-and-white cover).
    Tint { url: String, tint: Option<[u8; 3]> },
    PlaylistEdit { playlist: String, name: String, row: Row, add: bool, res: Result<(), String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Side,
    Main,
    Input,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}

/// How playlists / liked songs are ordered on screen (and therefore played).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Most recently added first (default).
    Added,
    /// The playlist's own order.
    Original,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SideKind {
    Liked,
    Queue,
    Search,
    Playlist,
}

pub struct SideItem {
    pub name: String,
    pub uri: String,
    pub kind: SideKind,
    /// Playlist we can add songs to / remove songs from.
    pub editable: bool,
    /// Pinned to the top of Your Library.
    pub pinned: bool,
}

/// What a row in the Queue view refers to, so Enter can act on it.
#[derive(Clone, Copy)]
enum QSrc {
    Now,
    Manual(usize),
    Upcoming(usize), // position in `order`
}

pub struct View {
    pub uri: String,
    pub title: String,
    pub rows: Rows,
    pub state: TableState,
    /// Rows are shown newest-added first.
    pub by_added: bool,
}

pub struct App {
    pub session: Session,
    pub engine: Engine,
    pub web: Option<Arc<Mutex<WebToken>>>,
    tx: UnboundedSender<Msg>,

    pub side: Vec<SideItem>,
    pub side_state: ListState,
    pub view: View,
    back: Vec<View>,
    queue_src: Vec<QSrc>,
    cache: HashMap<String, Rows>,
    pending: Option<String>,
    search_rows: Rows,
    pub search_title: String,

    pub sort: Sort,
    pub audio: AudioSettings,
    pub eq: Option<Eq>,
    /// `eq = off` in config disables the EQ entirely.
    pub eq_allowed: bool,
    /// Where the playlist list is cached so the sidebar fills instantly on start.
    pub cache_dir: Option<std::path::PathBuf>,
    /// EQ panel open, with this band selected.
    pub eq_panel: Option<usize>,
    /// When to restart Soloist to apply a crossfade change (debounced).
    pub restart_at: Option<Instant>,
    /// After a restart: position to seek to once the track is back.
    pending_seek: Option<u32>,
    /// After a restart while paused: reload this position on the next play.
    reload_at: Option<u32>,
    /// Ignore stale "Soloist stopped" messages from the old process.
    quiet_until: Instant,
    pub focus: Focus,
    pub input: String,
    pub status: String,
    pub loading: bool,
    pub quit: bool,

    // playback
    ctx: Rows,
    order: Vec<usize>,
    cursor: usize,
    pub queue: VecDeque<Row>,
    pub now: Option<Row>,
    pub playing: bool,
    pos_ms: u32,
    pos_at: Instant,
    /// Soloist: the uri we asked for and when; transitional events are ignored until it starts.
    awaiting: Option<(String, Instant)>,
    /// Soloist: the next track we already put in Soloist's own queue (so it switches
    /// gaplessly instead of autoplaying). Soloist can't un-queue, so we track it to consume it.
    ahead: Option<String>,
    vol: u32,
    vol_set_at: Instant,
    skips: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
    rng: u64,

    // library, lyrics, art
    pub liked: HashSet<String>,
    pub lyrics_open: bool,
    pub lyrics: Option<LyricsSlot>,
    /// Terminal image support (None = art disabled).
    pub img_picker: Option<ratatui_image::picker::Picker>,
    pub art_on: bool,
    pub theme: usize,
    /// (cover url, its colour) for the `cover` theme.
    pub tint: Option<(String, Option<[u8; 3]>)>,
    pub art: Option<Art>,
    /// Cell size the UI has room for (set while drawing).
    pub art_want: Option<(u16, u16)>,
    /// "Add to playlist" popup.
    pub chooser: Option<Chooser>,
    /// First `D` press: (track uri, when) — the second press within a few seconds removes.
    confirm_remove: Option<(String, Instant)>,
}

const SEEK_MS: u32 = 10_000;
const VOL_STEP: u32 = 5; // percent

impl App {
    pub fn new(
        session: Session,
        engine: Engine,
        vol: u32,
        web: Option<Arc<Mutex<WebToken>>>,
        tx: UnboundedSender<Msg>,
        audio: AudioSettings,
        eq: Option<Eq>,
    ) -> Self {
        let user = session.username();
        let (shuffle, sort) = (audio.shuffle, if audio.sort_original { Sort::Original } else { Sort::Added });
        let repeat = [Repeat::Off, Repeat::All, Repeat::One][audio.repeat.min(2) as usize];
        let (theme, tint) = (audio.theme, audio.tint.map(|c| (String::new(), Some(c))));
        let side = vec![
            SideItem { name: "♥ Liked Songs".into(), uri: format!("spotify:user:{user}:collection"), kind: SideKind::Liked, editable: false, pinned: false },
            SideItem { name: "≡ Queue".into(), uri: "cachify:queue".into(), kind: SideKind::Queue, editable: false, pinned: false },
            SideItem { name: "⌕ Search".into(), uri: "cachify:search".into(), kind: SideKind::Search, editable: false, pinned: false },
        ];
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        
        App {
            session,
            engine,
            web,
            tx,
            side,
            side_state: ListState::default().with_selected(Some(0)),
            view: View { uri: String::new(), title: "cachify".into(), rows: Arc::new(Vec::new()), state: TableState::default(), by_added: false },
            back: Vec::new(),
            queue_src: Vec::new(),
            cache: HashMap::new(),
            pending: None,
            search_rows: Arc::new(Vec::new()),
            search_title: "Search".into(),
            sort,
            audio,
            eq,
            eq_allowed: true,
            cache_dir: None,
            eq_panel: None,
            restart_at: None,
            pending_seek: None,
            reload_at: None,
            quiet_until: Instant::now(),
            focus: Focus::Side,
            input: String::new(),
            status: "Loading playlists…".into(),
            loading: false,
            quit: false,
            ctx: Arc::new(Vec::new()),
            order: Vec::new(),
            cursor: 0,
            queue: VecDeque::new(),
            now: None,
            playing: false,
            pos_ms: 0,
            pos_at: Instant::now(),
            awaiting: None,
            ahead: None,
            vol,
            vol_set_at: Instant::now().checked_sub(std::time::Duration::from_secs(10)).unwrap_or_else(Instant::now),
            skips: 0,
            shuffle,
            repeat,
            rng: seed,
            liked: HashSet::new(),
            lyrics_open: false,
            lyrics: None,
            img_picker: None,
            art_on: true,
            theme,
            tint,
            art: None,
            art_want: None,
            chooser: None,
            confirm_remove: None,
        }
    }

    // ---------------------------------------------------------------- data loading

    /// Shows the cached playlist list right away (if any), then refreshes it.
    pub fn load_playlists(&mut self) {
        if self.side.len() <= 3
            && let Some(list) = self.cache_dir.as_deref().and_then(api::read_playlist_cache)
        {
            self.set_playlists(list);
            self.status = format!("{} playlists. Tab to switch panes, / to search, ? for keys.", self.side.len() - 3);
        }
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::Playlists(api::playlists(&s).await));
        });
    }

    /// Replace the playlists in the sidebar, keeping the selected item selected.
    fn set_playlists(&mut self, list: Vec<(String, String, bool, bool)>) {
        let sel = self.side_state.selected().and_then(|i| self.side.get(i)).map(|s| s.uri.clone());
        self.side.truncate(3);
        self.side.extend(list.into_iter().map(|(uri, name, editable, pinned)| SideItem { name, uri, kind: SideKind::Playlist, editable, pinned }));
        let i = sel.and_then(|u| self.side.iter().position(|s| s.uri == u)).unwrap_or(0);
        self.side_state.select(Some(i.min(self.side.len() - 1)));
    }

    fn open(&mut self, uri: String, title: String, push_back: bool, force: bool) {
        self.lyrics_open = false; // the lyrics view would hide the list being opened
        if push_back && !self.view.uri.is_empty() {
            let prev = std::mem::replace(
                &mut self.view,
                View { uri: String::new(), title: String::new(), rows: Arc::new(Vec::new()), state: TableState::default(), by_added: false },
            );
            self.back.push(prev);
            if self.back.len() > 20 {
                self.back.remove(0);
            }
        }
        if !force && let Some(rows) = self.cache.get(&uri).cloned() {
            self.show(uri, title, rows);
            return;
        }
        self.view.uri = uri.clone();
        self.view.title = title.clone();
        self.view.rows = Arc::new(Vec::new());
        self.view.state = TableState::default();
        self.loading = true;
        self.status = format!("Loading {title}…");
        self.pending = Some(uri.clone());
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let rows = api::context_rows(&s, &uri).await;
            let _ = tx.send(Msg::Rows { uri, title, rows });
        });
    }

    fn show(&mut self, uri: String, title: String, rows: Rows) {
        let (rows, by_added) = self.sorted(&uri, rows);
        let sel = if rows.is_empty() { None } else { Some(0) };
        self.view = View { uri, title, rows, state: TableState::default().with_selected(sel), by_added };
    }

    /// Apply the sort mode to playlists and liked songs (albums/artists keep their order).
    fn sorted(&self, uri: &str, rows: Rows) -> (Rows, bool) {
        let sortable = uri.starts_with("spotify:playlist:") || uri.contains(":collection");
        if self.sort != Sort::Added || !sortable || !rows.iter().any(|r| r.added_at > 0) {
            return (rows, false);
        }
        if rows.windows(2).all(|w| w[0].added_at >= w[1].added_at) {
            return (rows, true); // already newest-first (liked songs): no copy
        }
        let mut v = (*rows).clone();
        v.sort_by(|a, b| b.added_at.cmp(&a.added_at)); // stable: ties keep playlist order
        (Arc::new(v), true)
    }

    fn toggle_sort(&mut self) {
        self.sort = if self.sort == Sort::Added { Sort::Original } else { Sort::Added };
        self.save_prefs();
        self.status = match self.sort {
            Sort::Added => "Sort: recently added first".into(),
            Sort::Original => "Sort: playlist order".into(),
        };
        let Some(orig) = self.cache.get(&self.view.uri).cloned() else { return };
        let keep = self.view.state.selected().and_then(|i| self.view.rows.get(i)).map(|r| r.uri.clone());
        let (uri, title) = (self.view.uri.clone(), self.view.title.clone());
        self.show(uri, title, orig);
        if let Some(k) = keep
            && let Some(i) = self.view.rows.iter().position(|r| r.uri == k)
        {
            self.view.state.select(Some(i));
        }
    }

    fn open_side(&mut self) {
        let Some(i) = self.side_state.selected() else { return };
        let item = &self.side[i];
        let (uri, name, kind) = (item.uri.clone(), item.name.clone(), item.kind);
        self.back.clear();
        self.lyrics_open = false;
        match kind {
            SideKind::Queue => self.show_queue(),
            SideKind::Search => {
                let rows = self.search_rows.clone();
                self.show("cachify:search".into(), self.search_title.clone(), rows);
            }
            SideKind::Liked | SideKind::Playlist => self.open(uri, name, false, false),
        }
        // Picking "Search" means "I want to search": open the search box right away.
        self.focus = if kind == SideKind::Search { Focus::Input } else { Focus::Main };
    }

    fn show_queue(&mut self) {
        let mut rows = Vec::new();
        self.queue_src.clear();
        if let Some(now) = &self.now {
            rows.push(now.clone());
            self.queue_src.push(QSrc::Now);
        }
        for (i, r) in self.queue.iter().enumerate() {
            rows.push(r.clone());
            self.queue_src.push(QSrc::Manual(i));
        }
        let start = if self.now.is_some() { self.cursor + 1 } else { self.cursor };
        for p in start..(start + 100).min(self.order.len()) {
            rows.push(self.ctx[self.order[p]].clone());
            self.queue_src.push(QSrc::Upcoming(p));
        }
        self.show("cachify:queue".into(), format!("Queue — {} manual, {} upcoming", self.queue.len(), rows.len().saturating_sub(self.queue.len() + self.now.is_some() as usize)), Arc::new(rows));
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Playlists(Ok(list)) => {
                if let Some(d) = &self.cache_dir {
                    api::write_playlist_cache(d, &list);
                }
                let had_cached = self.side.len() > 3;
                let n = list.len();
                self.set_playlists(list);
                if !had_cached {
                    self.status = format!("{n} playlists. Tab to switch panes, / to search, ? for keys.");
                }
            }
            Msg::Playlists(Err(e)) => self.status = format!("Couldn't load playlists: {e}  (R to retry)"),
            Msg::Rows { uri, title, rows } => {
                let is_current = self.pending.as_deref() == Some(uri.as_str());
                match rows {
                    Ok(rows) => {
                        let big = rows.len() > 500;
                        let rows: Rows = Arc::new(rows);
                        if uri == self.liked_uri() {
                            self.liked = rows.iter().map(|r| r.uri.clone()).collect();
                        }
                        self.cache.insert(uri.clone(), rows.clone());
                        if big {
                            // Loading a big list decodes megabytes of protobuf that is freed
                            // right away; hand those pages back to the OS instead of keeping them.
                            // SAFETY: malloc_trim only releases free heap memory.
                            unsafe {
                                libc::malloc_trim(0);
                            }
                        }
                        if is_current {
                            self.status = format!("{title}: {} tracks", rows.len());
                            self.show(uri, title, rows);
                        }
                    }
                    Err(e) if is_current => self.status = format!("Couldn't load {title}: {e}"),
                    Err(_) => {}
                }
                if is_current {
                    self.loading = false;
                    self.pending = None;
                }
            }
            Msg::Search { query, rows } => {
                self.loading = false;
                match rows {
                    Ok((rows, via)) => {
                        self.lyrics_open = false; // results replace whatever covers the list
                        self.status = format!("{} results for “{query}” ({via})", rows.len());
                        self.search_title = format!("Search: {query}");
                        self.search_rows = Arc::new(rows);
                        self.back.clear();
                        self.show("cachify:search".into(), self.search_title.clone(), self.search_rows.clone());
                        self.focus = Focus::Main;
                        if let Some(i) = self.side.iter().position(|s| s.kind == SideKind::Search) {
                            self.side_state.select(Some(i));
                        }
                    }
                    Err(e) => self.status = format!("Search failed: {e}"),
                }
            }
            other => self.on_extra_msg(other),
        }
    }

    fn search(&mut self, q: String) {
        if q.trim().is_empty() {
            return;
        }
        self.loading = true;
        self.status = format!("Searching “{q}”…");
        let (s, tx, web) = (self.session.clone(), self.tx.clone(), self.web.clone());
        tokio::spawn(async move {
            let rows = api::search(&s, web, &q).await;
            let _ = tx.send(Msg::Search { query: q, rows });
        });
    }

    // ---------------------------------------------------------------- playback

    pub fn position_ms(&self) -> u32 {
        if self.playing {
            self.pos_ms.saturating_add(self.pos_at.elapsed().as_millis() as u32)
        } else {
            self.pos_ms
        }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*: plenty for shuffling a playlist
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Rebuild `order`, keeping the current track at the cursor.
    fn reorder(&mut self, keep: Option<usize>) {
        self.order = (0..self.ctx.len()).collect();
        if self.shuffle {
            for i in (1..self.order.len()).rev() {
                let j = (self.rand() % (i as u64 + 1)) as usize;
                self.order.swap(i, j);
            }
            if let Some(k) = keep
                && let Some(p) = self.order.iter().position(|&x| x == k)
            {
                self.order.swap(0, p);
            }
            self.cursor = 0;
        } else {
            self.cursor = keep.unwrap_or(0);
        }
    }

    fn load(&mut self, row: Row) {
        if self.engine.is_soloist() && self.ahead.take().is_some() {
            // Burn the pre-queued track so it can't play later; `play` follows immediately.
            self.engine.skip_next();
        }
        self.engine.load(&row.uri);
        if self.engine.is_soloist() {
            self.awaiting = Some((row.uri.clone(), Instant::now()));
        }
        self.pos_ms = 0;
        self.pos_at = Instant::now();
        self.now = Some(row);
    }

    /// Play `rows[idx]`, making the track rows of that list the playback context.
    fn play_list(&mut self, rows: &Rows, idx: usize) {
        let tracks: Vec<Row> = rows.iter().filter(|r| r.kind == Kind::Track).cloned().collect();
        let target = &rows[idx].uri;
        let before = rows[..idx].iter().filter(|r| r.kind == Kind::Track).count();
        let pos = if tracks.get(before).map(|r| &r.uri) == Some(target) { before } else { 0 };
        self.ctx = if tracks.len() == rows.len() { rows.clone() } else { Arc::new(tracks) };
        self.reorder(Some(pos));
        self.skips = 0;
        let row = self.ctx[self.order[self.cursor]].clone();
        self.load(row);
    }

    fn peek_next(&self) -> Option<Row> {
        if self.repeat == Repeat::One {
            return self.now.clone();
        }
        if let Some(r) = self.queue.front() {
            return Some(r.clone());
        }
        let p = self.cursor + 1;
        if p < self.order.len() {
            Some(self.ctx[self.order[p]].clone())
        } else if self.repeat == Repeat::All && !self.order.is_empty() {
            Some(self.ctx[self.order[0]].clone())
        } else {
            None
        }
    }

    /// Move cursor/queue to the next track and return it (None = end of list).
    /// `auto` = track ended naturally (respects repeat-one).
    fn advance(&mut self, auto: bool) -> Option<Row> {
        if auto && self.repeat == Repeat::One {
            return self.now.clone();
        }
        if let Some(r) = self.queue.pop_front() {
            return Some(r);
        }
        if self.order.is_empty() {
            return None;
        }
        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
        } else if self.repeat == Repeat::All {
            self.cursor = 0;
        } else {
            self.status = "End of list".into();
            return None;
        }
        Some(self.ctx[self.order[self.cursor]].clone())
    }

    fn next(&mut self, auto: bool) {
        match self.advance(auto) {
            // Soloist already has it queued: skip to it rather than queue a duplicate.
            Some(r) if self.ahead.as_deref() == Some(r.uri.as_str()) => {
                self.ahead = None;
                self.engine.skip_next();
                self.awaiting = Some((r.uri.clone(), Instant::now()));
                self.pos_ms = 0;
                self.pos_at = Instant::now();
                self.now = Some(r);
            }
            Some(r) => self.load(r),
            None if auto => self.stop(),
            None => {}
        }
    }

    fn prev(&mut self) {
        if self.position_ms() > 3000 || self.order.is_empty() {
            self.seek_to(0);
            return;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
        } else if self.repeat == Repeat::All {
            self.cursor = self.order.len() - 1;
        }
        let row = self.ctx[self.order[self.cursor]].clone();
        self.load(row);
    }

    fn stop(&mut self) {
        self.engine.stop();
        self.playing = false;
        self.pos_ms = 0;
    }

    fn toggle(&mut self) {
        if let Some(pos) = self.reload_at.take()
            && let Some(row) = self.now.clone()
        {
            // Player was restarted while paused: bring the track back where it was.
            self.pending_seek = Some(pos);
            self.load(row);
            return;
        }
        if self.now.is_none() {
            // nothing loaded: play whatever is selected
            self.activate();
        } else if self.playing {
            self.engine.pause();
        } else {
            self.engine.resume();
        }
    }

    fn seek_to(&mut self, ms: u32) {
        if self.now.is_some() {
            self.engine.seek(ms);
            self.pos_ms = ms;
            self.pos_at = Instant::now();
        }
    }

    fn seek_by(&mut self, delta: i64) {
        let dur = self.now.as_ref().map(|r| r.duration_ms).unwrap_or(0) as i64;
        let to = (self.position_ms() as i64 + delta).clamp(0, (dur - 1000).max(0));
        self.seek_to(to as u32);
    }

    pub fn volume_pct(&self) -> u32 {
        self.vol
    }

    fn change_volume(&mut self, up: bool) {
        self.vol = if up { (self.vol + VOL_STEP).min(100) } else { self.vol.saturating_sub(VOL_STEP) };
        self.vol_set_at = Instant::now();
        self.engine.set_volume(self.vol);
        self.save_volume();
    }

    fn save_volume(&self) {
        if let Some(c) = self.session.cache() {
            c.save_volume((self.vol * u16::MAX as u32 / 100) as u16);
        }
    }

    /// Write the keyboard toggles (shuffle, repeat, sort, art) next to the EQ settings.
    pub(crate) fn save_prefs(&mut self) {
        self.audio.shuffle = self.shuffle;
        self.audio.repeat = self.repeat as u8;
        self.audio.sort_original = self.sort == Sort::Original;
        self.audio.art = self.art_on;
        self.audio.theme = self.theme;
        self.audio.tint = self.tint.as_ref().and_then(|t| t.1).or(self.audio.tint);
        self.audio.save();
    }

    /// Soloist autoplays a radio track the moment ours ends. To avoid that, we hand it our
    /// next track via its own queue ~8 s before the end; it then switches gaplessly.
    /// Returns when that should happen (None = nothing to do).
    pub fn end_deadline(&self) -> Option<Instant> {
        let lead_ms = 8_000 + self.audio.crossfade_s * 1000; // crossfade starts earlier
        if !self.engine.is_soloist()
            || !self.playing
            || self.awaiting.is_some()
            || self.ahead.is_some()
            || self.repeat == Repeat::One
        {
            return None;
        }
        let dur = self.now.as_ref()?.duration_ms;
        self.peek_next()?;
        let remaining = dur.saturating_sub(lead_ms).saturating_sub(self.position_ms());
        Some(Instant::now() + std::time::Duration::from_millis(remaining as u64))
    }

    pub fn on_end_deadline(&mut self) {
        if self.end_deadline().is_some_and(|d| d <= Instant::now() + std::time::Duration::from_millis(50))
            && let Some(r) = self.peek_next()
        {
            self.engine.add_to_queue(&r.uri);
            self.ahead = Some(r.uri);
        }
    }

    /// True when the current track is (about) finished — used to tell a natural end apart
    /// from the user changing tracks on another device.
    fn near_end(&self) -> bool {
        let dur = self.now.as_ref().map(|r| r.duration_ms).unwrap_or(0);
        dur > 0 && self.position_ms() + 5000 >= dur
    }

    /// Returns true if the UI needs a redraw.
    pub fn on_engine(&mut self, ev: EngineEvent) -> bool {
        if let Some((_, at)) = &self.awaiting
            && at.elapsed().as_secs() >= 10
        {
            self.awaiting = None; // never got confirmation; stop waiting
        }
        match ev {
            EngineEvent::Position { pos_ms, playing } => {
                if self.awaiting.is_some() {
                    return false; // belongs to the previous track
                }
                if let Some(p) = playing {
                    if p {
                        self.skips = 0;
                    }
                    self.playing = p;
                }
                self.pos_ms = pos_ms;
                self.pos_at = Instant::now();
            }
            EngineEvent::TrackChanged(row) => {
                if let Some((want, _)) = &self.awaiting {
                    if *want == row.uri {
                        if let Some(pos) = self.pending_seek.take()
                            && pos > 1000
                        {
                            self.engine.seek(pos);
                        }
                        if let Some(now) = self.now.as_mut()
                            && now.art.is_none()
                        {
                            now.art = row.art;
                        }
                        self.awaiting = None;
                        self.pos_ms = 0;
                        self.pos_at = Instant::now();
                    }
                    return false;
                }
                if let Some(now) = self.now.as_mut()
                    && now.uri == row.uri
                {
                    if now.art.is_none() && row.art.is_some() {
                        now.art = row.art;
                        return true;
                    }
                    return false;
                }
                if self.ahead.as_deref() == Some(row.uri.as_str()) {
                    // Soloist moved to the track we pre-queued: catch our state up.
                    self.ahead = None;
                    match self.advance(true) {
                        Some(r) if r.uri == row.uri => {
                            self.now = Some(r);
                            self.pos_ms = 0;
                            self.pos_at = Instant::now();
                        }
                        Some(r) => self.load(r), // plan changed since we queued it
                        None => self.stop(),
                    }
                } else if self.now.is_some() && self.near_end() {
                    // Soloist moved on by itself (autoplay) at the end of our track: play our next.
                    self.next(true);
                } else {
                    // Changed from another device (phone etc.): follow it.
                    self.now = Some(row);
                    self.pos_ms = 0;
                    self.pos_at = Instant::now();
                }
            }
            EngineEvent::Idle => {
                if self.awaiting.is_some() {
                    return false;
                }
                if self.now.is_some() && self.near_end() {
                    self.next(true);
                } else {
                    self.playing = false;
                }
            }
            EngineEvent::Stopped => self.playing = false,
            EngineEvent::PreloadNext => {
                if let Some(r) = self.peek_next() {
                    self.engine.preload(&r.uri);
                }
                return false;
            }
            EngineEvent::Ended => self.next(true),
            EngineEvent::Unavailable => {
                self.skips += 1;
                if self.skips > 15 {
                    self.stop();
                    self.status = "Too many unavailable tracks in a row; stopped. (CACHIFY_LOG=debug to see why)".into();
                } else {
                    self.status = format!(
                        "Unavailable: {}, skipping",
                        self.now.as_ref().map(|r| r.title.as_str()).unwrap_or("track")
                    );
                    self.next(true);
                }
            }
            EngineEvent::Volume(v) => {
                // Soloist echoes volume changes late; don't let an old echo undo a keypress.
                if self.vol_set_at.elapsed().as_millis() < 1500 || v.min(100) == self.vol {
                    return false;
                }
                self.vol = v.min(100); // changed from the phone / another Connect device
                self.save_volume();
            }
            EngineEvent::Status(m) | EngineEvent::Exited(m) => {
                if Instant::now() < self.quiet_until {
                    return false;
                }
                self.status = m;
            }
        }
        true
    }

    // ---------------------------------------------------------------- audio settings

    fn apply_eq(&mut self) {
        self.audio.save();
        match &self.eq {
            Some(eq) => {
                eq.apply(&self.audio.gains());
                self.status = format!("EQ: {}", self.audio.preset_name());
            }
            None if self.audio.is_flat() => self.status = format!("EQ: {}", self.audio.preset_name()),
            None if !self.eq_allowed => self.status = "EQ is disabled (eq = off in config)".into(),
            None if self.engine.is_soloist() => {
                // First non-flat setting: start the EQ and reroute the player (short restart).
                self.restart_at = Some(Instant::now() + std::time::Duration::from_millis(900));
                self.status = format!("EQ: {} (starting EQ…)", self.audio.preset_name());
            }
            None => self.status = format!("EQ: {} — restart cachify to apply", self.audio.preset_name()),
        }
    }

    /// The main loop should start the EQ process before the next player restart.
    pub fn eq_wanted(&self) -> bool {
        self.eq.is_none() && self.eq_allowed && !self.audio.is_flat()
    }

    fn cycle_crossfade(&mut self) {
        if !self.engine.is_soloist() {
            self.status = "Crossfade needs the Soloist engine".into();
            return;
        }
        self.audio.cycle_crossfade();
        self.audio.save();
        // debounce: tapping x a few times only restarts once
        self.restart_at = Some(Instant::now() + std::time::Duration::from_millis(1200));
        self.status = match self.audio.crossfade_s {
            0 => "Crossfade off (applying…)".into(),
            s => format!("Crossfade {s} s (applying…)"),
        };
    }

    pub fn restart_due(&self) -> bool {
        self.restart_at.is_some_and(|t| t <= Instant::now())
    }

    /// Returns what to resume after the player restarts.
    pub fn begin_restart(&mut self) -> Option<(Row, u32, bool)> {
        self.restart_at = None;
        self.awaiting = None;
        self.ahead = None; // the new process has an empty queue
        self.quiet_until = Instant::now() + std::time::Duration::from_secs(4);
        self.now.clone().map(|r| (r, self.position_ms(), self.playing))
    }

    pub fn finish_restart(&mut self, engine: Engine, resume: Option<(Row, u32, bool)>) {
        self.engine = engine;
        self.engine.set_repeat_track(self.repeat == Repeat::One);
        self.quiet_until = Instant::now() + std::time::Duration::from_secs(2);
        match resume {
            Some((row, pos, true)) => {
                self.pending_seek = Some(pos);
                self.load(row);
            }
            Some((_, pos, false)) => {
                self.reload_at = Some(pos);
                self.playing = false;
            }
            None => {}
        }
        let xfade = match self.audio.crossfade_s {
            0 => "crossfade off".to_string(),
            s => format!("crossfade {s} s"),
        };
        self.status = format!("EQ: {} · {xfade}", self.audio.preset_name());
    }

    fn on_eq_key(&mut self, band: usize, k: KeyEvent) {
        let n = BANDS.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('E') | KeyCode::Char('q') => {
                self.eq_panel = None;
            }
            KeyCode::Up | KeyCode::Char('k') => self.eq_panel = Some((band + n - 1) % n),
            KeyCode::Down | KeyCode::Char('j') => self.eq_panel = Some((band + 1) % n),
            KeyCode::Left | KeyCode::Char('h') => {
                self.audio.nudge(band, -1.0);
                self.apply_eq();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.audio.nudge(band, 1.0);
                self.apply_eq();
            }
            KeyCode::Char('0') => {
                self.audio.set_band(band, 0.0);
                self.apply_eq();
            }
            KeyCode::Char('p') | KeyCode::Char('e') | KeyCode::Tab => {
                self.audio.cycle_preset(false);
                self.apply_eq();
            }
            KeyCode::Char('P') | KeyCode::BackTab => {
                self.audio.cycle_preset(true);
                self.apply_eq();
            }
            KeyCode::Char('f') => {
                self.audio.preset = 0;
                self.apply_eq();
            }
            KeyCode::Char('x') => self.cycle_crossfade(),
            KeyCode::Char(' ') => self.toggle(),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- desktop (MPRIS)

    pub fn on_remote(&mut self, cmd: Remote) {
        match cmd {
            Remote::PlayPause => self.toggle(),
            Remote::Play => {
                if !self.playing {
                    self.toggle()
                }
            }
            Remote::Pause | Remote::Stop => {
                if self.playing {
                    self.engine.pause()
                }
            }
            Remote::Next => self.next(false),
            Remote::Prev => self.prev(),
            Remote::Seek(ms) => self.seek_by(ms),
            Remote::SetPosition(ms) => self.seek_to(ms),
            Remote::Volume(v) => {
                self.vol = v.min(100);
                self.vol_set_at = Instant::now();
                self.engine.set_volume(self.vol);
                self.save_volume();
            }
            Remote::Shuffle(on) => {
                if on != self.shuffle {
                    self.toggle_shuffle()
                }
            }
            Remote::Loop(r) => {
                self.repeat = r;
                self.save_prefs();
                self.engine.set_repeat_track(r == Repeat::One);
            }
        }
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.save_prefs();
        let cur = (!self.order.is_empty()).then(|| self.order[self.cursor]);
        self.reorder(cur);
        self.status = format!("Shuffle {}", if self.shuffle { "on" } else { "off" });
    }

    // ---------------------------------------------------------------- keys

    fn activate(&mut self) {
        let Some(i) = self.view.state.selected() else { return };
        let rows = self.view.rows.clone();
        let Some(row) = rows.get(i) else { return };
        if self.view.uri == "cachify:queue" {
            match self.queue_src.get(i).copied() {
                Some(QSrc::Now) => self.seek_to(0),
                Some(QSrc::Manual(q)) => {
                    if let Some(r) = self.queue.remove(q) {
                        self.load(r);
                    }
                }
                Some(QSrc::Upcoming(p)) => {
                    self.cursor = p;
                    let r = self.ctx[self.order[p]].clone();
                    self.load(r);
                }
                None => {}
            }
            self.show_queue();
            return;
        }
        match row.kind {
            Kind::Track => self.play_list(&rows, i),
            Kind::Album | Kind::Playlist | Kind::Artist => {
                let title = if row.artist.is_empty() { row.title.clone() } else { format!("{} — {}", row.title, row.artist) };
                self.open(row.uri.clone(), title, true, false);
            }
        }
    }

    fn enqueue(&mut self) {
        let Some(i) = self.view.state.selected() else { return };
        if let Some(r) = self.view.rows.get(i)
            && r.kind == Kind::Track
        {
            self.queue.push_back(r.clone());
            self.status = format!("Queued: {} ({} in queue)", r.title, self.queue.len());
            self.view.state.select(Some((i + 1).min(self.view.rows.len().saturating_sub(1))));
        }
    }

    fn move_sel(&mut self, delta: isize) {
        let (len, sel) = match self.focus {
            Focus::Side => (self.side.len(), self.side_state.selected()),
            _ => (self.view.rows.len(), self.view.state.selected()),
        };
        if len == 0 {
            return;
        }
        let cur = sel.unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        match self.focus {
            Focus::Side => self.side_state.select(Some(next)),
            _ => self.view.state.select(Some(next)),
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(band) = self.eq_panel {
            self.on_eq_key(band, k);
            return;
        }
        if self.chooser.is_some() {
            self.on_chooser_key(k);
            return;
        }
        if self.lyrics_open && self.focus != Focus::Input && self.on_lyrics_key(k) {
            return;
        }
        if self.focus == Focus::Input {
            match k.code {
                KeyCode::Esc => {
                    self.focus = Focus::Main;
                    self.input.clear();
                }
                KeyCode::Enter => {
                    let q = std::mem::take(&mut self.input);
                    self.focus = Focus::Main;
                    self.search(q);
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Side { Focus::Main } else { Focus::Side }
            }
            KeyCode::Char('h') => self.focus = Focus::Side,
            KeyCode::Char('l') => self.focus = Focus::Main,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(20),
            KeyCode::PageUp => self.move_sel(-20),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(isize::MAX / 2),
            KeyCode::Enter => {
                if self.focus == Focus::Side {
                    self.open_side()
                } else {
                    self.activate()
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some(v) = self.back.pop() {
                    self.view = v;
                    self.pending = None;
                    self.loading = false;
                } else {
                    self.focus = Focus::Side;
                }
            }
            KeyCode::Char('/') => {
                self.focus = Focus::Input;
                self.input.clear();
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('n') => self.next(false),
            KeyCode::Char('p') => self.prev(),
            KeyCode::Right | KeyCode::Char('.') => self.seek_by(SEEK_MS as i64),
            KeyCode::Left | KeyCode::Char(',') => self.seek_by(-(SEEK_MS as i64)),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(false),
            KeyCode::Char('a') => self.enqueue(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('o') => self.toggle_sort(),
            KeyCode::Char('t') => {
                self.theme = (self.theme + 1) % (crate::theme::COVER + 1);
                self.save_prefs();
                self.status = match self.theme {
                    crate::theme::COVER => "Theme: from the album art".into(),
                    i => format!("Theme: {}", crate::theme::name(i)),
                };
            }
            KeyCode::Char('e') => {
                self.audio.cycle_preset(false);
                self.apply_eq();
            }
            KeyCode::Char('E') => self.eq_panel = Some(0),
            KeyCode::Char('x') => self.cycle_crossfade(),
            KeyCode::Char('r') => {
                self.repeat = match self.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                self.save_prefs();
                self.engine.set_repeat_track(self.repeat == Repeat::One);
            }
            KeyCode::Char('c') => {
                // jump to the queue / now playing
                self.lyrics_open = false;
                if let Some(i) = self.side.iter().position(|s| s.kind == SideKind::Queue) {
                    self.side_state.select(Some(i));
                    self.back.clear();
                    self.show_queue();
                    self.focus = Focus::Main;
                }
            }
            KeyCode::Char('R') => {
                if self.focus == Focus::Side || self.view.uri.is_empty() {
                    self.status = "Reloading playlists…".into();
                    self.load_playlists();
                } else if !self.view.uri.starts_with("cachify:") {
                    let (u, t) = (self.view.uri.clone(), self.view.title.clone());
                    self.open(u, t, false, true);
                }
            }
            KeyCode::Char('f') => self.toggle_like(),
            KeyCode::Char('A') => self.open_chooser(),
            KeyCode::Char('D') => self.remove_from_playlist(),
            KeyCode::Char('L') => self.toggle_lyrics(),
            KeyCode::Char('i') => self.toggle_art(),
            KeyCode::Char('?') => {
                self.status = "↑↓/jk move · Tab/h/l pane · ⏎ play/open · space pause · n/p next/prev · ←→ seek · +/- vol · a queue · c queue view · f like · A add to playlist · D remove · L lyrics · i art · t theme · s shuffle · r repeat · o sort · e/E EQ · x crossfade · / search · Esc back · R reload · q quit".into();
            }
            _ => {}
        }
    }
}
