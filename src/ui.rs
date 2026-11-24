//! Rendering. Called only when something changed (or once a second while playing).

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Clear, List, ListItem, Paragraph, Row as TRow, Table, TableState},
};

use ratatui::layout::Rect;
use ratatui_image::Image;

use crate::{
    api::{self, Kind},
    app::{App, Focus, InputMode, LyricsState, Repeat},
    theme::{self, accent, grad, on_accent},
    audio::{BANDS, MAX_DB},
};


fn fmt_ms(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// 1204589367 -> "1,204,589,367", as Spotify shows play counts.
fn fmt_plays(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// ms since epoch -> "YYYY-MM-DD" in the local timezone.
fn fmt_date(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r / localtime_s only write into the tm we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    #[cfg(windows)]
    let ok = unsafe { libc::localtime_s(&mut tm, &t) } == 0;
    if !ok {
        return String::new();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// figlet "Slant", the same logo as the README; shown above the library when there's room.
const LOGO: [&str; 5] = [
    "   __       _ ____",
    "  / /______(_) / /",
    " / __/ ___/ / / / ",
    "/ /_/ /  / / / /  ",
    "\\__/_/  /_/_/_/   ",
];

/// `rows` with the gradient across it, if it fits `width`.
fn logo(rows: &[&str], width: u16) -> Option<Vec<Line<'static>>> {
    if rows[0].chars().count() as u16 > width {
        return None;
    }
    Some(
        rows.iter()
            .map(|row| {
                let n = row.chars().count().max(2) as f32 - 1.0;
                let spans: Vec<Span> = row
                    .chars()
                    .enumerate()
                    .map(|(i, c)| Span::styled(c.to_string(), Style::new().fg(grad(i as f32 / n)).bold()))
                    .collect();
                Line::from(spans).centered()
            })
            .collect(),
    )
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let b = Block::bordered().border_type(BorderType::Rounded).title(format!(" {title} "));
    if focused { b.border_style(Style::new().fg(accent())) } else { b.border_style(Style::new().dim()) }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    theme::set(theme::colors(app.theme, app.tint.as_ref().and_then(|t| t.1)));
    let [body, now, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(2), Constraint::Length(1)]).areas(f.area());
    // Narrow terminals: the sidebar only takes room while it has focus (Tab shows it).
    let side_w = if f.area().width < 70 && app.focus != Focus::Side { 0 } else { (f.area().width / 4).clamp(18, 32) };
    let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).areas(body);

    // sidebar, with the cover under it when there's room, framed like the panels so its
    // edges line up with theirs (a picture can't start mid-cell the way a border line does)
    let art_rows = art_rows(app, side);
    app.art_want = art_rows.map(|r| (side.width - 2, r));
    let (side, art_area) = match art_rows {
        Some(r) => {
            let [s, a] = Layout::vertical([Constraint::Min(6), Constraint::Length(r + 2)]).areas(side);
            let frame = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().dim());
            let inner = frame.inner(a);
            f.render_widget(frame, a);
            (s, Some(inner))
        }
        None => (side, None),
    };
    let side = match logo(&LOGO, side.width).filter(|l| side.height >= l.len() as u16 + 13) {
        Some(l) => {
            let [a, s] = Layout::vertical([Constraint::Length(l.len() as u16), Constraint::Min(6)]).areas(side);
            f.render_widget(Paragraph::new(l), a);
            s
        }
        None => side,
    };
    let items: Vec<ListItem> = app
        .side
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::styled(if s.pinned { "⚲ " } else { "" }, Style::new().fg(accent())),
                Span::styled(if s.uri.starts_with("spotify:folder:") { "▸ " } else { "" }, Style::new().fg(Color::Yellow)),
                Span::raw(s.name.as_str()),
            ]))
        })
        .collect();
    let hl = if app.focus == Focus::Side { Style::new().fg(on_accent()).bg(accent()) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let list = List::new(items).block(block("Library", app.focus == Focus::Side)).highlight_style(hl);
    f.render_stateful_widget(list, side, &mut app.side_state);
    (app.side_area, app.main_area) = (side, main);
    if app.lyrics_open {
        draw_lyrics(f, app, main);
    } else {
        draw_table(f, app, main);
    }
    draw_now(f, app, now);

    app.popup_rows = (Rect::default(), 0);
    let eq = app.eq_panel.map(|band| draw_eq(f, app, band));
    let chooser = draw_chooser(f, app);
    app.popup_area = chooser.or(eq).unwrap_or_default();
    let popups = [eq, chooser, app.help_open.then(|| draw_help(f))];
    // Terminal graphics (kitty placeholders in Ghostty at least) drop every image row that
    // anything else is drawn across, so a popup over the cover hides it whole instead.
    if let (Some(area), Some(proto)) = (art_area, app.art.as_ref().and_then(|a| a.proto.as_ref()))
        && !popups.iter().flatten().any(|p| p.intersects(area))
    {
        let sz = proto.size();
        let x = area.x + area.width.saturating_sub(sz.width) / 2;
        f.render_widget(Image::new(proto), Rect { x, y: area.y, width: sz.width.min(area.width), height: sz.height.min(area.height) });
    }

    // bottom line: search input or status
    if app.focus == Focus::Input {
        let text = match app.input_mode {
            InputMode::Search => format!(" Search: {}", app.input),
            InputMode::Find => format!(" Find in {}: {}", app.view.title, app.input),
        };
        let x = bottom.x + text.chars().count() as u16;
        f.render_widget(Paragraph::new(text).fg(accent()), bottom);
        f.set_cursor_position((x.min(bottom.right().saturating_sub(1)), bottom.y));
    } else {
        f.render_widget(Paragraph::new(format!(" {}", app.status)).dim(), bottom);
        if let Some(tag) = &app.update {
            let note = format!("trill {tag} is out: quit and run trill --update ");
            let w = (note.chars().count() as u16).min(bottom.width);
            let area = Rect { x: bottom.right() - w, width: w, ..bottom };
            f.render_widget(Paragraph::new(note).fg(accent()), area);
        }
    }
}

