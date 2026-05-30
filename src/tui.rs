//! Live fullscreen dashboard with 4-protocol header strip, live signal,
//! top-opportunities table, risk radar, whale watch, oracle divergence,
//! protocol leaderboard, chain status. Animation driven by a 50ms tick.

use crate::protocol::ImplStatus;
use crate::scanner::{OppRow, ScanResult, Scanner};
use crate::types::{DivergenceRow, RiskBuckets, WhaleRow};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Cell, Chart, Dataset, Gauge,
        GraphType, Paragraph, Row, Table, TableState,
    },
    DefaultTerminal, Frame,
};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::VecDeque,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

// Palette
const TEAL: Color   = Color::Rgb(0x4f, 0xd1, 0xc5); // MarginFi
const VIOLET: Color = Color::Rgb(0xc4, 0xb5, 0xfd); // Kamino
const AMBER: Color  = Color::Rgb(0xfb, 0xbf, 0x24); // Drift
const ROSE: Color   = Color::Rgb(0xfb, 0x71, 0xa8); // Solend
const DIM: Color    = Color::Rgb(0x6b, 0x72, 0x80);
const TEXT: Color   = Color::Rgb(0xe5, 0xe7, 0xeb);
const MUTED: Color  = Color::Rgb(0x9c, 0xa3, 0xaf);
const GOOD: Color   = Color::Rgb(0x4a, 0xde, 0x80);
const WARN: Color   = Color::Rgb(0xfb, 0xbf, 0x24);
const BAD: Color    = Color::Rgb(0xf8, 0x71, 0x71);
const SEL_BG: Color = Color::Rgb(0x2a, 0x32, 0x3d);

const SPINNER: &[&str] = &[
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}",
    "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

enum ScanUpdate {
    Started { protocol: String },
    Done(Box<ScanResult>),
    Failed { protocol: String, err: String },
    ChainTick { slot: u64, prio: u64 },
}

#[derive(PartialEq, Clone, Copy)]
enum Status { Starting, Scanning, Idle, Error, Pending }

struct Panel {
    title: String,
    accent: Color,
    status: Status,
    last: Option<ScanResult>,
    scan_started_at: Instant,
    last_done_at: Option<Instant>,
    impl_status: ImplStatus,
}

impl Panel {
    fn new(title: &str, accent: Color, impl_status: ImplStatus) -> Self {
        let initial = if impl_status == ImplStatus::Pending {
            Status::Pending
        } else {
            Status::Starting
        };
        Self {
            title: title.to_string(),
            accent, status: initial,
            last: None,
            scan_started_at: Instant::now(),
            last_done_at: None,
            impl_status,
        }
    }
    fn apply(&mut self, u: ScanUpdate) {
    match u {
        ScanUpdate::Started { .. } => {
            if self.impl_status == ImplStatus::Live {
                self.status = Status::Scanning;
                self.scan_started_at = Instant::now();
            }
        }
        ScanUpdate::Done(r) => {
            self.last = Some(*r);
            self.last_done_at = Some(Instant::now());
            self.status = if self.impl_status == ImplStatus::Pending {
                Status::Pending
            } else {
                Status::Idle
            };
        }
        ScanUpdate::Failed { err, .. } => {
            if self.impl_status == ImplStatus::Live {
                self.status = Status::Error;
                self.last_done_at = Some(Instant::now());
                tracing::error!(panel = %self.title, err, "scan failed");
            }
        }
        ScanUpdate::ChainTick { .. } => {
            // Routed by App, never reaches Panel.
        }
    }
}
}

struct App {
    panels: Vec<Panel>,
    table_state: TableState,
    profit_history: VecDeque<f64>,
    poll_interval: Duration,
    should_quit: bool,
    tick: u64,
    started_at: Instant,
    chain_slot: u64,
    chain_priority_fee: u64,
}

