//! Application state, key handling and playback queue logic.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Instant, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use librespot_core::{Session, SpotifyUri};
use librespot_playback::{
    mixer::Mixer,
    player::{Player, PlayerEvent},
};
use ratatui::widgets::{ListState, TableState};
use tokio::sync::{Mutex, mpsc::UnboundedSender};

use crate::{
    api::{self, Kind, Row, Rows},
    auth::WebToken,
};

pub enum Msg {
    Playlists(Result<Vec<(String, String)>, String>),
    Rows { uri: String, title: String, rows: Result<Vec<Row>, String> },
    Search { query: String, rows: Result<(Vec<Row>, &'static str), String> },
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
}

pub struct App {
    pub session: Session,
    pub player: Arc<Player>,
    pub mixer: Arc<dyn Mixer>,
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
    req_id: Option<u64>,
    skips: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
    rng: u64,
}

const SEEK_MS: u32 = 10_000;
const VOL_STEP: u16 = u16::MAX / 20; // 5%

impl App {
    pub fn new(
        session: Session,
        player: Arc<Player>,
        mixer: Arc<dyn Mixer>,
        web: Option<Arc<Mutex<WebToken>>>,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        let user = session.username();
        let side = vec![
            SideItem { name: "♥ Liked Songs".into(), uri: format!("spotify:user:{user}:collection"), kind: SideKind::Liked },
            SideItem { name: "≡ Queue".into(), uri: "cachify:queue".into(), kind: SideKind::Queue },
            SideItem { name: "⌕ Search".into(), uri: "cachify:search".into(), kind: SideKind::Search },
        ];
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        let app = App {
            session,
            player,
            mixer,
            web,
            tx,
            side,
            side_state: ListState::default().with_selected(Some(0)),
            view: View { uri: String::new(), title: "cachify".into(), rows: Arc::new(Vec::new()), state: TableState::default() },
            back: Vec::new(),
            queue_src: Vec::new(),
            cache: HashMap::new(),
            pending: None,
            search_rows: Arc::new(Vec::new()),
            search_title: "Search".into(),
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
            req_id: None,
            skips: 0,
            shuffle: false,
            repeat: Repeat::Off,
            rng: seed,
        };
        app.load_playlists();
        app
    }

    // ---------------------------------------------------------------- data loading

    pub fn load_playlists(&self) {
        let (s, tx) = (self.session.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::Playlists(api::playlists(&s).await));
        });
    }