/// First frame while logging in (~0.4 s): the same layout with the cached library, so the
/// real UI replaces it without a jump. Keys typed meanwhile wait in stdin.
pub fn draw_splash(f: &mut Frame, playlists: &[String]) {
    let [body, _now, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(2), Constraint::Length(1)]).areas(f.area());
    let side_w = if f.area().width < 70 { 0 } else { (f.area().width / 4).clamp(18, 32) };
    let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).areas(body);
    let side = match logo(&LOGO, side.width).filter(|l| side.height >= l.len() as u16 + 13) {
        Some(l) => {
            let [a, s] = Layout::vertical([Constraint::Length(l.len() as u16), Constraint::Min(6)]).areas(side);
            f.render_widget(Paragraph::new(l), a);
            s
        }
        None => side,
    };
    let fixed = ["♥ Liked Songs", "≡ Queue", "⌕ Search"];
    let items: Vec<ListItem> = fixed.iter().copied().chain(playlists.iter().map(String::as_str)).map(ListItem::new).collect();
    f.render_widget(List::new(items).block(block("Library", false)), side);
    f.render_widget(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().dim()).title(" trill "), main);
    f.render_widget(Paragraph::new(" Connecting…").dim(), bottom);
}

/// Rows for a square cover inside a frame the width of the sidebar (rounded, so the cover
/// fills it with the least cropped), if the sidebar is tall enough to still show a useful
/// list above it.
fn art_rows(app: &App, side: Rect) -> Option<u16> {
    let picker = app.img_picker.as_ref()?;
    if !app.art_on || app.now.as_ref()?.art.is_none() || side.width < 3 {
        return None;
    }
    let fs = picker.font_size();
    let (w, fh) = ((side.width - 2) as u32 * fs.width as u32, fs.height.max(1) as u32);
    let rows = ((w + fh / 2) / fh) as u16;
    (rows >= 4 && side.height >= rows + 12).then_some(rows)
}