impl App {
    fn route(&mut self, u: ScanUpdate) {
        let target = match &u {
            ScanUpdate::Started { protocol } => protocol.clone(),
            ScanUpdate::Done(r) => r.protocol_name.clone(),
            ScanUpdate::Failed { protocol, .. } => protocol.clone(),
            ScanUpdate::ChainTick { slot, prio } => {
                self.chain_slot = *slot;
                self.chain_priority_fee = *prio;
                return;
            }
        };
        if let ScanUpdate::Done(r) = &u {
            self.profit_history.push_back(r.total_profit_usd);
            if self.profit_history.len() > 240 { self.profit_history.pop_front(); }
        }
        if let Some(p) = self.panels.iter_mut().find(|p| p.title == target) {
            p.apply(u);
        }
    }
    fn any_scanning(&self) -> bool {
        self.panels.iter().any(|p| matches!(p.status, Status::Scanning | Status::Starting))
    }
    fn current_slot(&self) -> u64 { self.chain_slot }
    fn priority_fee(&self) -> u64 { self.chain_priority_fee }
    fn all_opps(&self) -> Vec<(Color, String, &OppRow)> {
        let mut v: Vec<(Color, String, &OppRow)> = Vec::new();
        for p in &self.panels {
            if let Some(r) = &p.last {
                for o in &r.opportunities {
                    v.push((p.accent, p.title.clone(), o));
                }
            }
        }
        v.sort_by(|a, b| b.2.est_profit_usd.total_cmp(&a.2.est_profit_usd));
        v
    }
}

