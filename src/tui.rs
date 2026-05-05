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
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table},
    Frame, Terminal,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const ACTIVE_MINS: i64 = 15;
const MAX_SESSIONS: usize = 12;
const REFRESH_SECS: u64 = 10;
const TU_TIMEOUT_SECS: u64 = 8;

#[derive(Clone)]
struct Session {
    source: String,
    project: String,
    project_full: String,
    session_id: String,     // matches watcher key & jsonl path
    model: String,
    run: String,
    last: String,
    tokens: String,
    cost: f64,
    active: bool,
    count: u32,
    waiting: bool,
    is_subagent: bool,
}

struct State {
    sessions: Vec<Session>,
    show_all: bool,
    footer_segs: Vec<(String, Color)>,
    mtd_cost: f64,
    loading: bool,
    show_subagents: bool, // toggleable with `s`
    tick: u64,            // increments each redraw — drives waiting-row flash
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

    let mut state = State {
        sessions: Vec::new(),
        show_all: false,
        footer_segs: Vec::new(),
        mtd_cost: 0.0,
        loading: true,
        show_subagents: false,
        tick: 0,
    };

    let stop_flag = Arc::new(AtomicBool::new(false));

    // Filesystem watcher: instant per-session activity detection.
    let watch_map: Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let watch_stop = spawn_jsonl_watcher(watch_map.clone());

