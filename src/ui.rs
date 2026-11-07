//! Rendering. Called only when something changed (or once a second while playing).

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Clear, LineGauge, List, ListItem, Paragraph, Row as TRow, Table, TableState},
};

use ratatui::layout::Rect;
use ratatui_image::Image;

use crate::{
    api::Kind,
    app::{App, Focus, LyricsState, Repeat},
    audio::{BANDS, MAX_DB},
};

const ACCENT: Color = Color::Green;

fn fmt_ms(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// ms since epoch -> "YYYY-MM-DD" in the local timezone.
fn fmt_date(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r only writes into the tm we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let b = Block::bordered().border_type(BorderType::Rounded).title(format!(" {title} "));
    if focused { b.border_style(Style::new().fg(ACCENT)) } else { b.border_style(Style::new().dim()) }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [body, now, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(2), Constraint::Length(1)]).areas(f.area());
    let side_w = (f.area().width / 4).clamp(18, 32);
    let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).areas(body);

    // sidebar, with the cover under it when there's room
    let art_rows = art_rows(app, side);
    app.art_want = art_rows.map(|r| (side.width, r));
    let (side, art_area) = match art_rows {
        Some(r) => {
            let [s, a] = Layout::vertical([Constraint::Min(6), Constraint::Length(r)]).areas(side);
            (s, Some(a))
        }
        None => (side, None),
    };
    let items: Vec<ListItem> = app.side.iter().map(|s| ListItem::new(s.name.as_str())).collect();
    let hl = if app.focus == Focus::Side { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let list = List::new(items).block(block("Library", app.focus == Focus::Side)).highlight_style(hl);
    f.render_stateful_widget(list, side, &mut app.side_state);
    if let (Some(area), Some(proto)) = (art_area, app.art.as_ref().and_then(|a| a.proto.as_ref())) {
        let sz = proto.size();
        let x = area.x + area.width.saturating_sub(sz.width) / 2;
        f.render_widget(Image::new(proto), Rect { x, y: area.y, width: sz.width.min(area.width), height: sz.height.min(area.height) });
    }

    if app.lyrics_open {
        draw_lyrics(f, app, main);
    } else {
        draw_table(f, app, main);
    }
    draw_now(f, app, now);

    if let Some(band) = app.eq_panel {
        draw_eq(f, app, band);
    }
    if app.chooser.is_some() {
        draw_chooser(f, app);
    }

    // bottom line: search input or status
    if app.focus == Focus::Input {
        let text = format!(" Search: {}", app.input);
        let x = bottom.x + text.chars().count() as u16;
        f.render_widget(Paragraph::new(text).fg(ACCENT), bottom);
        f.set_cursor_position((x.min(bottom.right().saturating_sub(1)), bottom.y));
    } else {
        f.render_widget(Paragraph::new(format!(" {}", app.status)).dim(), bottom);
    }
}