pub async fn run(scanners: Vec<Scanner>, poll_interval: Duration, rpc: std::sync::Arc<crate::rpc::Rpc>) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<ScanUpdate>(32);

    let accents = [TEAL, VIOLET, AMBER, ROSE];
    let panels: Vec<Panel> = scanners.iter().enumerate().map(|(i, s)| {
        Panel::new(s.name(), accents[i % accents.len()], s.impl_status())
    }).collect();

    for mut scanner in scanners {
        let tx = tx.clone();
        tokio::spawn(async move {
            let pname = scanner.name().to_string();
            loop {
                let _ = tx.send(ScanUpdate::Started { protocol: pname.clone() }).await;
                let update = match scanner.scan_once().await {
                    Ok(r) => ScanUpdate::Done(Box::new(r)),
                    Err(e) => ScanUpdate::Failed { protocol: pname.clone(), err: format!("{e:#}") },
                };
                if tx.send(update).await.is_err() { break; }
                tokio::time::sleep(poll_interval).await;
            }
        });
    }
    {
        let tx = tx.clone();
        let rpc = rpc.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let slot = rpc.current_slot().await.unwrap_or(0);
                let prio = rpc.median_priority_fee().await.unwrap_or(0);
                let _ = tx.send(ScanUpdate::ChainTick { slot, prio }).await;
            }
        });
    }
    drop(tx);

    let mut terminal = ratatui::init();
    let mut app = App {
        panels,
        table_state: TableState::default(),
        profit_history: VecDeque::new(),
        poll_interval,
        should_quit: false,
        tick: 0,
        started_at: Instant::now(),
        chain_slot: 0,
        chain_priority_fee: 0,
    };
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
        while let Ok(u) = rx.try_recv() { app.route(u); }
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL)
                            => app.should_quit = true,
                        KeyCode::Down | KeyCode::Char('j') => {
                            let n = app.all_opps().len();
                            if n > 0 {
                                let c = app.table_state.selected().unwrap_or(0);
                                app.table_state.select(Some(((c + 1).min(n - 1))));
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let c = app.table_state.selected().unwrap_or(0);
                            app.table_state.select(Some(c.saturating_sub(1)));
                        }
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
            Constraint::Length(3),    // header
            Constraint::Length(9),    // live signal
            Constraint::Length(8),    // 4-protocol strip
            Constraint::Length(7),    // risk radar | leaderboard | chain
            Constraint::Min(12),      // top opportunities table + breakdown
            Constraint::Length(9),    // whale watch | oracle divergence
            Constraint::Length(1),    // footer
        ])
        .split(f.area());

    header(f, root[0], app);
    live_signal(f, root[1], app);
    protocol_strip(f, root[2], app);
    middle_row(f, root[3], app);
    opportunities_and_breakdown(f, root[4], app);
    bottom_row(f, root[5], app);
    footer(f, root[6], app);
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(42),
            Constraint::Min(10),
            Constraint::Length(36),
        ])
        .split(inner);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(" \u{25c6} ", Style::default().fg(TEAL)),
        Span::styled("LIQ-BOT",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled("  Solana Multi-Protocol Liquidation Terminal",
            Style::default().fg(MUTED)),
    ]));
    f.render_widget(title, cols[0]);

    let total_opps: usize = app.panels.iter()
        .filter_map(|p| p.last.as_ref().map(|r| r.opportunities.len())).sum();
    let total_profit: f64 = app.panels.iter()
        .filter_map(|p| p.last.as_ref()).map(|r| r.total_profit_usd).sum();
    let center = Paragraph::new(Line::from(vec![
        Span::styled(" opps ", Style::default().fg(DIM)),
        Span::styled(fmt_int(total_opps),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("aggregate profit ", Style::default().fg(DIM)),
        Span::styled(fmt_usd(total_profit),
            Style::default().fg(GOOD).add_modifier(Modifier::BOLD)),
    ])).alignment(Alignment::Center);
    f.render_widget(center, cols[1]);

    let uptime = app.started_at.elapsed().as_secs();
    let up_str = format!("{:02}:{:02}:{:02}",
        uptime / 3600, (uptime / 60) % 60, uptime % 60);
    let clock = Paragraph::new(Line::from(vec![
        Span::styled(utc_clock(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled(" UTC", Style::default().fg(DIM)),
        Span::raw("  "),
        Span::styled("up ", Style::default().fg(DIM)),
        Span::styled(up_str, Style::default().fg(MUTED)),
        Span::raw(" "),
    ])).alignment(Alignment::Right);
    f.render_widget(clock, cols[2]);
}

fn live_signal(f: &mut Frame, area: Rect, app: &App) {
    let scanning = app.any_scanning();
    let spinner = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
    let title = if scanning {
        format!(" {} live signal  scanning ", spinner)
    } else {
        " \u{25cb} live signal  idle ".to_string()
    };
    let border = if scanning {
        scale_color(TEAL, 0.6 + 0.4 * ((app.tick as f64 * 0.18).sin() + 1.0) * 0.5)
    } else { DIM };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(title,
            Style::default().fg(if scanning { WARN } else { GOOD })
                .add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let width = (inner.width as usize).max(8) * 8;
    let phase = app.tick as f64 * 0.38;
    let amp = if scanning { 2.2 } else { 1.1 };

    // Four overlaid waves matching the four protocol accents.
    let waves: Vec<(Color, f64, f64)> = vec![
        (TEAL,   0.0, 1.00),
        (VIOLET, 1.8, 0.78),
        (AMBER,  3.4, 0.55),
        (ROSE,   2.5, 0.42),
    ];

    let mut datasets_data: Vec<Vec<(f64, f64)>> = Vec::with_capacity(waves.len());
    for (_, offset, scale) in &waves {
        let data: Vec<(f64, f64)> = (0..width).map(|x| {
            let xf = x as f64 * 0.10;
            let v = (xf + phase + offset).sin() * amp * scale
                  + (xf * 4.1 + phase * 1.7 + offset).sin() * (amp * 0.18)
                  + (xf * 9.3 + phase * 2.4).sin() * (amp * 0.06);
            (x as f64, v)
        }).collect();
        datasets_data.push(data);
    }

    let datasets: Vec<Dataset> = waves.iter().enumerate().map(|(i, (c, _, _))| {
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(*c))
            .data(&datasets_data[i])
    }).collect();

    let y = amp * 1.15;
    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([0.0, width as f64]))
        .y_axis(Axis::default().bounds([-y, y]));
    f.render_widget(chart, inner);
}

fn protocol_strip(f: &mut Frame, area: Rect, app: &App) {
    let n = app.panels.len() as u32;
    if n == 0 { return; }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, n); n as usize])
        .split(area);
    for i in 0..app.panels.len() {
        render_proto_card(f, cols[i], &app.panels[i], app.tick);
    }
}

