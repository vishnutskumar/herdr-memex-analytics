use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        ExecutableCommand,
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            MouseEventKind,
        },
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::render::{human_ms, human_tokens, rel_time};
use crate::report::{self, Filters, Report};
use crate::tips;

/// How often the dashboard re-reads the daemon snapshot and tips.
const REFRESH_SECS: u64 = 30;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// What the braille activity chart plots; `c` cycles tokens -> cost -> sessions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChartMode {
    Tokens,
    Cost,
    Sessions,
}

impl ChartMode {
    fn next(self) -> Self {
        match self {
            ChartMode::Tokens => ChartMode::Cost,
            ChartMode::Cost => ChartMode::Sessions,
            ChartMode::Sessions => ChartMode::Tokens,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ChartMode::Tokens => "tokens",
            ChartMode::Cost => "cost",
            ChartMode::Sessions => "sessions",
        }
    }
    fn value(self, d: &crate::report::DayPoint) -> f64 {
        match self {
            ChartMode::Tokens => d.tokens as f64,
            ChartMode::Cost => d.cost_usd,
            ChartMode::Sessions => d.sessions as f64,
        }
    }
}

/// Full-screen dashboard for the herdr pane, in the spirit of `memex tui`:
/// live tips on top, selectable project table on the left, token-usage panel
/// on the right, activity chart and heatmap below. `q` quits, `r` forces a
/// rescan, `j/k` or the scroll wheel move, `c` cycles the chart mode.
pub fn run(filters: &Filters, paths: &crate::config::PluginPaths) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    let result = match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(mut terminal) => event_loop(&mut terminal, filters, paths),
        Err(err) => Err(err.into()),
    };
    // Restore the terminal on every exit path before surfacing the result.
    io::stdout().execute(DisableMouseCapture)?;
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
    let mut table_state = TableState::default();
    let mut rep = crate::render::load_report_shared(filters, paths);
    let mut live = tips::load(paths);
    let mut last_refresh = Instant::now();
    let mut force_rescan = false;
    let mut mode = ChartMode::Tokens;

    loop {
        if force_rescan || last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
            rep = if force_rescan {
                report::gather(filters, Some(paths)).unwrap_or(rep)
            } else {
                crate::render::load_report_shared(filters, paths)
            };
            live = tips::load(paths);
            last_refresh = Instant::now();
            force_rescan = false;
        }

        let count = rep.projects.len();
        selected = count.checked_sub(1).map_or(0, |max| selected.min(max));

        terminal.draw(|f| draw(f, &rep, &live, selected, &mut table_state, mode))?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => {
                        selected = selected.saturating_add(1).min(count.saturating_sub(1));
                    }
                    KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Char('c') => mode = mode.next(),
                    KeyCode::Char('r') => force_rescan = true,
                    _ => {}
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => selected = selected.saturating_sub(1),
                    MouseEventKind::ScrollDown => {
                        selected = selected.saturating_add(1).min(count.saturating_sub(1));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn draw(
    f: &mut Frame,
    rep: &Report,
    live: &tips::Tips,
    selected: usize,
    table_state: &mut TableState,
    mode: ChartMode,
) {
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
    draw_body(f, body, rep, selected, table_state, mode);
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
        Span::raw(format!(
            "  {window} · updated {}",
            rel_time(rep.generated_at_ms, report::now_ms())
        )),
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

fn draw_body(
    f: &mut Frame,
    area: Rect,
    rep: &Report,
    selected: usize,
    table_state: &mut TableState,
    mode: ChartMode,
) {
    // The activity chart is the full-width bottom band so the two screen lines
    // above its lowest braille row carry only the panel title.
    let [main, chart_area] =
        Layout::vertical([Constraint::Min(8), Constraint::Length(4)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);
    let [usage_area, heat_area] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(9)]).areas(right);

    draw_sessions_table(f, left, rep, selected, table_state);
    draw_usage(f, usage_area, rep);
    draw_heatmap(f, heat_area, rep);
    draw_chart(f, chart_area, rep, mode);
}

fn draw_sessions_table(
    f: &mut Frame,
    area: Rect,
    rep: &Report,
    selected: usize,
    table_state: &mut TableState,
) {
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
                Cell::from(rel_time(p.last_at_ms, report::now_ms())),
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
    .block(Block::new().title(format!(
        " Sessions {}/{} ",
        if rep.projects.is_empty() {
            0
        } else {
            selected + 1
        },
        rep.projects.len()
    )));
    table_state.select(Some(selected));
    f.render_stateful_widget(table, area, table_state);
}

/// Two-row braille area chart over the daily series, bucketed to the panel
/// width. Flat `⣀` baseline when there is nothing to scale.
fn chart_lines(
    daily: &[crate::report::DayPoint],
    mode: ChartMode,
    width: usize,
) -> Vec<Line<'static>> {
    const RAMP: [char; 5] = [' ', '⣀', '⣤', '⣶', '⣿'];
    if width == 0 {
        return Vec::new();
    }
    let n = daily.len();
    let vals: Vec<f64> = if n == 0 {
        vec![0.0; width]
    } else {
        (0..width)
            .map(|col| mode.value(&daily[col * n / width]))
            .collect()
    };
    let max = vals.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return vec![
            Line::from(" ".repeat(width)),
            Line::from(RAMP[1].to_string().repeat(width)),
        ];
    }
    let steps: Vec<usize> = vals
        .iter()
        .map(|v| (((v / max) * 8.0).round().clamp(0.0, 8.0)) as usize)
        .collect();
    let top: String = steps
        .iter()
        .map(|s| RAMP[s.saturating_sub(4).min(4)])
        .collect();
    let bottom: String = steps.iter().map(|s| RAMP[(*s).min(4)]).collect();
    vec![Line::from(top), Line::from(bottom)]
}

