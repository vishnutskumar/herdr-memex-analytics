use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        ExecutableCommand,
        event::{self, Event, KeyCode, KeyEventKind},
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
};

use crate::report::{self, Filters, Report};
use crate::tips;

/// How often the dashboard re-reads the daemon snapshot and tips.
const REFRESH_SECS: u64 = 30;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Full-screen dashboard for the herdr pane, in the spirit of `memex tui`:
/// live tips on top, selectable project table on the left, token-usage panel
/// on the right. `q` quits, `r` forces a rescan, `j/k` move.
pub fn run(filters: &Filters, paths: &crate::config::PluginPaths) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal: Tui = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = event_loop(&mut terminal, filters, paths);
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

fn event_loop(
    terminal: &mut Tui,
    filters: &Filters,
    paths: &crate::config::PluginPaths,
) -> Result<()> {
    let mut selected: usize = 0;
    let mut rep = crate::render::load_report_shared(filters, paths);
    let mut live = tips::load(paths);
    let mut last_refresh = Instant::now();
    let mut force_rescan = false;

    loop {
        if force_rescan || last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
            rep = if force_rescan {
                report::gather(filters).unwrap_or(rep)
            } else {
                crate::render::load_report_shared(filters, paths)
            };
            live = tips::load(paths);
            last_refresh = Instant::now();
            force_rescan = false;
        }

        let count = rep.projects.len();
        selected = count.checked_sub(1).map_or(0, |max| selected.min(max));

        terminal.draw(|f| draw(f, &rep, &live, selected))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('j') | KeyCode::Down => {
                    if count > 0 {
                        selected = (selected + 1).min(count - 1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Char('r') => force_rescan = true,
                _ => {}
            }
        }
    }
}

fn draw(f: &mut Frame, rep: &Report, live: &tips::Tips, selected: usize) {
    let tips_h = if live.items.is_empty() {
        0
    } else {
        live.items.len() as u16 + 1
    };
    let [header, tips_area, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(tips_h),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, rep);
    draw_tips(f, tips_area, live);
    draw_body(f, body, rep, selected);
    draw_footer(f, footer);
}

fn draw_header(f: &mut Frame, area: Rect, rep: &Report) {
    let window = if rep.since_ms.is_some() {
        "last 30d"
    } else {
        "all history"
    };
    let line = Line::from(vec![
        Span::styled(
            " herdr analytics",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {window} · updated {}", rel(rep.generated_at_ms))),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_tips(f: &mut Frame, area: Rect, live: &tips::Tips) {
    if area.height == 0 {
        return;
    }
    let lines: Vec<Line> = live
        .items
        .iter()
        .map(|t| {
            let (mark, color) = if t.urgent {
                (" ! ", Color::Red)
            } else {
                (" · ", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(mark, Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::raw(t.message.clone()),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_body(f: &mut Frame, area: Rect, rep: &Report, selected: usize) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);

    let rows: Vec<Row> = rep
        .projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if i == selected {
                Style::new().bg(Color::DarkGray)
            } else {
                Style::new()
            };
            Row::new(vec![
                Cell::from(p.project.clone()),
                Cell::from(p.sessions.to_string()),
                Cell::from(p.messages.to_string()),
                Cell::from(format!("{:.1}", p.active_ms as f64 / 3_600_000.0)),
                Cell::from(rel(p.last_at_ms)),
            ])
            .style(style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(14),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(vec!["project", "sess", "msgs", "hrs", "last"])
            .style(accent().add_modifier(Modifier::BOLD)),
    )
    .block(Block::new().borders(Borders::ALL).title(" Sessions "));
    f.render_widget(table, left);

    draw_usage(f, right, rep);
}

fn draw_usage(f: &mut Frame, area: Rect, rep: &Report) {
    let mut lines: Vec<Line> = Vec::new();
    match &rep.usage {
        Some(u) => {
            lines.push(kv("events", &thousands(u.events)));
            lines.push(kv("tokens", &human(u.total_tokens)));
            lines.push(kv_colored(
                "cost",
                &format!("${:.2}", u.known_cost_usd),
                Color::Green,
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "cache waste",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.push(kv_colored("  tokens", &human(u.missed_tokens), Color::Red));
            lines.push(kv_colored(
                "  wasted cost",
                &format!("${:.2}", u.missed_cost_usd),
                Color::Red,
            ));
            lines.push(kv("  misses", &thousands(u.miss_count)));
            lines.push(kv("  after idle", &thousands(u.idle_misses)));
            lines.push(kv("  switches", &thousands(u.model_switch_misses)));
            lines.push(Line::from(""));
            for s in &u.by_source {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<8}", s.source), accent()),
                    Span::raw(format!(
                        "{:>8}  ${:.2}",
                        human(s.total_tokens),
                        s.known_cost_usd
                    )),
                ]));
            }
            lines.push(Line::from(""));
            for hint in [
                "idle-gap misses = you came back after",
                "the cache went cold. Batch related work",
                "into one session to keep it warm.",
            ] {
                lines.push(Line::from(Span::styled(
                    hint,
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                "token usage unavailable",
                Style::new().fg(Color::DarkGray),
            )));
            if let Some(note) = &rep.usage_note {
                lines.push(Line::from(Span::styled(
                    note.clone(),
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).title(" Token usage ")),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect) {
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" q", Style::new().fg(Color::Magenta)),
            Span::raw(" quit  "),
            Span::styled("j/k", Style::new().fg(Color::Magenta)),
            Span::raw(" move  "),
            Span::styled("r", Style::new().fg(Color::Magenta)),
            Span::raw(" rescan now"),
        ])),
        area,
    );
}

fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<14}"), Style::new().fg(Color::DarkGray)),
        Span::raw(v.to_string()),
    ])
}

fn kv_colored(k: &str, v: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<14}"), Style::new().fg(Color::DarkGray)),
        Span::styled(v.to_string(), Style::new().fg(color)),
    ])
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
    }
}

fn rel(then_ms: u64) -> String {
    let secs = report::now_ms().saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}