fn draw_table(f: &mut Frame, app: &mut App, main: Rect) {

    // main table
    let now_uri = app.now.as_ref().map(|r| r.uri.as_str());
    let rows = app.view.rows.clone();
    // Only build widgets for the rows that fit on screen (lists can have thousands of tracks).
    let visible = main.height.saturating_sub(3).max(1) as usize; // 2 borders + header
    let sel = app.view.state.selected().unwrap_or(0).min(rows.len().saturating_sub(1));
    let mut off = app.view.state.offset().min(rows.len().saturating_sub(1));
    if sel < off {
        off = sel;
    } else if sel >= off + visible {
        off = sel + 1 - visible;
    }
    *app.view.state.offset_mut() = off;
    let end = (off + visible).min(rows.len());
    let mut window_state = TableState::default().with_selected((!rows.is_empty()).then_some(sel - off));
    // Columns drop out as the pane narrows: dates/blend members, then album, then plays, then
    // artist (which then trails the title). A column that only repeats what the title bar
    // already says (an album's own name and artist) isn't shown at all.
    let w = main.width;
    let repeats_title = |col: fn(&api::Row) -> &str| {
        let first = rows.first().map_or("", col);
        !first.is_empty() && app.view.title.contains(first) && rows.iter().all(|r| col(r) == first)
    };
    let shown = &rows[off..end];
    let show_artist = w >= 45 && !repeats_title(|r| &r.artist);
    let show_album = w >= 75 && !repeats_title(|r| &r.album);
    let show_extra = w >= 100;
    let plays_w = if w >= 60 { shown.iter().map(|r| r.plays).max().filter(|&n| n > 0).map_or(0, |n| fmt_plays(n).len() as u16) } else { 0 };
    // Blends: who each song is for (instead of "Added", which is the same for every song there).
    let for_w = if show_extra { shown.iter().map(|r| r.members.chars().count()).max().unwrap_or(0).min(28) as u16 } else { 0 };
    let show_added = show_extra && for_w == 0 && shown.iter().any(|r| r.added_at > 0);
    let time_w = if shown.iter().any(|r| r.duration_ms >= 6_000_000) { 6 } else { 5 }; // "100:00" vs "4:03"
    let in_queue = app.view.uri == "trill:queue";
    let titles: Vec<Line> = shown
        .iter()
        .map(|r| {
            let (tag, tag_color) = match r.kind {
                // smart-shuffle recommendations, kept out of the row so likes and adds get the plain title
                Kind::Track if in_queue && app.is_rec(&r.uri) => ("✦ ", accent()),
                Kind::Track => ("", Color::Reset),
                Kind::Album => ("album ", Color::Cyan),
                Kind::Playlist => ("playlist ", Color::Magenta),
                Kind::Artist => ("artist ", Color::Yellow),
            };
            let heart = if app.liked.contains(&r.uri) { "♥ " } else { "" };
            Line::from(vec![
                Span::styled(heart, Style::new().fg(accent())),
                Span::styled(tag, Style::new().fg(tag_color)),
                Span::raw(r.title.as_str()),
                Span::styled(if show_artist || r.artist.is_empty() { String::new() } else { format!(" · {}", r.artist) }, Style::new().dim()),
            ])
        })
        .collect();

    // Text columns are as wide as their longest value (artist and album at most 30% of the
    // room each), and what's left over sits between them and the dates/numbers on the right,
    // so a wide window doesn't push each column into a gap of its own.
    let need = |col: fn(&api::Row) -> &str, head: usize| shown.iter().map(|r| col(r).chars().count()).max().unwrap_or(0).max(head) as u16;
    let fixed = [(show_added, 10), (for_w > 0, for_w.max(3)), (plays_w > 0, plays_w.max(5)), (true, time_w)];
    let cols = 3 + show_artist as u16 + show_album as u16 + fixed.iter().filter(|c| c.0).count() as u16; // + title, spacer
    let free = w.saturating_sub(2 + (cols - 1) + fixed.iter().filter(|c| c.0).map(|c| c.1).sum::<u16>());
    let artist_w = if show_artist { need(|r| &r.artist, 6).min(free * 3 / 10) } else { 0 };
    let album_w = if show_album { need(|r| &r.album, 5).min(free * 3 / 10) } else { 0 };
    let title_w = (titles.iter().map(Line::width).max().unwrap_or(0) as u16).max(5).min(free.saturating_sub(artist_w + album_w));

    let trows = shown.iter().zip(titles).map(|(r, title)| {
        let dur = if r.duration_ms > 0 { fmt_ms(r.duration_ms) } else { String::new() };
        let mut cells = vec![Cell::from(title)];
        if show_artist {
            cells.push(Cell::from(r.artist.as_str()));
        }
        if show_album {
            cells.push(Cell::from(r.album.as_str()));
        }
        cells.push(Cell::from(""));
        if show_added {
            cells.push(Cell::from(if r.added_at > 0 { fmt_date(r.added_at) } else { String::new() }).style(Style::new().dim()));
        }
        if for_w > 0 {
            cells.push(Cell::from(r.members.as_str()).style(Style::new().fg(Color::Magenta)));
        }
        if plays_w > 0 {
            cells.push(Cell::from(Line::from(if r.plays > 0 { fmt_plays(r.plays) } else { String::new() }).right_aligned()).style(Style::new().dim()));
        }
        cells.push(Cell::from(Line::from(dur).right_aligned()));
        let mut row = TRow::new(cells);
        if Some(r.uri.as_str()) == now_uri {
            row = row.style(Style::new().fg(accent()).bold());
        }
        row
    });
    let hl = if app.focus == Focus::Main { Style::new().fg(on_accent()).bg(accent()) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let mut title = app.view.title.clone();
    if let Some(s) = app.view.sort {
        title.push_str(" · ");
        title.push_str(s);
    }
    if !app.view.find.is_empty() {
        title.push_str(&format!(" · “{}” {} of {}", app.view.find, app.view.rows.len(), app.view.all.len()));
    }
    if app.loading {
        title.push_str(" (loading…)");
    }
    let mut widths = vec![Constraint::Length(title_w)];
    let mut header = vec![Cell::from("Title")];
    for (on, wd, h) in [
        (show_artist, Constraint::Length(artist_w), Line::from("Artist")),
        (show_album, Constraint::Length(album_w), Line::from("Album")),
        (true, Constraint::Fill(1), Line::default()),
        (show_added, Constraint::Length(10), Line::from("Added")),
        (for_w > 0, Constraint::Length(for_w.max(3)), Line::from("For")),
        (plays_w > 0, Constraint::Length(plays_w.max(5)), Line::from("Plays").right_aligned()),
        (true, Constraint::Length(time_w), Line::from("Time").right_aligned()),
    ] {
        if on {
            widths.push(wd);
            header.push(Cell::from(h));
        }
    }
    let table = Table::new(trows, widths)
    .header(TRow::new(header).style(Style::new().dim()))
    .block(block(&title, app.focus == Focus::Main))
    .row_highlight_style(hl)
    .column_spacing(1);
    f.render_stateful_widget(table, main, &mut window_state);
}

/// The right side of the now-playing line: what has a value as short text (only when it
/// matters), then the toggles as fixed icons, lit when on and dimmed when off, so each one's
/// state is always visible: ⇄ shuffle (⇄✦ smart), ↻ repeat (↻1 one), ⊘ private session.
fn now_flags(app: &App) -> Line<'static> {
    let info = Style::new().dim();
    let mut s = Vec::new();
    if app.eq.is_some() && !app.audio.is_flat() {
        s.push(Span::styled(format!("eq {}  ", app.audio.preset_name().to_lowercase()), info));
    }
    if let Some(t) = &app.sleep {
        let left = match t.until {
            Some(u) => format!("{}m", u.saturating_duration_since(std::time::Instant::now()).as_secs().div_ceil(60)),
            None => "end".into(),
        };
        s.push(Span::styled(format!("sleep {left}  "), Style::new().fg(accent())));
    }
    if app.audio.crossfade_s > 0 {
        s.push(Span::styled(format!("xfade {}s  ", app.audio.crossfade_s), info));
    }
    if !app.queue.is_empty() {
        s.push(Span::styled(format!("queue {}  ", app.queue.len()), info));
    }
    s.push(Span::styled(format!("vol {}%  ", app.volume_pct()), info));
    // on: a solid accent pill; off: the same cell, dim, so nothing shifts when it toggles
    let flag = |lit: bool, icon: &str| {
        let style = if lit { Style::new().fg(Color::Black).bg(accent()).bold() } else { Style::new().fg(Color::DarkGray) };
        Span::styled(format!(" {icon} "), style)
    };
    s.push(flag(app.shuffle, if app.shuffle && app.smart { "⇄✦" } else { "⇄" }));
    s.push(flag(app.repeat != Repeat::Off, if app.repeat == Repeat::One { "↻1" } else { "↻" }));
    s.push(flag(app.private_on(), "⊘"));
    Line::from(s)
}