fn render_proto_card(f: &mut Frame, area: Rect, panel: &Panel, tick: u64) {
    let (badge, badge_color) = match panel.status {
        Status::Starting => ("STARTING", WARN),
        Status::Scanning => ("SCANNING", WARN),
        Status::Idle     => ("LIVE",     GOOD),
        Status::Error    => ("ERROR",    BAD),
        Status::Pending  => ("PENDING",  MUTED),
    };
    let scanning = matches!(panel.status, Status::Scanning | Status::Starting);
    let pulse = if scanning {
        let t = ((tick as f64 * 0.18).sin() + 1.0) * 0.5;
        scale_color(panel.accent, 0.55 + 0.45 * t)
    } else if panel.impl_status == ImplStatus::Pending {
        DIM
    } else { panel.accent };

    let spinner = if scanning {
        SPINNER[(tick as usize / 2) % SPINNER.len()]
    } else { "\u{25cf}" };

    let title = format!(" {} {} ", spinner, panel.title.to_uppercase());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(pulse))
        .title(Span::styled(title,
            Style::default().fg(panel.accent).add_modifier(Modifier::BOLD)))
        .title_bottom(Span::styled(
            format!(" {} ", badge),
            Style::default().fg(badge_color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if panel.impl_status == ImplStatus::Pending {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("layout verification",
                Style::default().fg(MUTED))),
            Line::from(Span::styled("pending",
                Style::default().fg(MUTED))),
            Line::from(""),
            Line::from(Span::styled("trait scaffold ready",
                Style::default().fg(DIM))),
        ]).alignment(Alignment::Center);
        f.render_widget(msg, inner);
        return;
    }

    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let (scanned, opps, dur, priced, total, profit) = match &panel.last {
        Some(r) => (
            r.positions_scanned, r.opportunities.len(),
            r.scan_duration.as_secs(),
            r.banks_priced, r.banks_total,
            r.total_profit_usd,
        ),
        None => (0, 0, 0, 0, 0, 0.0),
    };

    f.render_widget(stat_line(" positions  ", fmt_int(scanned), TEXT), rows_layout[0]);
    f.render_widget(stat_line(" opps       ", opps.to_string(), panel.accent), rows_layout[1]);
    f.render_widget(stat_line(" profit     ", fmt_usd(profit), GOOD), rows_layout[2]);
    f.render_widget(stat_line(" scan time  ", format!("{dur}s"), MUTED), rows_layout[3]);

    let ratio = if total == 0 { 0.0 } else { priced as f64 / total as f64 };
    let cov_color = if ratio > 0.66 { GOOD } else if ratio > 0.25 { WARN } else { BAD };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(cov_color))
        .ratio(ratio.min(1.0))
        .label(format!("oracle {priced}/{total}"));
    f.render_widget(gauge, rows_layout[4]);
}

fn stat_line(label: &'static str, value: String, value_color: Color) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(label, Style::default().fg(DIM)),
        Span::styled(value, Style::default().fg(value_color).add_modifier(Modifier::BOLD)),
    ]))
}

fn middle_row(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(2, 5),
            Constraint::Ratio(2, 5),
            Constraint::Ratio(1, 5),
        ])
        .split(area);
    risk_radar(f, cols[0], app);
    protocol_leaderboard(f, cols[1], app);
    chain_panel(f, cols[2], app);
}

