//! Live terminal dashboard. Renders scan results as they arrive from a
//! background scan loop. This module owns the terminal; in dashboard mode
//! `main` routes tracing output to a log file so it does not corrupt the UI.

use crate::scanner::{ScanResult, Scanner};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Table, TableState},
    DefaultTerminal, Frame,
};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const ACCENT: Color = Color::Rgb(0x4f, 0xd1, 0xc5);
const DIM: Color    = Color::Rgb(0x6b, 0x72, 0x80);
const TEXT: Color   = Color::Rgb(0xe5, 0xe7, 0xeb);
const GOOD: Color   = Color::Rgb(0x4a, 0xde, 0x80);
const WARN: Color   = Color::Rgb(0xfb, 0xbf, 0x24);
const BAD: Color    = Color::Rgb(0xf8, 0x71, 0x71);
const SEL_BG: Color = Color::Rgb(0x2a, 0x32, 0x3d);

enum ScanUpdate {
    Started,
    Done(Box<ScanResult>),
    Failed(String),
}

#[derive(PartialEq)]
enum Status { Starting, Scanning, Idle, Error }

struct App {
    status: Status,
    last: Option<ScanResult>,
    last_error: Option<String>,
    history: VecDeque<u64>,
    scan_started_at: Instant,
    last_done_at: Option<Instant>,
    poll_interval: Duration,
    table: TableState,
    should_quit: bool,
}

impl App {
    fn new(poll_interval: Duration) -> Self {
        Self {
            status: Status::Starting,
            last: None,
            last_error: None,
            history: VecDeque::new(),
            scan_started_at: Instant::now(),
            last_done_at: None,
            poll_interval,
            table: TableState::default(),
            should_quit: false,
        }
    }

    fn apply(&mut self, u: ScanUpdate) {
        match u {
            ScanUpdate::Started => {
                self.status = Status::Scanning;
                self.scan_started_at = Instant::now();
            }
            ScanUpdate::Done(r) => {
                self.history.push_back(r.opportunities.len() as u64);
                if self.history.len() > 120 { self.history.pop_front(); }
                self.last = Some(*r);
                self.last_done_at = Some(Instant::now());
                self.status = Status::Idle;
                self.last_error = None;
            }
            ScanUpdate::Failed(e) => {
                self.status = Status::Error;
                self.last_error = Some(e);
                self.last_done_at = Some(Instant::now());
            }
        }
    }

    fn scroll(&mut self, delta: i32) {
        let len = self.last.as_ref().map(|r| r.opportunities.len()).unwrap_or(0);
        if len == 0 { return; }
        let cur = self.table.selected().unwrap_or(0) as i32;
        self.table.select(Some((cur + delta).clamp(0, len as i32 - 1) as usize));
    }
}

/// Entry point. Spawns the scan loop, then runs the render loop until quit.
pub async fn run(mut scanner: Scanner, poll_interval: Duration) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<ScanUpdate>(8);

    tokio::spawn(async move {
        loop {
            if tx.send(ScanUpdate::Started).await.is_err() { break; }
            let update = match scanner.scan_once().await {
                Ok(r)  => ScanUpdate::Done(Box::new(r)),
                Err(e) => ScanUpdate::Failed(format!("{e:#}")),
            };
            if tx.send(update).await.is_err() { break; }
            tokio::time::sleep(poll_interval).await;
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new(poll_interval);
    let result = render_loop(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();
    result
}

async fn render_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::Receiver<ScanUpdate>,
) -> Result<()> {
    loop {
        while let Ok(u) = rx.try_recv() { app.apply(u); }
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Char('c')
                            if k.modifiers.contains(KeyModifiers::CONTROL) =>
                            app.should_quit = true,
                        KeyCode::Down | KeyCode::Char('j') => app.scroll(1),
                        KeyCode::Up   | KeyCode::Char('k') => app.scroll(-1),
                        _ => {}
                    }
                }
            }
        }
        if app.should_quit { return Ok(()); }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(f.area());

    header(f, root[0], app);
    stats_row(f, root[1], app);

    if app.last.is_some() {
        opportunities_table(f, root[2], app);
    } else {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "Scanning MarginFi v2 for the first time, this takes ~50s",
                Style::default().fg(DIM),
            )),
        ])
        .alignment(Alignment::Center)
        .block(panel_block("TOP LIQUIDATION OPPORTUNITIES"));
        f.render_widget(msg, root[2]);
    }

    history_panel(f, root[3], app);
    footer(f, root[4], app);
}

fn panel_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(30)])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(" \u{25c6} ", Style::default().fg(ACCENT)),
        Span::styled("LIQ-BOT", Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled("  MarginFi v2 Liquidation Scanner", Style::default().fg(DIM)),
    ]))
    .block(Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM)));
    f.render_widget(title, cols[0]);

    let (label, color) = match app.status {
        Status::Starting => ("STARTING", WARN),
        Status::Scanning => ("SCANNING", WARN),
        Status::Idle     => ("LIVE", GOOD),
        Status::Error    => ("ERROR", BAD),
    };
    let badge = Paragraph::new(Line::from(Span::styled(
        format!(" {label} "),
        Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center)
    .block(Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM)));
    f.render_widget(badge, cols[1]);
}