/// Where the last three spans of `flags` (the ⇄ ↻ ⊘ icons) land when right-aligned in `area`.
/// shortcut: none when the flags don't fit (narrow terminals); clicks there do nothing.
fn flag_areas(flags: &Line, area: Rect) -> [Rect; 3] {
    let mut out = [Rect::default(); 3];
    if flags.width() <= area.width as usize {
        let mut x = area.right();
        for (r, s) in out.iter_mut().rev().zip(flags.spans.iter().rev()) {
            let w = s.width() as u16;
            x -= w;
            *r = Rect { x, width: w, height: 1, ..area };
        }
    }
    out
}

fn draw_now(f: &mut Frame, app: &mut App, now: Rect) {
    app.bar_area = Rect::default();
    let [line1, line2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(now);
    let flags = now_flags(app);
    match &app.now {
        Some(r) => {
            let icon = if app.playing { "▶" } else { "⏸" };
            let heart = match (app.liked.contains(&r.uri), app.is_rec(&r.uri)) {
                (true, _) => " ♥",
                (false, true) => " ✦",
                _ => "",
            };
            // the flags keep their full width; the title gives way first
            let [l, rt] = Layout::horizontal([Constraint::Min(10), Constraint::Length((flags.width() as u16).min(line1.width / 2))]).areas(line1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::new().fg(accent()).bold()),
                    Span::styled(r.title.as_str(), Style::new().fg(accent()).bold()),
                    Span::styled(heart, Style::new().fg(accent())),
                    Span::raw("  "),
                    Span::styled(r.artist.as_str(), Style::new().dim()),
                ])),
                l,
            );
            app.flag_areas = flag_areas(&flags, rt);
            f.render_widget(Paragraph::new(flags).right_aligned(), rt);
            let pos = app.position_ms().min(r.duration_ms);
            let ratio = if r.duration_ms > 0 { pos as f64 / r.duration_ms as f64 } else { 0.0 };
            // " 1:23 ██████████▌░░░░░░░ 3:45 ": solid bar, 1/8-cell precision, gradient fill
            let (ta, tb) = (format!(" {} ", fmt_ms(pos)), format!(" {} ", fmt_ms(r.duration_ms)));
            let bar = (line2.width as usize).saturating_sub(ta.len() + tb.len());
            app.bar_area = Rect { x: line2.x + ta.len() as u16, width: bar as u16, ..line2 };
            let eighths = (ratio.clamp(0.0, 1.0) * (bar * 8) as f64) as usize;
            let track = Color::Rgb(55, 55, 60);
            let n = bar.max(2) as f32 - 1.0;
            let fill = |i: usize| if app.playing { grad(i as f32 / n) } else { Color::Rgb(120, 120, 120) };
            let mut spans = vec![Span::styled(ta, Style::new().dim())];
            spans.extend((0..bar).map(|i| match (eighths / 8).cmp(&i) {
                std::cmp::Ordering::Greater => Span::styled("█", Style::new().fg(fill(i))),
                std::cmp::Ordering::Equal if !eighths.is_multiple_of(8) => {
                    Span::styled(["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][eighths % 8], Style::new().fg(fill(i)).bg(track))
                }
                _ => Span::styled(" ", Style::new().bg(track)),
            }));
            spans.push(Span::styled(tb, Style::new().dim()));
            f.render_widget(Paragraph::new(Line::from(spans)), line2);
        }
        None => {
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" ■ Nothing playing").dim()])), line1);
            app.flag_areas = flag_areas(&flags, line1);
            f.render_widget(Paragraph::new(flags).right_aligned(), line1);
        }
    }
}