fn risk_radar(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block(" \u{25c9} risk radar  (opportunity pipeline) ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Sum buckets across all live protocols.
    let mut agg = RiskBuckets::default();
    for p in &app.panels {
        if let Some(r) = &p.last {
            agg.total_priced += r.risk_buckets.total_priced;
            agg.watch += r.risk_buckets.watch;
            agg.at_risk += r.risk_buckets.at_risk;
            agg.edge += r.risk_buckets.edge;
            agg.liquidatable += r.risk_buckets.liquidatable;
        }
    }
    let max = agg.watch.max(1) as f64;

    let bars: Vec<(&str, usize, Color)> = vec![
        ("HF < 1.00  liquidatable", agg.liquidatable, BAD),
        ("HF < 1.02  edge",         agg.edge,         Color::Rgb(0xf9, 0xa8, 0x25)),
        ("HF < 1.05  at risk",      agg.at_risk,      WARN),
        ("HF < 1.10  watch",        agg.watch,        MUTED),
    ];

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 4])
        .split(inner);

    let bar_width = (inner.width as usize).saturating_sub(36);
    for (i, (label, n, color)) in bars.iter().enumerate() {
        let frac = (*n as f64 / max).min(1.0);
        let blocks = (frac * bar_width as f64) as usize;
        let bar: String = "\u{2588}".repeat(blocks);
        let line = Line::from(vec![
            Span::styled(format!("  {:<24}", label), Style::default().fg(MUTED)),
            Span::styled(format!("{:>7}  ", fmt_int(*n)),
                Style::default().fg(*color).add_modifier(Modifier::BOLD)),
            Span::styled(bar, Style::default().fg(*color)),
        ]);
        f.render_widget(Paragraph::new(line), rows[i]);
    }
}

fn protocol_leaderboard(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block(" \u{2605} protocol leaderboard  (potential profit) ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut entries: Vec<(&str, Color, f64, usize, ImplStatus)> = app.panels.iter().map(|p| {
        let (profit, opps) = match &p.last {
            Some(r) => (r.total_profit_usd, r.opportunities.len()),
            None => (0.0, 0),
        };
        (p.title.as_str(), p.accent, profit, opps, p.impl_status)
    }).collect();
    entries.sort_by(|a, b| b.2.total_cmp(&a.2));

    let header = Row::new(["#", "PROTOCOL", "OPPS", "POTENTIAL PROFIT"])
        .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = entries.iter().enumerate().map(|(i, (name, color, profit, opps, st))| {
        let profit_cell = if *st == ImplStatus::Pending {
            Cell::from("pending verification").style(Style::default().fg(MUTED))
        } else {
            Cell::from(fmt_usd(*profit))
                .style(Style::default().fg(GOOD).add_modifier(Modifier::BOLD))
        };
        Row::new(vec![
            Cell::from(format!("{}", i + 1)).style(Style::default().fg(DIM)),
            Cell::from(name.to_uppercase()).style(
                Style::default().fg(*color).add_modifier(Modifier::BOLD)),
            Cell::from(opps.to_string()).style(Style::default().fg(TEXT)),
            profit_cell,
        ])
    }).collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(2);
    f.render_widget(table, inner);
}

fn chain_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block(" \u{25aa} chain ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let slot = app.current_slot();
    let prio = app.priority_fee();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1); 5])
        .split(inner);

    f.render_widget(stat_line(" slot       ", fmt_int(slot as usize), TEXT), rows[0]);
    f.render_widget(stat_line(" block time ", "~400ms".to_string(), MUTED), rows[1]);
    let prio_str = if prio == 0 { "0".to_string() } else { format!("{} \u{00b5}lpc", prio) };
    f.render_widget(stat_line(" priority   ", prio_str, MUTED), rows[2]);
    f.render_widget(stat_line(" cluster    ", "mainnet-beta".to_string(), MUTED), rows[3]);
    f.render_widget(stat_line(" rpc        ", "Helius".to_string(), MUTED), rows[4]);
}