fn stats_row(f: &mut Frame, area: Rect, app: &App) {
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(area);

    let (scanned, opps, priced, total, dur) = match &app.last {
        Some(r) => (
            r.positions_scanned,
            r.opportunities.len(),
            r.banks_priced,
            r.banks_total,
            r.scan_duration.as_secs(),
        ),
        None => (0, 0, 0, 0, 0),
    };

    stat(f, cells[0], "POSITIONS SCANNED", fmt_int(scanned), TEXT);
    stat(f, cells[1], "OPPORTUNITIES", opps.to_string(), ACCENT);
    gauge_panel(f, cells[2], priced, total);
    stat(f, cells[3], "LAST SCAN", format!("{dur}s"), TEXT);
}

fn stat(f: &mut Frame, area: Rect, title: &str, value: String, color: Color) {
    let block = panel_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let para = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            value,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center);
    f.render_widget(para, inner);
}

fn gauge_panel(f: &mut Frame, area: Rect, priced: usize, total: usize) {
    let block = panel_block("ORACLE COVERAGE");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let ratio = if total == 0 { 0.0 } else { priced as f64 / total as f64 };
    let color = if ratio > 0.66 { GOOD } else if ratio > 0.25 { WARN } else { BAD };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(format!("{priced} / {total} banks"));
    f.render_widget(gauge, rows[1]);
}

fn opportunities_table(f: &mut Frame, area: Rect, app: &mut App) {
    let header = Row::new(
        ["#", "POSITION", "OWNER", "HEALTH", "COLLATERAL", "DEBT", "EST. PROFIT"],
    )
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .last
        .as_ref()
        .map(|r| {
            r.opportunities
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    Row::new(vec![
                        Cell::from(format!("{}", i + 1)).style(Style::default().fg(DIM)),
                        Cell::from(fmt_pk(&o.position)).style(Style::default().fg(TEXT)),
                        Cell::from(fmt_pk(&o.owner)).style(Style::default().fg(DIM)),
                        Cell::from(format!("{:.4}", o.health_factor)).style(
                            Style::default()
                                .fg(health_color(o.health_factor))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Cell::from(fmt_usd(o.collateral_usd))
                            .style(Style::default().fg(TEXT)),
                        Cell::from(fmt_usd(o.debt_usd)).style(Style::default().fg(TEXT)),
                        Cell::from(fmt_usd(o.est_profit_usd)).style(
                            Style::default()
                                .fg(if o.est_profit_usd > 0.0 { GOOD } else { DIM })
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();

    let widths = [
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Length(16),
        Constraint::Length(9),
        Constraint::Length(15),
        Constraint::Length(15),
        Constraint::Min(13),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(panel_block("TOP LIQUIDATION OPPORTUNITIES  (ranked by est. profit)"))
        .column_spacing(2)
        .row_highlight_style(Style::default().bg(SEL_BG))
        .highlight_symbol("> ");

    f.render_stateful_widget(table, area, &mut app.table);
}

fn history_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block("OPPORTUNITIES OVER TIME");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let bars = spark(&app.history, inner.width as usize);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(bars, Style::default().fg(ACCENT)))),
        rows[1],
    );
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let info = match (&app.status, app.last_done_at) {
        (Status::Scanning, _) =>
            format!("scanning... {}s elapsed", app.scan_started_at.elapsed().as_secs()),
        (Status::Idle, Some(t)) => format!(
            "next scan in {}s",
            app.poll_interval.saturating_sub(t.elapsed()).as_secs()
        ),
        (Status::Error, _) => app.last_error.clone().unwrap_or_default(),
        _ => "starting...".into(),
    };
    let line = Line::from(vec![
        Span::styled("  q ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("quit   ", Style::default().fg(DIM)),
        Span::styled("up/down ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled("scroll   .  ", Style::default().fg(DIM)),
        Span::styled(info, Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn fmt_pk(pk: &Pubkey) -> String {
    let s = pk.to_string();
    if s.len() > 12 {
        format!("{}\u{2026}{}", &s[..6], &s[s.len() - 4..])
    } else {
        s
    }
}

fn health_color(h: f64) -> Color {
    if h < 0.25 { BAD }
    else if h < 0.60 { Color::Rgb(0xf9, 0xa8, 0x25) }
    else if h < 0.85 { WARN }
    else { Color::Rgb(0xa3, 0xe6, 0x35) }
}

fn fmt_int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.push(','); }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn fmt_usd(v: f64) -> String {
    let neg = v < 0.0;
    let cents = format!("{:.2}", v.abs());
    let (int, frac) = cents.split_once('.').unwrap_or((&cents, "00"));
    let grouped = fmt_int(int.parse::<usize>().unwrap_or(0));
    format!("{}${}.{}", if neg { "-" } else { "" }, grouped, frac)
}

fn spark(data: &VecDeque<u64>, width: usize) -> String {
    const BARS: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}',
                             '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
    if data.is_empty() || width == 0 { return String::new(); }
    let max = (*data.iter().max().unwrap()).max(1);
    data.iter()
        .rev()
        .take(width)
        .rev()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * 7.0).round() as usize;
            BARS[idx.min(7)]
        })
        .collect()
}