fn draw_lyrics(f: &mut Frame, app: &App, area: Rect) {
    let Some(now) = app.now.as_ref() else {
        let p = Paragraph::new("\n Play something to see its lyrics.  (L or Esc to close)").dim();
        f.render_widget(p.block(block("Lyrics", true)), area);
        return;
    };
    let title = format!("Lyrics · {} — {}", now.title, now.artist);
    let mut blk = block(&title, true);
    let slot = app.lyrics.as_ref().filter(|l| l.uri == now.uri);
    let msg = match slot.map(|l| &l.state) {
        None | Some(LyricsState::Loading) => Some("Loading lyrics…".to_string()),
        Some(LyricsState::Missing) => Some("Spotify has no lyrics for this song.".to_string()),
        Some(LyricsState::Failed(e)) => Some(format!("Couldn't load lyrics: {e}")),
        Some(LyricsState::Ready(_)) => None,
    };
    if let Some(m) = msg {
        f.render_widget(Paragraph::new(format!("\n {m}")).dim().block(blk), area);
        return;
    }
    let (Some(slot), Some(LyricsState::Ready(l))) = (slot, slot.map(|l| &l.state)) else { return };
    if !l.provider.is_empty() {
        blk = blk.title_bottom(Line::from(format!(" {} ", l.provider)).dim().right_aligned());
    }
    let inner = blk.inner(area);
    f.render_widget(blk, area);
    let h = inner.height as usize;
    let cur = l.current(app.position_ms());
    // Synced: keep the current line a third of the way down. Unsynced: manual scroll.
    let start = match cur {
        Some(c) => c.saturating_sub(h / 3),
        None if l.synced => 0,
        None => slot.scroll,
    };
    let lines: Vec<Line> = l
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, (_, text))| {
            let style = match cur {
                Some(c) if i == c => Style::new().fg(accent()).bold(),
                Some(c) if i < c => Style::new().dim(),
                _ => Style::new(),
            };
            Line::styled(text.as_str(), style).centered()
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_chooser(f: &mut Frame, app: &mut App) -> Option<Rect> {
    let c = app.chooser.as_ref()?;
    let area = f.area();
    let w = 50.min(area.width.saturating_sub(2));
    let h = 18.min(area.height.saturating_sub(2));
    let rect = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let blk = block(if c.checking { "Add to playlist · checking…" } else { "Add to playlist" }, true);
    let inner = blk.inner(rect);
    f.render_widget(blk, rect);
    let [song, filter, list, help] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(format!(" “{}” — {}", c.track.title, c.track.artist)).fg(accent()).bold(), song);
    f.render_widget(Paragraph::new(format!(" filter: {}▏", c.filter)).fg(accent()), filter);
    let matches = c.matches(app);
    let visible = list.height as usize;
    let off = c.sel.saturating_sub(visible.saturating_sub(1));
    let items: Vec<ListItem> = matches.iter().skip(off).take(visible).map(|(u, n)| ListItem::new(format!(" {} {n}", if c.holding.contains(*u) { "✓" } else { " " }))).collect();
    let mut st = ratatui::widgets::ListState::default().with_selected((!matches.is_empty()).then_some(c.sel - off));
    let l = if matches.is_empty() { List::new(vec![ListItem::new(" no matching playlists").dim()]) } else { List::new(items) };
    f.render_stateful_widget(l.highlight_style(Style::new().fg(on_accent()).bg(accent())), list, &mut st);
    app.popup_rows = (list, off);
    f.render_widget(Paragraph::new(" type to filter · ⏎ add / remove ✓ · Esc done").dim(), help);
    Some(rect)
}