fn opportunities_and_breakdown(f: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(3, 4), Constraint::Ratio(1, 4)])
        .split(area);

    f.render_widget(panel_block(
        " \u{25c6} top opportunities  (all protocols, ranked by net profit) ", TEXT),
        cols[0]);
    let inner_left = Rect::new(
        cols[0].x + 1, cols[0].y + 1,
        cols[0].width.saturating_sub(2), cols[0].height.saturating_sub(2));

    // Materialize the opportunities into owned data so the immutable
    // borrow on `app` ends before we mutably borrow app.table_state below.
    let opps_owned: Vec<(Color, String, OppRow)> = app.all_opps()
        .into_iter()
        .map(|(c, name, o)| (c, name, o.clone()))
        .collect();

    let header = Row::new(["#", "PROTO", "POSITION", "OWNER", "HEALTH", "DEBT", "BONUS", "NET PROFIT"])
        .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = opps_owned.iter().enumerate()
        .take((inner_left.height.saturating_sub(2)) as usize)
        .map(|(i, (c, name, o))| {
        Row::new(vec![
            Cell::from(format!("{}", i + 1)).style(Style::default().fg(DIM)),
            Cell::from(short_proto(name))
                .style(Style::default().fg(*c).add_modifier(Modifier::BOLD)),
            Cell::from(fmt_pk(&o.position)).style(Style::default().fg(TEXT)),
            Cell::from(fmt_pk(&o.owner)).style(Style::default().fg(DIM)),
            Cell::from(format!("{:.4}", o.health_factor))
                .style(Style::default().fg(health_color(o.health_factor))
                    .add_modifier(Modifier::BOLD)),
            Cell::from(fmt_usd(o.debt_usd)).style(Style::default().fg(TEXT)),
            Cell::from(fmt_usd(o.bonus_usd)).style(Style::default().fg(MUTED)),
            Cell::from(fmt_usd(o.est_profit_usd))
                .style(Style::default().fg(GOOD).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let widths = [
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(11),
        Constraint::Min(12),
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(1)
        .row_highlight_style(Style::default().bg(SEL_BG))
        .highlight_symbol("> ");

    let sel_idx = app.table_state.selected().unwrap_or(0);
    f.render_stateful_widget(table, inner_left, &mut app.table_state);

    // Breakdown panel: take ownership of the selected OppRow so no
    // borrow of `app` outlives this call.
    let selected = opps_owned.get(sel_idx).map(|(_, _, o)| o.clone());
    breakdown_panel(f, cols[1], selected.as_ref());
}

fn breakdown_panel(f: &mut Frame, area: Rect, opp: Option<&OppRow>) {
    let block = panel_block(" \u{25b8} selected breakdown ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = if let Some(o) = opp {
        vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  position    ", Style::default().fg(DIM)),
                Span::styled(fmt_pk(&o.position),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  health      ", Style::default().fg(DIM)),
                Span::styled(format!("{:.4}", o.health_factor),
                    Style::default().fg(health_color(o.health_factor))
                        .add_modifier(Modifier::BOLD)),
            ]),
            Line::from(""),
            Line::from(Span::styled("  components",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD))),
            Line::from(vec![
                Span::styled("  debt repaid ", Style::default().fg(DIM)),
                Span::styled(fmt_usd(o.debt_repaid_usd),
                    Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("  bonus  2.5% ", Style::default().fg(DIM)),
                Span::styled(format!("+{}", fmt_usd(o.bonus_usd)),
                    Style::default().fg(GOOD)),
            ]),
            Line::from(vec![
                Span::styled("  cost  est.  ", Style::default().fg(DIM)),
                Span::styled(format!("-{}", fmt_usd(o.cost_usd)),
                    Style::default().fg(BAD)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  net profit  ", Style::default().fg(DIM)),
                Span::styled(fmt_usd(o.est_profit_usd),
                    Style::default().fg(GOOD).add_modifier(Modifier::BOLD)),
            ]),
        ]
    } else {
        vec![
            Line::from(""),
            Line::from(Span::styled("  no opportunities", Style::default().fg(MUTED))),
            Line::from(Span::styled("  to break down",   Style::default().fg(MUTED))),
            Line::from(""),
            Line::from(Span::styled("  scroll table with",   Style::default().fg(DIM))),
            Line::from(Span::styled("  up/down  to select",  Style::default().fg(DIM))),
        ]
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn bottom_row(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(3, 5), Constraint::Ratio(2, 5)])
        .split(area);
    whale_watch(f, cols[0], app);
    oracle_divergence_panel(f, cols[1], app);
}

fn whale_watch(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block(" \u{1f40b} whale watch  (largest positions, any health) ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut all: Vec<(Color, String, &WhaleRow)> = Vec::new();
    for p in &app.panels {
        if let Some(r) = &p.last {
            for w in &r.whale_watch {
                all.push((p.accent, p.title.clone(), w));
            }
        }
    }
    all.sort_by(|a, b| b.2.debt_usd.total_cmp(&a.2.debt_usd));

    let header = Row::new(["#", "PROTO", "POSITION", "OWNER", "HEALTH", "DEBT", "STATUS"])
        .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = all.iter().enumerate()
        .take(inner.height.saturating_sub(2) as usize)
        .map(|(i, (c, name, w))| {
        let status_text = if w.liquidatable { "LIQ NOW" } else { "watching" };
        let status_color = if w.liquidatable { BAD } else { MUTED };
        Row::new(vec![
            Cell::from(format!("{}", i + 1)).style(Style::default().fg(DIM)),
            Cell::from(short_proto(name))
                .style(Style::default().fg(*c).add_modifier(Modifier::BOLD)),
            Cell::from(fmt_pk(&w.position)).style(Style::default().fg(TEXT)),
            Cell::from(fmt_pk(&w.owner)).style(Style::default().fg(DIM)),
            Cell::from(format!("{:.3}", w.health_factor))
                .style(Style::default().fg(health_color(w.health_factor))),
            Cell::from(fmt_usd(w.debt_usd))
                .style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Cell::from(status_text)
                .style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let widths = [
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Min(9),
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(1);
    f.render_widget(table, inner);
}

fn oracle_divergence_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel_block(" \u{26a0} oracle divergence  (cross-bank price drift) ", TEXT);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut all: Vec<(Color, String, &DivergenceRow)> = Vec::new();
    for p in &app.panels {
        if let Some(r) = &p.last {
            for d in &r.oracle_divergence {
                all.push((p.accent, p.title.clone(), d));
            }
        }
    }
    all.sort_by(|a, b| b.2.spread_pct.total_cmp(&a.2.spread_pct));

    let header = Row::new(["PROTO", "MINT", "SRC", "SPREAD"])
        .style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = all.iter()
        .take(inner.height.saturating_sub(2) as usize)
        .map(|(c, name, d)| {
        let spread_color = if d.spread_pct < 0.5 { GOOD }
            else if d.spread_pct < 2.0 { WARN } else { BAD };
        Row::new(vec![
            Cell::from(short_proto(name))
                .style(Style::default().fg(*c).add_modifier(Modifier::BOLD)),
            Cell::from(fmt_pk(&d.mint)).style(Style::default().fg(TEXT)),
            Cell::from(d.sources.to_string()).style(Style::default().fg(MUTED)),
            Cell::from(format!("{:.3}%", d.spread_pct))
                .style(Style::default().fg(spread_color).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let widths = [
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Length(5),
        Constraint::Min(8),
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(2);
    f.render_widget(table, inner);
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(" q ", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
        Span::styled("quit  ", Style::default().fg(DIM)),
        Span::styled("up/down ", Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
        Span::styled("scroll opportunities  ", Style::default().fg(DIM)),
        Span::styled(format!("\u{2022} poll {}s  ", app.poll_interval.as_secs()),
            Style::default().fg(DIM)),
        Span::styled(format!("tick {}", app.tick), Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// helpers

fn panel_block(title: &'static str, title_color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(title,
            Style::default().fg(title_color).add_modifier(Modifier::BOLD)))
}
fn fmt_pk(pk: &Pubkey) -> String {
    let s = pk.to_string();
    if s.len() > 12 { format!("{}\u{2026}{}", &s[..5], &s[s.len() - 4..]) } else { s }
}
fn health_color(h: f64) -> Color {
    if h < 0.25 { BAD }
    else if h < 0.6 { Color::Rgb(0xf9, 0xa8, 0x25) }
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
fn scale_color(c: Color, k: f64) -> Color {
    let k = k.clamp(0.0, 1.5);
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(
            ((r as f64 * k).min(255.0)) as u8,
            ((g as f64 * k).min(255.0)) as u8,
            ((b as f64 * k).min(255.0)) as u8,
        ),
        other => other,
    }
}
fn short_proto(name: &str) -> String {
    match name {
        "marginfi-v2" => "MFI".to_string(),
        "kamino"      => "KMNO".to_string(),
        "drift"       => "DRFT".to_string(),
        "solend"      => "SLND".to_string(),
        other         => other.to_string(),
    }
}
fn utc_clock() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}