/// Height (in rows) for a square cover the width of the sidebar, if the sidebar is tall
/// enough to still show a useful list above it.
fn art_rows(app: &App, side: Rect) -> Option<u16> {
    let picker = app.img_picker.as_ref()?;
    if !app.art_on || app.now.as_ref()?.art.is_none() {
        return None;
    }
    let fs = picker.font_size();
    let rows = (side.width as u32 * fs.width as u32 / fs.height.max(1) as u32) as u16;
    (rows >= 4 && side.height >= rows + 10).then_some(rows)
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
    let show_added = rows[off..end].iter().any(|r| r.added_at > 0);
    let trows = rows[off..end].iter().map(|r| {
        let (tag, tag_color) = match r.kind {
            Kind::Track => ("", Color::Reset),
            Kind::Album => ("album ", Color::Cyan),
            Kind::Playlist => ("playlist ", Color::Magenta),
            Kind::Artist => ("artist ", Color::Yellow),
        };
        let heart = if r.kind == Kind::Track && app.liked.contains(&r.uri) { "♥ " } else { "" };
        let title = Line::from(vec![
            Span::styled(heart, Style::new().fg(ACCENT)),
            Span::styled(tag, Style::new().fg(tag_color)),
            Span::raw(r.title.as_str()),
        ]);
        let dur = if r.duration_ms > 0 { fmt_ms(r.duration_ms) } else { String::new() };
        let mut cells = vec![Cell::from(title), Cell::from(r.artist.as_str()), Cell::from(r.album.as_str())];
        if show_added {
            cells.push(Cell::from(if r.added_at > 0 { fmt_date(r.added_at) } else { String::new() }).style(Style::new().dim()));
        }
        cells.push(Cell::from(Line::from(dur).right_aligned()));
        let mut row = TRow::new(cells);
        if Some(r.uri.as_str()) == now_uri {
            row = row.style(Style::new().fg(ACCENT).bold());
        }
        row
    });
    let hl = if app.focus == Focus::Main { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let mut title = app.view.title.clone();
    if app.view.by_added {
        title.push_str(" · newest first");
    }
    if app.loading {
        title.push_str(" (loading…)");
    }
    let (widths, header): (Vec<Constraint>, Vec<&str>) = if show_added {
        (
            vec![Constraint::Percentage(38), Constraint::Percentage(25), Constraint::Percentage(25), Constraint::Length(10), Constraint::Length(6)],
            vec!["Title", "Artist", "Album", "Added", "  Time"],
        )
    } else {
        (
            vec![Constraint::Percentage(42), Constraint::Percentage(28), Constraint::Percentage(30), Constraint::Length(6)],
            vec!["Title", "Artist", "Album", "  Time"],
        )
    };
    let table = Table::new(trows, widths)
    .header(TRow::new(header).style(Style::new().dim()))
    .block(block(&title, app.focus == Focus::Main))
    .row_highlight_style(hl)
    .column_spacing(1);
    f.render_stateful_widget(table, main, &mut window_state);
}

fn draw_now(f: &mut Frame, app: &App, now: Rect) {
    let [line1, line2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(now);
    let mut flags = format!("vol {}%", app.volume_pct());
    if app.eq.is_some() && !app.audio.is_flat() {
        flags.push_str(&format!("  eq {}", app.audio.preset_name().to_lowercase()));
    }
    if app.audio.crossfade_s > 0 {
        flags.push_str(&format!("  xfade {}s", app.audio.crossfade_s));
    }
    if app.shuffle {
        flags.push_str("  shuffle");
    }
    match app.repeat {
        Repeat::All => flags.push_str("  repeat"),
        Repeat::One => flags.push_str("  repeat-1"),
        Repeat::Off => {}
    }
    if !app.queue.is_empty() {
        flags.push_str(&format!("  queue {}", app.queue.len()));
    }
    match &app.now {
        Some(r) => {
            let icon = if app.playing { "▶" } else { "⏸" };
            let heart = if app.liked.contains(&r.uri) { " ♥" } else { "" };
            let [l, rt] = Layout::horizontal([Constraint::Min(10), Constraint::Length(flags.len() as u16 + 1)]).areas(line1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::new().fg(ACCENT).bold()),
                    Span::styled(r.title.as_str(), Style::new().bold()),
                    Span::styled(heart, Style::new().fg(ACCENT)),
                    Span::raw("  "),
                    Span::styled(r.artist.as_str(), Style::new().dim()),
                ])),
                l,
            );
            f.render_widget(Paragraph::new(flags.as_str()).dim().right_aligned(), rt);
            let pos = app.position_ms().min(r.duration_ms);
            let ratio = if r.duration_ms > 0 { pos as f64 / r.duration_ms as f64 } else { 0.0 };
            let label = format!(" {} / {} ", fmt_ms(pos), fmt_ms(r.duration_ms));
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(label)
                    .filled_style(Style::new().fg(ACCENT))
                    .unfilled_style(Style::new().dim()),
                line2,
            );
        }
        None => {
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" ■ Nothing playing").dim()])), line1);
            f.render_widget(Paragraph::new(flags.as_str()).dim().right_aligned(), line1);
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
                Some(c) if i == c => Style::new().fg(ACCENT).bold(),
                Some(c) if i < c => Style::new().dim(),
                _ => Style::new(),
            };
            Line::styled(text.as_str(), style).centered()
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_chooser(f: &mut Frame, app: &App) {
    let Some(c) = app.chooser.as_ref() else { return };
    let area = f.area();
    let w = 50.min(area.width.saturating_sub(2));
    let h = 18.min(area.height.saturating_sub(2));
    let rect = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let blk = block("Add to playlist", true);
    let inner = blk.inner(rect);
    f.render_widget(blk, rect);
    let [song, filter, list, help] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(format!(" “{}” — {}", c.track.title, c.track.artist)).bold(), song);
    f.render_widget(Paragraph::new(format!(" filter: {}▏", c.filter)).fg(ACCENT), filter);
    let matches = c.matches(app);
    let visible = list.height as usize;
    let off = c.sel.saturating_sub(visible.saturating_sub(1));
    let items: Vec<ListItem> = matches.iter().skip(off).take(visible).map(|(_, n)| ListItem::new(format!(" {n}"))).collect();
    let mut st = ratatui::widgets::ListState::default().with_selected((!matches.is_empty()).then_some(c.sel - off));
    let l = if matches.is_empty() { List::new(vec![ListItem::new(" no matching playlists").dim()]) } else { List::new(items) };
    f.render_stateful_widget(l.highlight_style(Style::new().fg(Color::Black).bg(ACCENT)), list, &mut st);
    f.render_widget(Paragraph::new(" type to filter · ↑↓ pick · ⏎ add · Esc cancel").dim(), help);
}


fn draw_eq(f: &mut Frame, app: &App, sel: usize) {
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
        let style = if i == sel { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:>4} Hz "), style),
            Span::styled(bar, if i == sel { Style::new().fg(ACCENT) } else { Style::new().dim() }),
            Span::styled(format!(" {g:+3.0} dB"), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(" ↑↓ band  ←→ ±1 dB  0 reset band  p/P preset  f flat").dim());
    lines.push(Line::from(" x crossfade  space play/pause  Esc close").dim());
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    unsafe extern "C" {
        fn tzset();
    }

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
}