/// Every key, grouped; what `?` shows. Keep in step with the README's key table.
const HELP: [(&str, &[(&str, &str)]); 4] = [
    ("Playback", &[
        ("Space", "play / pause"),
        ("n  p", "next / previous"),
        ("← →  , .", "seek ±10 s"),
        ("+  -", "volume ±5%"),
        ("s", "shuffle ⇄: off → on → smart ⇄✦"),
        ("r", "repeat ↻: off → all → one ↻1"),
        ("x", "crossfade off / 3 / 5 / 8 / 12 s"),
        ("z", "sleep timer: 15…90 min, song end"),
        ("P", "play everything in the list"),
    ]),
    ("Library", &[
        ("f", "like · save album · follow artist"),
        ("a", "add to queue"),
        ("A", "add to / remove from a playlist"),
        ("D D", "remove from the playlist · queue"),
        ("y  Y", "copy link: highlighted · open list"),
        ("I", "private session ⊘ on / off (6 h)"),
    ]),
    ("Browse", &[
        ("↑↓  jk", "move (PgUp/PgDn, g/G jump)"),
        ("Tab  h  l", "switch pane"),
        ("Enter", "play / open"),
        ("Esc  ⌫", "back"),
        ("/", "search (↑↓ past searches)"),
        ("F", "find in this list"),
        ("o  O", "sort this list: next / previous"),
        ("c  w", "show the queue · the playing song"),
        ("u  U", "open the song's album · artist"),
        ("R", "reload the current list"),
    ]),
    ("View", &[
        ("L", "lyrics"),
        ("i  t", "album art on / off · next theme"),
        ("e  E", "next EQ preset / EQ panel"),
        ("?  q", "this help · quit (also Ctrl-C)"),
    ]),
];

