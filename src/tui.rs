//! TUI variant of the AI session board.
//!
//! Renders the same dashboard as `board_widget.rs` but using ratatui in the
//! current terminal — works inside a psmux pane, doesn't need Win32, and
//! quits cleanly with `q` or Ctrl-C.
//!
//! Data fetching here is duplicated from board_widget.rs for now; consolidate
//! into a shared `board_data` module later.

use std::collections::{HashMap, HashSet};
use std::io::{self};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Row, Table},
    Frame, Terminal,
};
use sysinfo::System;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const ACTIVE_MINS: i64 = 15;
const MAX_SESSIONS: usize = 12;
const REFRESH_SECS: u64 = 5;
const HISTORY_LEN: usize = 60;
const SAMPLE_INTERVAL_MS: u64 = 1000;
const TARGETS: &[(&str, Color)] = &[("claude", Color::Cyan), ("codex", Color::Yellow)];

#[derive(Clone)]
struct Session {
    source: String,
    project: String,
    model: String,
    run: String,
    last: String,
    tokens: String,
    cost: f64,
    active: bool,
    count: u32,
    /// Active but not currently streaming: agent stopped, awaiting user reply.
    waiting: bool,
}

struct State {
    sessions: Vec<Session>,
    show_all: bool,
    footer_segs: Vec<(String, Color)>,
    mtd_cost: f64,
    loading: bool,        // true while a background refresh is in flight
    cpu_history: Vec<Vec<f64>>,   // one row per TARGETS entry
    ram_history: Vec<Vec<f64>>,
    proc_counts: Vec<usize>,
    show_charts: bool,    // toggleable with `c`
    tick: u64,            // increments each redraw — drives waiting-row flash
}

#[derive(Clone, Default)]
struct ProcSample {
    cpu: Vec<f64>,        // %; one entry per TARGETS
    ram: Vec<f64>,        // % of total system RAM
    counts: Vec<usize>,
}

/// Snapshot produced by the background refresh thread.
struct Snapshot {
    sessions: Vec<Session>,
    footer_segs: Vec<(String, Color)>,
    mtd_cost: f64,
}

/// Tells the worker which view mode to compute.
struct RefreshRequest { show_all: bool }

pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let n = TARGETS.len();
    let mut state = State {
        sessions: Vec::new(),
        show_all: false,
        footer_segs: Vec::new(),
        mtd_cost: 0.0,
        loading: true,
        cpu_history: vec![vec![0.0; HISTORY_LEN]; n],
        ram_history: vec![vec![0.0; HISTORY_LEN]; n],
        proc_counts: vec![0; n],
        show_charts: true,
        tick: 0,
    };

    let stop_flag = Arc::new(AtomicBool::new(false));

    // CPU/RAM sampler: ticks every second, sends a ProcSample.
    let (sample_tx, sample_rx) = mpsc::channel::<ProcSample>();
    {
        let stop = stop_flag.clone();
        thread::spawn(move || {
            // new_all() primes total_memory() and all process info — without this,
            // total_memory() is 0 and every RAM% divides by zero.
            let mut sys = System::new_all();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let total_ram_mb = sys.total_memory() as f64 / 1024.0 / 1024.0;
            // sysinfo's cpu_usage() is % of one core — divide by core count so
            // 100% means the whole system is pegged.
            let core_count = sys.cpus().len().max(1) as f64;
            loop {
                if stop.load(Ordering::Relaxed) { break; }
                thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                let mut s = ProcSample {
                    cpu: vec![0.0; TARGETS.len()],
                    ram: vec![0.0; TARGETS.len()],
                    counts: vec![0; TARGETS.len()],
                };
                for proc in sys.processes().values() {
                    let name = proc.name().to_string_lossy().to_lowercase();
                    for (idx, (target, _)) in TARGETS.iter().enumerate() {
                        if name.contains(target) {
                            s.cpu[idx] += proc.cpu_usage() as f64 / core_count;
                            // proc.memory() returns bytes (sysinfo 0.30+).
                            let rss_mb = proc.memory() as f64 / 1024.0 / 1024.0;
                            if total_ram_mb > 0.0 {
                                s.ram[idx] += rss_mb / total_ram_mb * 100.0;
                            }
                            s.counts[idx] += 1;
                            break;
                        }
                    }
                }
                if sample_tx.send(s).is_err() { break; }
            }
        });
    }

    // Background worker: fetches snapshots without blocking the UI loop.
    // Triggered immediately, then on every RefreshRequest received via tx.
    let (req_tx, req_rx) = mpsc::channel::<RefreshRequest>();
    let (snap_tx, snap_rx) = mpsc::channel::<Snapshot>();
    {
        let stop = stop_flag.clone();
        thread::spawn(move || {
            // Initial fetch with default mode.
            let mut current_show_all = false;
            let _ = snap_tx.send(fetch_snapshot(current_show_all));

            loop {
                if stop.load(Ordering::Relaxed) { break; }
                // Wait up to REFRESH_SECS for a manual request, otherwise tick.
                match req_rx.recv_timeout(Duration::from_secs(REFRESH_SECS)) {
                    Ok(req) => current_show_all = req.show_all,
                    Err(mpsc::RecvTimeoutError::Timeout) => {} // periodic refresh
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if stop.load(Ordering::Relaxed) { break; }
                if snap_tx.send(fetch_snapshot(current_show_all)).is_err() { break; }
            }
        });
    }

    let mut last_refresh = Instant::now();
    let _ = last_refresh; // silence unused-mut if logic changes later

    let result = (|| -> io::Result<()> {
        loop {
            // Drain any snapshots produced by the worker (non-blocking).
            while let Ok(snap) = snap_rx.try_recv() {
                state.sessions = snap.sessions;
                state.footer_segs = snap.footer_segs;
                state.mtd_cost = snap.mtd_cost;
                state.loading = false;
            }

            // Drain process samples (1Hz), shifting each into the history ring.
            while let Ok(s) = sample_rx.try_recv() {
                for i in 0..TARGETS.len() {
                    let cpu = state.cpu_history[i].clone();
                    let ram = state.ram_history[i].clone();
                    state.cpu_history[i] = cpu.into_iter().skip(1)
                        .chain(std::iter::once(s.cpu[i])).collect();
                    state.ram_history[i] = ram.into_iter().skip(1)
                        .chain(std::iter::once(s.ram[i])).collect();
                }
                state.proc_counts = s.counts;
            }

            state.tick = state.tick.wrapping_add(1);
            term.draw(|f| render(f, &state))?;

            // Wait ~500ms for the next frame, processing keys as they come.
            let frame_end = Instant::now() + Duration::from_millis(500);
            while Instant::now() < frame_end {
                match event::poll(Duration::from_millis(20)) {
                    Ok(true) => match event::read() {
                        Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                            match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL)
                                    => return Ok(()),
                                KeyCode::Char('a') => {
                                    state.show_all = !state.show_all;
                                    state.loading = true;
                                    let _ = req_tx.send(RefreshRequest { show_all: state.show_all });
                                    break;
                                }
                                KeyCode::Char('r') => {
                                    state.loading = true;
                                    let _ = req_tx.send(RefreshRequest { show_all: state.show_all });
                                    break;
                                }
                                KeyCode::Char('c') => {
                                    state.show_charts = !state.show_charts;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    })();

    stop_flag.store(true, Ordering::Relaxed);
    drop(req_tx);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

fn render(f: &mut Frame, st: &State) {
    let area = f.area();

    // Allocate up to 10 rows for charts if the terminal is tall enough,
    // and the user hasn't hidden them via `c`.
    let chart_rows: u16 = if !st.show_charts { 0 }
        else if area.height >= 24 { 10 }
        else if area.height >= 16 { 7 }
        else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                  // title bar
            Constraint::Min(3),                     // table
            Constraint::Length(chart_rows),         // CPU/RAM charts (or 0)
            Constraint::Length(1),                  // footer
        ])
        .split(area);

    render_title(f, chunks[0], st);
    render_table(f, chunks[1], st);
    if chart_rows > 0 { render_charts(f, chunks[2], st); }
    render_footer(f, chunks[3], st);
}

fn render_charts(f: &mut Frame, area: Rect, st: &State) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    // Shared Y scale across CPU and RAM (both are %-of-system) so the 0 baseline
    // and visual scale match. Floor at 20 so small values remain readable.
    let max_cpu = st.cpu_history.iter().flat_map(|h| h.iter().copied()).fold(0.0_f64, f64::max);
    let max_ram = st.ram_history.iter().flat_map(|h| h.iter().copied()).fold(0.0_f64, f64::max);
    let shared_max = max_cpu.max(max_ram).ceil().max(20.0);
    render_chart(f, halves[0], st, true, shared_max);
    render_chart(f, halves[1], st, false, shared_max);
}

fn render_chart(f: &mut Frame, area: Rect, st: &State, is_cpu: bool, max_y: f64) {
    let history = if is_cpu { &st.cpu_history } else { &st.ram_history };

    // Convert each target's history into (x, y) points; ratatui's Chart
    // expects f64 pairs. Hold the buffers so the dataset references stay live.
    let data_owned: Vec<Vec<(f64, f64)>> = history.iter().map(|h| {
        h.iter().enumerate().map(|(x, &y)| (x as f64, y)).collect()
    }).collect();

    let mut datasets: Vec<Dataset> = Vec::new();
    for (i, (label, color)) in TARGETS.iter().enumerate() {
        datasets.push(
            Dataset::default()
                .name(*label)
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
                .data(&data_owned[i])
        );
    }

    let title_text = if is_cpu { "CPU %" } else { "RAM %" };
    // Build a Line with colored legend: "CPU %  ●claude(N)  ●codex(N)"
    let mut title_spans: Vec<Span> = vec![
        Span::styled(format!("{} ", title_text),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ];
    for (i, (name, color)) in TARGETS.iter().enumerate() {
        let v = history.get(i).and_then(|h| h.last().copied()).unwrap_or(0.0);
        title_spans.push(Span::styled(
            format!(" ●{} {:.1}%", name, v),
            Style::default().fg(*color),
        ));
    }
    let title = Line::from(title_spans);

    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(title)
            .border_style(Style::default().fg(Color::DarkGray)))
        .x_axis(Axis::default().bounds([0.0, HISTORY_LEN as f64 - 1.0]))
        .y_axis(Axis::default()
            .bounds([0.0, max_y])
            .labels(vec![
                Span::raw("0"),
                Span::raw(format!("{:.0}", max_y / 2.0)),
                Span::raw(format!("{:.0}", max_y)),
            ]));

    f.render_widget(chart, area);
}

fn render_title(f: &mut Frame, area: Rect, st: &State) {
    let mode = if st.show_all { " [24H]" } else { "" };
    let load = if st.loading { " ⏳" } else { "" };
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    let left = format!("■ AI SESSION BOARD ■{}{}", mode, load);
    let right = format!("{}    q quit · a 24H · c chart · r refresh", now);
    let pad = area.width.saturating_sub(left.len() as u16 + right.len() as u16) as usize;
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_table(f: &mut Frame, area: Rect, st: &State) {
    let header = Row::new(vec!["SRC", "PROJECT", "MODEL", "RUN", "LAST", "TOKENS", "COST", "●"])
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    // Pulse waiting rows ~1Hz between bright yellow and dim. Tick increments each
    // redraw (~2x/sec), so divide by 2 for a ~1Hz pulse.
    let flash_bright = (st.tick / 2) % 2 == 0;
    let waiting_color = if flash_bright { Color::Yellow } else { Color::Rgb(140, 110, 0) };

    let rows: Vec<Row> = st.sessions.iter().take(MAX_SESSIONS).map(|s| {
        let main = if s.waiting {
            Style::default().fg(waiting_color).add_modifier(Modifier::BOLD)
        } else if s.active {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let dot = if s.waiting {
            Span::styled("●", Style::default().fg(waiting_color))
        } else if s.active {
            Span::styled("●", Style::default().fg(Color::Green))
        } else {
            Span::styled("○", Style::default().fg(Color::DarkGray))
        };
        // count == 0 → unique project; count >= 1 → (N) duplicate label
        let proj_label = if s.count >= 1 {
            format!("{}({})", s.project, s.count)
        } else {
            s.project.clone()
        };
        let run_style = if s.waiting {
            Style::default().fg(waiting_color)
        } else if s.active {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Row::new(vec![
            Span::styled(s.source.clone(), main).into(),
            Span::styled(proj_label, main).into(),
            Span::styled(s.model.clone(), main).into(),
            Span::styled(s.run.clone(), run_style).into(),
            Span::styled(s.last.clone(), main).into(),
            Span::styled(s.tokens.clone(), main).into(),
            Span::styled(format!("{:.2}", s.cost), main).into(),
            ratatui::text::Text::from(dot),
        ])
    }).collect();

    let widths = [
        Constraint::Length(7),
        Constraint::Length(24),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(2),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::DarkGray)));

    f.render_widget(table, area);
}

fn render_footer(f: &mut Frame, area: Rect, st: &State) {
    let mut spans: Vec<Span> = Vec::new();
    for (text, color) in &st.footer_segs {
        spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
    }
    if st.mtd_cost > 0.0 {
        spans.push(Span::styled(
            format!("  MTD${:.0}", st.mtd_cost),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ─── Data refresh (duplicates board_widget.rs logic) ──────────────────────

/// Synchronous, blocking — runs in the background worker thread.
fn fetch_snapshot(show_all: bool) -> Snapshot {
    let mut snap = Snapshot {
        sessions: Vec::new(),
        footer_segs: Vec::new(),
        mtd_cost: 0.0,
    };
    do_fetch(&mut snap, show_all);
    snap
}

fn do_fetch(st: &mut Snapshot, show_all: bool) {
    let now = chrono::Local::now().naive_local();
    let today = chrono::Local::now().format("%Y%m%d").to_string();
    let month_start = chrono::Local::now().format("%Y%m01").to_string();

    // MTD cost
    if let Some(json) = run_tu(&["session", "-j", "--since", &month_start]) {
        if let Some(arr) = json["sessions"].as_array() {
            st.mtd_cost = arr.iter()
                .map(|s| s["totals"]["cost_usd"].as_f64().unwrap_or(0.0))
                .sum();
        }
    }

    // Today's sessions
    let Some(json) = run_tu(&["session", "-j", "--since", &today]) else { return; };

    struct ProjEntry {
        active_first_dt: Option<chrono::NaiveDateTime>,
        active_last_dt: Option<chrono::NaiveDateTime>,
        last_dt: chrono::NaiveDateTime,
        tokens: u64,
        cost: f64,
        sources: HashSet<String>,
        model: String,
        count: u32,
    }
    // Each tu session entry becomes its own row — no aggregation by project.
    let mut entries: Vec<(String, ProjEntry)> = Vec::new();

    if let Some(arr) = json["sessions"].as_array() {
        for s in arr {
            let project = s["project"].as_str().unwrap_or("?").to_string();
            let session_id = s["session_id"].as_str().unwrap_or("");
            let last_str = s["last_activity"].as_str().unwrap_or("");
            let Ok(last_dt) = chrono::NaiveDateTime::parse_from_str(last_str, "%Y-%m-%d %H:%M:%S")
            else { continue; };

            let tokens = s["totals"]["total_tokens"].as_u64().unwrap_or(0);
            let cost = s["totals"]["cost_usd"].as_f64().unwrap_or(0.0);
            let src = source_of(&s["sources"]);
            let model = detect_model(&s["models"]);

            // Per-session JSONL — gives accurate run_start (last real user msg)
            // and run_end (file mtime). Without this, tu only has last_activity
            // which collapses RUN to 0s.
            let (active_first, active_last, effective_last) =
                match jsonl_run_for_session(session_id) {
                    Some((start, end)) => (Some(start), Some(end), end.max(last_dt)),
                    None => (Some(last_dt), Some(last_dt), last_dt),
                };

            let mut sources = HashSet::new();
            sources.insert(src);

            entries.push((project, ProjEntry {
                active_first_dt: active_first,
                active_last_dt:  active_last,
                last_dt: effective_last,
                tokens, cost, sources, model,
                count: 1,
            }));
        }
    }

    entries.sort_by(|a, b| b.1.last_dt.cmp(&a.1.last_dt));
    let sorted: Vec<(String, ProjEntry)> = entries.into_iter().take(MAX_SESSIONS).collect();

    let mut project_total: HashMap<String, u32> = HashMap::new();
    for (proj, _) in &sorted {
        *project_total.entry(proj.clone()).or_insert(0) += 1;
    }
    let mut project_seen: HashMap<String, u32> = HashMap::new();

    let all: Vec<Session> = sorted.into_iter().map(|(project, e)| {
        let last_secs = (now - e.last_dt).num_seconds().max(0);
        let active = last_secs <= ACTIVE_MINS * 60;
        let waiting = active && last_secs >= 60;
        let source = match (e.sources.contains("claude"), e.sources.contains("codex")) {
            (true, true) => "BOTH",
            (true, false) => "CLAUDE",
            (false, true) => "CODEX",
            _ => "?",
        }.into();
        let run = match (e.active_first_dt, e.active_last_dt) {
            (Some(start), Some(end)) => fmt_duration((end - start).num_seconds().max(0)),
            _ => "--".into(),
        };
        // (N) suffix only if multiple rows share this project
        let total = project_total.get(&project).copied().unwrap_or(1);
        let suffix_n = if total > 1 {
            let n = project_seen.entry(project.clone()).and_modify(|v| *v += 1).or_insert(1);
            *n
        } else { 0 };
        Session {
            source,
            project: trunc(&project, 22),
            model: e.model,
            run,
            last: fmt_ago(&e.last_dt, &now),
            tokens: fmt_tokens(e.tokens),
            cost: e.cost,
            active,
            count: suffix_n,    // 0 = no suffix; 1+ = (N) duplicate label
            waiting,
        }
    }).collect();

    st.sessions = if show_all {
        all
    } else {
        all.into_iter().filter(|s| s.active).collect()
    };

    refresh_quota(st);
}

fn refresh_quota(st: &mut Snapshot) {
    let path = format!(
        "{}\\AppData\\Local\\tokenusage\\live-frame-cache.json",
        std::env::var("USERPROFILE").unwrap_or_default()
    );
    let Ok(raw) = std::fs::read(&path) else { return; };
    let Ok(j) = serde_json::from_slice::<serde_json::Value>(&raw) else { return; };

    let cla_pct = j["official_claude"]["primary_used_percent"].as_f64().unwrap_or(0.0);
    let cla_wk  = j["official_claude"]["secondary_used_percent"].as_f64().unwrap_or(0.0);
    let cod_pct = j["official_codex"]["primary_used_percent"].as_f64().unwrap_or(0.0);
    let cod_wk  = j["official_codex"]["secondary_used_percent"].as_f64().unwrap_or(0.0);

    let mut segs: Vec<(String, Color)> = Vec::new();
    segs.push((format!("CC {:.0}%/", cla_pct), pct_color(cla_pct)));
    segs.push((format!("wk{:.0}%", cla_wk), pct_color(cla_wk)));
    segs.push(("  ".into(), Color::White));
    segs.push((format!("CDX {:.0}%/", cod_pct), pct_color(cod_pct)));
    segs.push((format!("wk{:.0}%", cod_wk), pct_color(cod_wk)));
    st.footer_segs = segs;
}

fn pct_color(v: f64) -> Color {
    if v >= 90.0 { Color::Red }
    else if v >= 50.0 { Color::Yellow }
    else { Color::White }
}

// ─── Helpers (duplicated from board_widget.rs) ────────────────────────────

fn run_tu(args: &[&str]) -> Option<serde_json::Value> {
    let mut cmd = std::process::Command::new("cmd");
    cmd.arg("/c").arg("tu").args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().ok()?;
    let raw = if !out.stdout.is_empty() { out.stdout } else { out.stderr };
    serde_json::from_slice(&raw).ok()
}

fn source_of(v: &serde_json::Value) -> String {
    if v.get("claude").is_some() { "claude".into() }
    else if v.get("codex").is_some() { "codex".into() }
    else { "?".into() }
}

fn detect_model(v: &serde_json::Value) -> String {
    let obj = v.as_object();
    let key = obj.and_then(|m| m.iter()
            .filter(|(k, _)| !k.to_ascii_lowercase().contains("haiku"))
            .max_by_key(|(_, val)| val["total_tokens"].as_u64().unwrap_or(0)))
        .or_else(|| obj.and_then(|m| m.iter()
            .max_by_key(|(_, val)| val["total_tokens"].as_u64().unwrap_or(0))))
        .map(|(k, _)| k.to_ascii_lowercase())
        .unwrap_or_default();

    for (needle, abbr) in &[("opus", "OPS"), ("sonnet", "SNT"), ("haiku", "HAI")] {
        if let Some(pos) = key.find(needle) {
            let after = &key[pos + needle.len()..];
            let ver: String = after.chars()
                .filter(|c| c.is_ascii_digit() || *c == '-')
                .take(5)
                .collect::<String>()
                .trim_matches('-')
                .replace('-', ".");
            let ver2: String = ver.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
            return if ver2.is_empty() { abbr.to_string() }
                   else { format!("{}{}", abbr, ver2) };
        }
    }
    if key.contains("codex") { return "CODEX".into(); }
    if key.starts_with("gpt-") {
        let ver: String = key[4..].chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        return if ver.is_empty() { "GPT".into() } else { format!("GPT-{}", ver) };
    }
    if let Some(rest) = key.strip_prefix('o').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())) {
        let ver: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.').collect();
        return format!("O{}", ver).to_uppercase();
    }
    key.chars().take(8).collect::<String>().to_uppercase()
}

/// Per-session JSONL lookup. session_id from tu maps directly to a JSONL path
/// under `~/.claude/projects/`. Returns (run_start, run_end) where
/// run_start = timestamp of last real user message, run_end = file mtime.
fn jsonl_run_for_session(session_id: &str)
    -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>
{
    let home = std::env::var("USERPROFILE").ok()?;
    let rel = session_id.replace('/', "\\");
    let path = std::path::PathBuf::from(format!(r"{}\.claude\projects\{}.jsonl", home, rel));

    let meta = std::fs::metadata(&path).ok()?;
    let mtime_sys = meta.modified().ok()?;

    let content = std::fs::read_to_string(&path).ok()?;
    let mut last_user_utc: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"].as_str() != Some("user") { continue; }
        if !is_real_user_input(&v["message"]["content"]) { continue; }
        let Some(ts) = v["timestamp"].as_str() else { continue };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else { continue };
        last_user_utc = Some(dt.with_timezone(&chrono::Utc));
    }
    let start_utc = last_user_utc?;

    let unix = mtime_sys.duration_since(std::time::UNIX_EPOCH).ok()?;
    let end_local = chrono::Local.timestamp_opt(unix.as_secs() as i64, 0).single()?;
    let start_local = start_utc.with_timezone(&chrono::Local).naive_local();
    let end_naive = end_local.naive_local();
    if end_naive < start_local { return None; }
    Some((start_local, end_naive))
}

fn is_real_user_input(content: &serde_json::Value) -> bool {
    if content.is_string() { return true; }
    if let Some(arr) = content.as_array() {
        let has_tool_result = arr.iter().any(|c| c["type"].as_str() == Some("tool_result"));
        if has_tool_result { return false; }
        return arr.iter().any(|c| c["type"].as_str() == Some("text"));
    }
    false
}

fn decode_project_name(raw: &str) -> String {
    let s = if let Some(pos) = raw.find("--") { &raw[pos + 2..] } else { raw };
    s.replace('-', "/")
}

fn fmt_duration(secs: i64) -> String {
    if secs < 60 { format!("{}s", secs) }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
}

fn fmt_tokens(t: u64) -> String {
    if t >= 1_000_000 { format!("{:.1}M", t as f64 / 1_000_000.0) }
    else if t >= 1_000 { format!("{:.1}K", t as f64 / 1_000.0) }
    else { format!("{}", t) }
}

fn fmt_ago(dt: &chrono::NaiveDateTime, now: &chrono::NaiveDateTime) -> String {
    let secs = (*now - *dt).num_seconds().max(0);
    if secs < 60 { "今".into() }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}

fn trunc(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max { s.to_string() }
    else { chars[..max - 1].iter().collect::<String>() + "…" }
}
