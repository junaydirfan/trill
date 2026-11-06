//! Rendering. Called only when something changed (or once a second while playing).

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, LineGauge, List, ListItem, Paragraph, Row as TRow, Table},
};

use crate::{
    api::Kind,
    app::{App, Focus, Repeat},
};

const ACCENT: Color = Color::Green;

fn fmt_ms(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let b = Block::bordered().border_type(BorderType::Rounded).title(format!(" {title} "));
    if focused { b.border_style(Style::new().fg(ACCENT)) } else { b.border_style(Style::new().dim()) }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [body, now, bottom] = Layout::vertical([Constraint::Min(5), Constraint::Length(2), Constraint::Length(1)]).areas(f.area());
    let side_w = (f.area().width / 4).clamp(18, 32);
    let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20)]).areas(body);

    // sidebar
    let items: Vec<ListItem> = app.side.iter().map(|s| ListItem::new(s.name.as_str())).collect();
    let hl = if app.focus == Focus::Side { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let list = List::new(items).block(block("Library", app.focus == Focus::Side)).highlight_style(hl);
    f.render_stateful_widget(list, side, &mut app.side_state);

    // main table
    let now_uri = app.now.as_ref().map(|r| r.uri.as_str());
    let rows = app.view.rows.clone();
    let trows = rows.iter().map(|r| {
        let (tag, tag_color) = match r.kind {
            Kind::Track => ("", Color::Reset),
            Kind::Album => ("album ", Color::Cyan),
            Kind::Playlist => ("playlist ", Color::Magenta),
            Kind::Artist => ("artist ", Color::Yellow),
        };
        let title = Line::from(vec![Span::styled(tag, Style::new().fg(tag_color)), Span::raw(r.title.as_str())]);
        let dur = if r.duration_ms > 0 { fmt_ms(r.duration_ms) } else { String::new() };
        let mut row = TRow::new(vec![
            Cell::from(title),
            Cell::from(r.artist.as_str()),
            Cell::from(r.album.as_str()),
            Cell::from(Line::from(dur).right_aligned()),
        ]);
        if Some(r.uri.as_str()) == now_uri {
            row = row.style(Style::new().fg(ACCENT).bold());
        }
        row
    });
    let hl = if app.focus == Focus::Main { Style::new().fg(Color::Black).bg(ACCENT) } else { Style::new().add_modifier(Modifier::REVERSED) };
    let title = if app.loading { format!("{} (loading…)", app.view.title) } else { app.view.title.clone() };
    let table = Table::new(
        trows,
        [Constraint::Percentage(42), Constraint::Percentage(28), Constraint::Percentage(30), Constraint::Length(6)],
    )
    .header(TRow::new(["Title", "Artist", "Album", "  Time"]).style(Style::new().dim()))
    .block(block(&title, app.focus == Focus::Main))
    .row_highlight_style(hl)
    .column_spacing(1);
    f.render_stateful_widget(table, main, &mut app.view.state);

    // now playing
    let [line1, line2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(now);
    let mut flags = format!("vol {}%", app.volume_pct());
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
            let [l, rt] = Layout::horizontal([Constraint::Min(10), Constraint::Length(flags.len() as u16 + 1)]).areas(line1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::new().fg(ACCENT).bold()),
                    Span::styled(r.title.as_str(), Style::new().bold()),
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

    // bottom line: search input or status
    if app.focus == Focus::Input {
        let text = format!(" / {}", app.input);
        let x = bottom.x + text.chars().count() as u16;
        f.render_widget(Paragraph::new(text).fg(ACCENT), bottom);
        f.set_cursor_position((x.min(bottom.right().saturating_sub(1)), bottom.y));
    } else {
        f.render_widget(Paragraph::new(format!(" {}", app.status)).dim(), bottom);
    }
}