fn help_lines(groups: &[(&str, &[(&str, &str)])]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (name, keys)) in groups.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(name.to_string(), Style::new().fg(accent()).bold())));
        for (k, what) in keys.iter() {
            lines.push(Line::from(vec![Span::styled(format!(" {k:<11}"), Style::new().bold()), Span::raw(what.to_string())]));
        }
    }
    lines
}

/// `?`: all the keys in a centred pane, two columns when there's room.
fn draw_help(f: &mut Frame) -> Rect {
    let area = f.area();
    let two = area.width >= 100;
    let (left, right) = if two { (help_lines(&HELP[..2]), help_lines(&HELP[2..])) } else { (help_lines(&HELP), Vec::new()) };
    let w = if two { 96 } else { 50 }.min(area.width.saturating_sub(2));
    let h = (left.len().max(right.len()) as u16 + 3).min(area.height.saturating_sub(2));
    let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let blk = block("Keys · any key closes", true);
    let inner = blk.inner(rect);
    f.render_widget(blk, rect);
    if two {
        let [l, r] = ratatui::layout::Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(inner);
        f.render_widget(Paragraph::new(left), l);
        f.render_widget(Paragraph::new(right), r);
    } else {
        f.render_widget(Paragraph::new(left), inner);
    }
    rect
}