    fn open(&mut self, uri: String, title: String, push_back: bool, force: bool) {
        if push_back && !self.view.uri.is_empty() {
            let prev = std::mem::replace(
                &mut self.view,
                View { uri: String::new(), title: String::new(), rows: Arc::new(Vec::new()), state: TableState::default() },
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
        let sel = if rows.is_empty() { None } else { Some(0) };
        self.view = View { uri, title, rows, state: TableState::default().with_selected(sel) };
    }

    fn open_side(&mut self) {
        let Some(i) = self.side_state.selected() else { return };
        let item = &self.side[i];
        let (uri, name, kind) = (item.uri.clone(), item.name.clone(), item.kind);
        self.back.clear();
        match kind {
            SideKind::Queue => self.show_queue(),
            SideKind::Search => {
                let rows = self.search_rows.clone();
                self.show("cachify:search".into(), self.search_title.clone(), rows);
            }
            SideKind::Liked | SideKind::Playlist => self.open(uri, name, false, false),
        }
        self.focus = Focus::Main;
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
                self.side.truncate(3);
                let n = list.len();
                self.side.extend(list.into_iter().map(|(uri, name)| SideItem { name, uri, kind: SideKind::Playlist }));
                self.status = format!("{n} playlists. Tab to switch panes, / to search, ? for keys.");
            }
            Msg::Playlists(Err(e)) => self.status = format!("Couldn't load playlists: {e}  (R to retry)"),
            Msg::Rows { uri, title, rows } => {
                let is_current = self.pending.as_deref() == Some(uri.as_str());
                match rows {
                    Ok(rows) => {
                        let rows: Rows = Arc::new(rows);
                        self.cache.insert(uri.clone(), rows.clone());
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
        let Ok(uri) = SpotifyUri::from_uri(&row.uri) else {
            self.status = format!("Can't play {}", row.uri);
            return;
        };
        self.player.load(uri, true, 0);
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

    /// Advance. `auto` = track ended naturally (respects repeat-one).
    fn next(&mut self, auto: bool) {
        if auto && self.repeat == Repeat::One && let Some(now) = self.now.clone() {
            self.load(now);
            return;
        }
        if let Some(r) = self.queue.pop_front() {
            self.load(r);
            return;
        }
        if self.order.is_empty() {
            self.stop();
            return;
        }
        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
        } else if self.repeat == Repeat::All {
            self.cursor = 0;
        } else {
            self.status = "End of list".into();
            if auto {
                self.stop();
            }
            return;
        }
        let row = self.ctx[self.order[self.cursor]].clone();
        self.load(row);
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
        self.player.stop();
        self.playing = false;
        self.pos_ms = 0;
    }

    fn toggle(&mut self) {
        if self.now.is_none() {
            // nothing loaded: play whatever is selected
            self.activate();
        } else if self.playing {
            self.player.pause();
        } else {
            self.player.play();
        }
    }

    fn seek_to(&mut self, ms: u32) {
        if self.now.is_some() {
            self.player.seek(ms);
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
        (self.mixer.volume() as u32 * 100 + u16::MAX as u32 / 2) / u16::MAX as u32
    }

    fn change_volume(&mut self, up: bool) {
        let v = self.mixer.volume();
        let v = if up { v.saturating_add(VOL_STEP) } else { v.saturating_sub(VOL_STEP) };
        self.mixer.set_volume(v);
        if let Some(c) = self.session.cache() {
            c.save_volume(v);
        }
    }

    /// Returns true if the UI needs a redraw.
    pub fn on_player(&mut self, ev: PlayerEvent) -> bool {
        use PlayerEvent::*;
        let ours = |id: u64, me: &Self| me.req_id.is_none_or(|r| r == id);
        match ev {
            PlayRequestIdChanged { play_request_id } => {
                self.req_id = Some(play_request_id);
                false
            }
            Playing { play_request_id, position_ms, .. } if ours(play_request_id, self) => {
                self.playing = true;
                self.skips = 0;
                self.pos_ms = position_ms;
                self.pos_at = Instant::now();
                true
            }
            Paused { play_request_id, position_ms, .. } if ours(play_request_id, self) => {
                self.playing = false;
                self.pos_ms = position_ms;
                true
            }
            Seeked { play_request_id, position_ms, .. } | PositionCorrection { play_request_id, position_ms, .. }
                if ours(play_request_id, self) =>
            {
                self.pos_ms = position_ms;
                self.pos_at = Instant::now();
                true
            }
            Stopped { play_request_id, .. } if ours(play_request_id, self) => {
                self.playing = false;
                true
            }
            TimeToPreloadNextTrack { play_request_id, .. } if ours(play_request_id, self) => {
                if let Some(r) = self.peek_next()
                    && let Ok(u) = SpotifyUri::from_uri(&r.uri)
                {
                    self.player.preload(u);
                }
                false
            }
            EndOfTrack { play_request_id, .. } if ours(play_request_id, self) => {
                self.next(true);
                true
            }
            Unavailable { play_request_id, .. } if ours(play_request_id, self) => {
                self.skips += 1;
                if self.skips > 15 {
                    self.stop();
                    self.status = "Too many unavailable tracks in a row; stopped.".into();
                } else {
                    self.status = format!(
                        "Unavailable: {}, skipping",
                        self.now.as_ref().map(|r| r.title.as_str()).unwrap_or("track")
                    );
                    self.next(true);
                }
                true
            }
            _ => false,
        }
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
            KeyCode::Char('s') => {
                self.shuffle = !self.shuffle;
                let cur = (!self.order.is_empty()).then(|| self.order[self.cursor]);
                self.reorder(cur);
                self.status = format!("Shuffle {}", if self.shuffle { "on" } else { "off" });
            }
            KeyCode::Char('r') => {
                self.repeat = match self.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
            }
            KeyCode::Char('c') => {
                // jump to the queue / now playing
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
            KeyCode::Char('?') => {
                self.status = "↑↓/jk move · Tab/h/l pane · ⏎ play/open · space pause · n/p next/prev · ←→ seek · +/- vol · a queue · c queue view · s shuffle · r repeat · / search · Esc back · R reload · q quit".into();
            }
            _ => {}
        }
    }
}