fn draw_chart(f: &mut Frame, area: Rect, rep: &Report, mode: ChartMode) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let lines = chart_lines(&rep.daily, mode, inner_w);
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" Activity · {} ", mode.label()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// 7x24 token-intensity grid for the last seven local days; cells are spaces
/// shaded by a five-step background ramp.
fn draw_heatmap(f: &mut Frame, area: Rect, rep: &Report) {
    const HEAT: [Color; 5] = [
        Color::Indexed(236),
        Color::Indexed(22),
        Color::Indexed(28),
        Color::Indexed(34),
        Color::Indexed(40),
    ];
    let max = rep
        .activity_heatmap
        .iter()
        .flat_map(|row| row.iter())
        .cloned()
        .fold(0u64, u64::max);
    let level = |v: u64| -> usize {
        if max == 0 || v == 0 {
            0
        } else {
            (1 + (v as f64 / max as f64 * 3.0).round() as usize).min(4)
        }
    };
    let rows = rep.activity_heatmap.len();
    let mut lines: Vec<Line> = Vec::new();
    for (i, hours) in rep.activity_heatmap.iter().enumerate() {
        let age = rows.saturating_sub(i + 1);
        let mut spans = vec![Span::styled(
            format!("-{age}d "),
            Style::new().fg(Color::DarkGray),
        )];
        for hour in hours {
            spans.push(Span::styled(" ", Style::new().bg(HEAT[level(*hour)])));
        }
        lines.push(Line::from(spans));
    }
    if rows < 7 {
        for _ in rows..7 {
            lines.push(Line::from(vec![
                Span::styled("-d ", Style::new().fg(Color::DarkGray)),
                Span::raw(" ".repeat(24)),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Last 7 days by hour "),
        ),
        area,
    );
}

fn status_lines(rep: &Report) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut spend = Vec::new();
    if let Some(burn) = rep.burn_rate_usd_per_hr {
        spend.push(Span::raw(format!("burn ${burn:.2}/hr")));
    }
    if let Some(today) = rep.today_cost_usd {
        spend.push(Span::raw(format!("today ${today:.2}")));
    }
    if !spend.is_empty() {
        lines.push(kv_spans("spend", spend));
    }
    if let Some(usage) = &rep.usage
        && let Some(wow) = &rep.wow
    {
        let delta = usage.known_cost_usd - wow.cost_usd;
        let pct = if wow.cost_usd > 0.0 {
            format!(" ({:+.1}%)", delta / wow.cost_usd * 100.0)
        } else {
            String::new()
        };
        let color = if delta > 0.0 {
            Color::Red
        } else {
            Color::Green
        };
        lines.push(kv_colored(
            "vs prior",
            &format!("{}{pct}", crate::render::signed_usd(delta)),
            color,
        ));
    }
    if let Some(fleet) = &rep.fleet {
        let color = if fleet.blocked > 0 {
            Color::Red
        } else {
            Color::Green
        };
        lines.push(kv_colored(
            "fleet",
            &format!(
                "{} working · {} blocked · {} idle",
                fleet.working, fleet.blocked, fleet.idle
            ),
            color,
        ));
    }
    if let Some(t) = &rep.turns {
        let ir = t
            .intervention_rate
            .map(|r| format!(" · IR {:.0}%", r * 100.0))
            .unwrap_or_default();
        lines.push(kv(
            "turns",
            &format!(
                "{} · p50 {} · p95 {}{ir}",
                t.completed,
                t.p50_ms.map_or_else(|| "n/a".into(), human_ms),
                t.p95_ms.map_or_else(|| "n/a".into(), human_ms),
            ),
        ));
    }
    lines
}

fn draw_usage(f: &mut Frame, area: Rect, rep: &Report) {
    let mut lines: Vec<Line> = status_lines(rep);
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    match &rep.usage {
        Some(u) => {
            lines.push(kv("events", &thousands(u.events)));
            lines.push(kv("tokens", &human_tokens(u.total_tokens)));
            lines.push(kv_colored(
                "cost",
                &format!("${:.2}", u.known_cost_usd),
                Color::Green,
            ));
            match u.cache_hit_rate {
                Some(rate) => {
                    let color = if rate >= 0.60 {
                        Color::Green
                    } else if rate >= 0.30 {
                        Color::Yellow
                    } else {
                        Color::Red
                    };
                    lines.push(kv_colored(
                        "hit-rate",
                        &format!(
                            "{:.1}% of {} prompt tokens",
                            rate * 100.0,
                            human_tokens(u.input_tokens)
                        ),
                        color,
                    ));
                }
                None => lines.push(kv("hit-rate", "n/a")),
            }
            if rep.reasoning_tokens > 0 {
                let share = rep
                    .reasoning_share
                    .map(|s| format!(" ({:.1}% of output)", s * 100.0))
                    .unwrap_or_default();
                lines.push(kv(
                    "reasoning",
                    &format!("{} tokens{share}", human_tokens(rep.reasoning_tokens)),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "cache waste",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.push(kv_colored(
                "  tokens",
                &human_tokens(u.missed_tokens),
                Color::Red,
            ));
            lines.push(kv_colored(
                "  wasted cost",
                &format!("${:.2}", u.missed_cost_usd),
                Color::Red,
            ));
            lines.push(kv("  misses", &thousands(u.miss_count)));
            lines.push(kv("  after idle", &thousands(u.idle_misses)));
            lines.push(kv("  switches", &thousands(u.model_switch_misses)));
            if let Some(b) = rep.bloating_sessions.first() {
                lines.push(kv_colored(
                    "bloat",
                    &format!(
                        "{:.12} ({}) at {} uncached",
                        b.session_id,
                        b.project,
                        human_tokens(b.last_uncached_input)
                    ),
                    Color::Yellow,
                ));
            }
            lines.push(Line::from(""));
            for s in &u.by_source {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<8}", s.source), accent()),
                    Span::raw(format!(
                        "{:>8}  ${:.2}",
                        human_tokens(s.total_tokens),
                        s.known_cost_usd
                    )),
                ]));
            }
            if !u.by_model.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "top models",
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )));
                for m in u.by_model.iter().take(3) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<14.14}", m.model), accent()),
                        Span::raw(format!(
                            "{:>8}  ${:.2}",
                            human_tokens(m.total_tokens),
                            m.known_cost_usd
                        )),
                    ]));
                }
            }
        }
        None => {
            lines.push(dim("token usage unavailable"));
            if let Some(note) = &rep.usage_note {
                lines.push(dim(note));
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
            Span::styled("c", Style::new().fg(Color::Magenta)),
            Span::raw(" chart mode  "),
            Span::styled("r", Style::new().fg(Color::Magenta)),
            Span::raw(" rescan now"),
        ])),
        area,
    );
}

fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

fn kv_line(k: &str, v: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<14}"), Style::new().fg(Color::DarkGray)),
        v,
    ])
}

fn kv(k: &str, v: &str) -> Line<'static> {
    kv_line(k, Span::raw(v.to_string()))
}

fn kv_colored(k: &str, v: &str, color: Color) -> Line<'static> {
    kv_line(k, Span::styled(v.to_string(), Style::new().fg(color)))
}

fn kv_spans(k: &str, mut v: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{k:<14}"),
        Style::new().fg(Color::DarkGray),
    )];
    spans.append(&mut v);
    Line::from(spans)
}

fn dim(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        Style::new().fg(Color::DarkGray),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn day(tokens: u64, cost: f64, sessions: u64) -> crate::report::DayPoint {
        crate::report::DayPoint {
            date: "2026-08-21".into(),
            tokens,
            cost_usd: cost,
            events: 1,
            sessions,
        }
    }

    #[test]
    fn chart_mode_cycles_tokens_cost_sessions_and_labels_match() {
        assert_eq!(ChartMode::Tokens.next(), ChartMode::Cost);
        assert_eq!(ChartMode::Cost.next(), ChartMode::Sessions);
        assert_eq!(ChartMode::Sessions.next(), ChartMode::Tokens);
        assert_eq!(ChartMode::Tokens.label(), "tokens");
        assert_eq!(ChartMode::Cost.label(), "cost");
        assert_eq!(ChartMode::Sessions.label(), "sessions");
        assert_eq!(ChartMode::Cost.value(&day(100, 2.5, 3)), 2.5);
        assert_eq!(ChartMode::Sessions.value(&day(100, 2.5, 3)), 3.0);
    }

    #[test]
    fn empty_daily_renders_a_flat_braille_baseline_row() {
        let lines = chart_lines(&[], ChartMode::Tokens, 10);
        assert_eq!(lines.len(), 2);
        let bottom = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(bottom, "⣀".repeat(10));
    }

    #[test]
    fn chart_buckets_daily_series_to_panel_width_with_peak_at_top_ramp() {
        let daily: Vec<_> = (1..=4).map(|i| day(i * 1_000, i as f64, i)).collect();
        for mode in [ChartMode::Tokens, ChartMode::Cost, ChartMode::Sessions] {
            let lines = chart_lines(&daily, mode, 8);
            assert_eq!(lines.len(), 2);
            let bottom = lines[1]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            // Peak days map to the full block; zero-free series never blanks a column.
            assert!(bottom.contains('⣿'), "peak column must be ⣿: {bottom}");
            assert!(!bottom.contains(' '), "no dead columns: {bottom}");
        }
        // Narrow panels still bucket without panicking.
        assert_eq!(chart_lines(&daily, ChartMode::Cost, 2).len(), 2);
    }
}