fn draw_eq(f: &mut Frame, app: &mut App, sel: usize) -> Rect {
    let area = f.area();
    let w = 62.min(area.width.saturating_sub(2));
    let h = (BANDS.len() as u16 + 7).min(area.height.saturating_sub(2));
    let rect = ratatui::layout::Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let xfade = match app.audio.crossfade_s {
        0 => "off".to_string(),
        s => format!("{s} s"),
    };
    let title = format!(" Equalizer · {} · crossfade {xfade} ", app.audio.preset_name());
    let blk = block(&title, true);
    let inner = blk.inner(rect);
    f.render_widget(blk, rect);

    let gains = app.audio.gains();
    let bar_w = inner.width.saturating_sub(18).max(10) as i32; // "16k Hz ▕…▏ +12 dB"
    let half = bar_w / 2;
    let mut lines: Vec<Line> = Vec::new();
    if !app.eq_allowed {
        lines.push(Line::from(" EQ is disabled (eq = off in config)").fg(Color::Yellow));
    } else if app.eq.is_none() && !app.audio.is_flat() {
        lines.push(Line::from(" Starting EQ… (the player restarts once)").fg(Color::Yellow));
    }
    let first = inner.y + lines.len() as u16;
    app.popup_rows = (Rect { y: first, height: BANDS.len() as u16, ..inner }.intersection(inner), 0);
    for (i, (&hz, &g)) in BANDS.iter().zip(gains.iter()).enumerate() {
        let label = if hz >= 1000 { format!("{}k", hz / 1000) } else { hz.to_string() };
        let fill = ((g / MAX_DB) * half as f32).round() as i32;
        let mut bar = String::with_capacity(bar_w as usize * 3);
        for x in 0..bar_w {
            let c = x - half;
            bar.push(if c == 0 {
                '│'
            } else if (fill > 0 && c > 0 && c <= fill) || (fill < 0 && c < 0 && c >= fill) {
                '█'
            } else {
                '·'
            });
        }
        let style = if i == sel { Style::new().fg(on_accent()).bg(accent()) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:>4} Hz "), style),
            Span::styled(bar, if i == sel { Style::new().fg(accent()) } else { Style::new().dim() }),
            Span::styled(format!(" {g:+3.0} dB"), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(" ↑↓ band  ←→ ±1 dB  0 reset band  p/P preset  f flat").dim());
    lines.push(Line::from(" x crossfade  space play/pause  Esc close").dim());
    f.render_widget(Paragraph::new(lines), inner);
    rect
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    unsafe extern "C" {
        fn tzset();
    }

    #[test]
    fn formats_plays() {
        assert_eq!(super::fmt_plays(0), "0");
        assert_eq!(super::fmt_plays(999), "999");
        assert_eq!(super::fmt_plays(1000), "1,000");
        assert_eq!(super::fmt_plays(1_204_589_367), "1,204,589,367");
    }

    // Windows' C runtime has no tzset and doesn't know zone names like America/Toronto.
    #[cfg(unix)]
    #[test]
    fn formats_dates() {
        // SAFETY: test-only; no other threads read TZ here.
        unsafe {
            std::env::set_var("TZ", "UTC");
            tzset();
        }
        assert_eq!(super::fmt_date(0), "1970-01-01");
        assert_eq!(super::fmt_date(1_470_832_337_000), "2016-08-10");
        assert_eq!(super::fmt_date(951_782_400_000), "2000-02-29");
        unsafe {
            std::env::set_var("TZ", "America/Toronto");
            tzset();
        }
        // 04:09 UTC on Nov 6 is still the evening of Nov 5 in Toronto
        assert_eq!(super::fmt_date(1_793_933_395_000), "2026-11-05");
    }

    #[test]
    fn splash_shows_logo() {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        t.draw(|f| super::draw_splash(f, &["Mix".into()])).unwrap();
        let screen: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains(super::LOGO[1].trim()));
    }

    #[test]
    fn flag_icons_are_clickable_where_drawn() {
        use super::{Line, Rect, Span};
        let flags = Line::from(vec![Span::raw("vol 50%  "), Span::raw(" ⇄ "), Span::raw(" ↻1 "), Span::raw(" ⊘ ")]);
        let [sh, rp, pv] = super::flag_areas(&flags, Rect::new(0, 3, 40, 1));
        assert_eq!((sh.x, sh.width, rp.x, rp.width, pv.x, pv.width, pv.y), (30, 3, 33, 4, 37, 3, 3));
        assert_eq!(super::flag_areas(&flags, Rect::new(0, 3, 10, 1)), [Rect::default(); 3], "doesn't fit");
    }

    #[test]
    fn help_shows_every_key() {
        for (w, h) in [(120, 40), (60, 40)] {
            let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            t.draw(|f| { super::draw_help(f); }).unwrap();
            let screen: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
            for (_, keys) in super::HELP {
                for (_, what) in keys {
                    assert!(screen.contains(what), "{w}x{h} is missing {what}");
                }
            }
        }
    }
}