    // Background worker: fetches snapshots without blocking the UI loop.
    // Triggered immediately, then on every RefreshRequest received via tx.
    let (req_tx, req_rx) = mpsc::channel::<RefreshRequest>();
    let (snap_tx, snap_rx) = mpsc::channel::<Snapshot>();
    {
        let stop = stop_flag.clone();
        thread::spawn(move || {
            let mut current_show_all = false;
            // Past-days MTD is frozen until midnight — compute once.
            let mut past_days_cost = fetch_past_days_cost();
            let mut cached_for_day = chrono::Local::now().date_naive();
            let mtd_cost = past_days_cost + read_today_cost();
            let _ = snap_tx.send(fetch_snapshot(current_show_all, mtd_cost));

            loop {
                if stop.load(Ordering::Relaxed) { break; }
                match req_rx.recv_timeout(Duration::from_secs(REFRESH_SECS)) {
                    Ok(req) => current_show_all = req.show_all,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if stop.load(Ordering::Relaxed) { break; }
                let today = chrono::Local::now().date_naive();
                if today != cached_for_day {
                    past_days_cost = fetch_past_days_cost();
                    cached_for_day = today;
                }
                let mtd_cost = past_days_cost + read_today_cost();
                if snap_tx.send(fetch_snapshot(current_show_all, mtd_cost)).is_err() { break; }
            }
        });
    }
    let result = (|| -> io::Result<()> {
        loop {
            // Drain any snapshots produced by the worker (non-blocking).
            while let Ok(snap) = snap_rx.try_recv() {
                state.sessions = snap.sessions;
                state.footer_segs = snap.footer_segs;
                state.mtd_cost = snap.mtd_cost;
                state.loading = false;
            }

            // Overlay watcher data into session active/waiting flags.
            apply_watcher_state(&mut state, &watch_map);

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
                                KeyCode::Char('s') => {
                                    state.show_subagents = !state.show_subagents;
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(3),    // table
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_title(f, chunks[0], st);
    render_table(f, chunks[1], st);
    render_footer(f, chunks[2], st);
}

fn render_title(f: &mut Frame, area: Rect, st: &State) {
    let mode = if st.show_all { " [24H]" } else { "" };
    let load = if st.loading { " ⏳" } else { "" };
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    let left = format!("■ AI SESSION BOARD ■{}{}", mode, load);
    let right = format!("{}    q quit · a 24H · s subagent · r refresh", now);
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

    // Filter subagents according to toggle, then take MAX_SESSIONS rows, then
    // recompute (N) duplicate suffixes on the visible set.
    let mut visible: Vec<Session> = st.sessions.iter()
        .filter(|s| st.show_subagents || !s.is_subagent)
        .take(MAX_SESSIONS)
        .cloned()
        .collect();
    let mut totals: HashMap<String, u32> = HashMap::new();
    for s in &visible { *totals.entry(s.project_full.clone()).or_insert(0) += 1; }
    let mut seen: HashMap<String, u32> = HashMap::new();
    for s in &mut visible {
        let total = totals.get(&s.project_full).copied().unwrap_or(1);
        s.count = if total > 1 {
            let n = seen.entry(s.project_full.clone()).and_modify(|v| *v += 1).or_insert(1);
            *n
        } else { 0 };
    }

    let rows: Vec<Row> = visible.iter().map(|s| {
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
fn fetch_snapshot(show_all: bool, mtd_cost: f64) -> Snapshot {
    let mut snap = Snapshot {
        sessions: Vec::new(),
        footer_segs: Vec::new(),
        mtd_cost,
    };
    do_fetch(&mut snap, show_all);
    snap
}

/// Sum the cost of every day from month-start up to (but not including) today.
/// Past days are frozen — call this once on startup, then only at midnight.
fn fetch_past_days_cost() -> f64 {
    let now = chrono::Local::now();
    let month_start = now.format("%Y%m01").to_string();
    let today_str = now.format("%Y-%m-%d").to_string();
    if let Some(json) = run_tu(&["daily", "-j", "--since", &month_start]) {
        if let Some(arr) = json["daily"].as_array() {
            return arr.iter()
                .filter(|d| d["date"].as_str().map(|s| s < today_str.as_str()).unwrap_or(false))
                .map(|d| d["totals"]["cost_usd"].as_f64().unwrap_or(0.0))
                .sum();
        }
    }
    0.0
}

/// Read today's cost from tu's live-frame-cache.json — tiny file, already
/// maintained by `tu live`, no subprocess needed.
fn read_today_cost() -> f64 {
    let path = format!(
        "{}\\AppData\\Local\\tokenusage\\live-frame-cache.json",
        std::env::var("USERPROFILE").unwrap_or_default()
    );
    let Ok(raw) = std::fs::read(&path) else { return 0.0 };
    let Ok(j) = serde_json::from_slice::<serde_json::Value>(&raw) else { return 0.0 };
    j["today_totals"]["cost_usd"].as_f64().unwrap_or(0.0)
}

fn do_fetch(st: &mut Snapshot, show_all: bool) {
    let now = chrono::Local::now().naive_local();
    let today = chrono::Local::now().format("%Y%m%d").to_string();

    // Today's sessions
    let Some(json) = run_tu(&["session", "-j", "--since", &today]) else { return; };

    struct ProjEntry {
        session_id: String,
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
                session_id: session_id.to_string(),
                active_first_dt: active_first,
                active_last_dt:  active_last,
                last_dt: effective_last,
                tokens, cost, sources, model,
                count: 1,
            }));
        }
    }

    entries.sort_by(|a, b| b.1.last_dt.cmp(&a.1.last_dt));
    // Keep extra rows (subagents may be hidden, then revealed by toggle).
    let sorted: Vec<(String, ProjEntry)> = entries.into_iter()
        .take(MAX_SESSIONS * 4)
        .collect();

    let all: Vec<Session> = sorted.into_iter().map(|(project, e)| {
        let last_secs = (now - e.last_dt).num_seconds().max(0);
        let active = last_secs <= ACTIVE_MINS * 60;
        let waiting = active && last_secs >= 60;
        let is_subagent = e.session_id.contains("/subagents/");
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
        // (N) suffix is computed at render time after the subagent/active filter.
        Session {
            source,
            project: trunc(&project, 22),
            project_full: project.clone(),
            session_id: e.session_id.clone(),
            model: e.model,
            run,
            last: fmt_ago(&e.last_dt, &now),
            tokens: fmt_tokens(e.tokens),
            cost: e.cost,
            active,
            count: 0,
            waiting,
            is_subagent,
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
    let out = run_with_timeout(cmd, Duration::from_secs(TU_TIMEOUT_SECS))?;
    let raw = if !out.stdout.is_empty() { out.stdout } else { out.stderr };
    serde_json::from_slice(&raw).ok()
}

fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::fs::File;
    use std::process::Stdio;

    let stamp = chrono::Local::now().timestamp_nanos_opt().unwrap_or_default();
    let base = std::env::temp_dir();
    let stdout_path = base.join(format!("ai-board-tu-{}-{}.out", std::process::id(), stamp));
    let stderr_path = base.join(format!("ai-board-tu-{}-{}.err", std::process::id(), stamp));
    let stdout_file = File::create(&stdout_path).ok()?;
    let stderr_file = File::create(&stderr_path).ok()?;

    cmd.stdout(Stdio::from(stdout_file)).stderr(Stdio::from(stderr_file));
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        if child.try_wait().ok()?.is_some() {
            let status = child.wait().ok()?;
            let stdout = std::fs::read(&stdout_path).unwrap_or_default();
            let stderr = std::fs::read(&stderr_path).unwrap_or_default();
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            return Some(std::process::Output { status, stdout, stderr });
        }
        if start.elapsed() >= timeout {
            kill_process_tree(child.id());
            let _ = child.wait();
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.output();
}

#[cfg(not(windows))]
fn kill_process_tree(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output();
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

/// Mtime-keyed cache so idle JSONL files aren't re-read every 5s.
static JSONL_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<
        std::path::PathBuf,
        (std::time::SystemTime, chrono::NaiveDateTime, chrono::NaiveDateTime),
    >>
> = std::sync::OnceLock::new();

/// Per-session JSONL lookup with mtime caching.
fn jsonl_run_for_session(session_id: &str)
    -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>
{
    let home = std::env::var("USERPROFILE").ok()?;
    let rel = session_id.replace('/', "\\");
    let path = std::path::PathBuf::from(format!(r"{}\.claude\projects\{}.jsonl", home, rel));

    let meta = std::fs::metadata(&path).ok()?;
    let mtime_sys = meta.modified().ok()?;

    // Cache hit: file unchanged since last parse.
    let cache = JSONL_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(c) = cache.lock() {
        if let Some((cached_mtime, s, e)) = c.get(&path) {
            if *cached_mtime == mtime_sys { return Some((*s, *e)); }
        }
    }

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

    if let Ok(mut c) = cache.lock() {
        c.insert(path, (mtime_sys, start_local, end_naive));
    }
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

// ─── Filesystem watcher (zero-polling activity detection) ────────────────

fn spawn_jsonl_watcher(map: Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>)
    -> Arc<AtomicBool>
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    thread::spawn(move || {
        use notify::{Watcher, RecursiveMode, EventKind};
        let Ok(home) = std::env::var("USERPROFILE") else { return };
        let projects_root = std::path::PathBuf::from(home.clone())
            .join(".claude").join("projects");
        let codex_root = std::path::PathBuf::from(home).join(".codex").join("sessions");

        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| { let _ = tx.send(res); }) {
            Ok(w) => w,
            Err(_) => return,
        };
        let _ = watcher.watch(&projects_root, RecursiveMode::Recursive);
        let _ = watcher.watch(&codex_root, RecursiveMode::Recursive);

        for evt in rx {
            if stop_w.load(Ordering::Relaxed) { break; }
            let Ok(event) = evt else { continue };
            if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) { continue; }
            for path in &event.paths {
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
                let Some(sid) = session_id_from_path(path, &projects_root) else { continue };
                if let Ok(mut m) = map.lock() {
                    m.insert(sid, Instant::now());
                }
            }
        }
    });
    stop
}

fn session_id_from_path(path: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.with_extension("").to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

fn apply_watcher_state(state: &mut State,
    map: &Arc<std::sync::Mutex<std::collections::HashMap<String, Instant>>>)
{
    let snap = map.lock().ok().map(|m| m.clone()).unwrap_or_default();
    let now = Instant::now();
    for s in &mut state.sessions {
        if let Some(last) = snap.get(&s.session_id) {
            let secs = now.duration_since(*last).as_secs();
            if secs < 5 {
                s.active = true; s.waiting = false;
            } else if secs < (ACTIVE_MINS as u64) * 60 {
                s.active = true; s.waiting = true;
            } else {
                s.active = false; s.waiting = false;
            }
        }
    }
}

