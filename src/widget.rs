#![cfg_attr(not(windows), allow(dead_code))]

/// Public entry point — launch the Win32 floating overlay.
#[cfg(windows)]
pub fn run() { win::main(); }

#[cfg(not(windows))]
pub fn run() { eprintln!("AI session board is only available on Windows."); }

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::mem::zeroed;
    use std::os::windows::process::CommandExt;
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    /// CREATE_NO_WINDOW — prevents the CMD console window from flashing
    /// every time we spawn `cmd /c tu session`.
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint,
        FillRect, GetDC, GetTextExtentPoint32W, ReleaseDC, HFONT, HGDIOBJ, PAINTSTRUCT,
        SelectObject, SetBkMode, SetTextColor, TextOutW,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, LoadCursorW, PostMessageW,
        PostQuitMessage, RegisterClassW, SetForegroundWindow,
        SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, TrackPopupMenu, TranslateMessage,
        CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, LWA_ALPHA, MF_CHECKED,
        MF_POPUP, MF_STRING, MF_UNCHECKED, MSG,
        SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        SW_SHOW, SWP_NOSIZE, SWP_NOZORDER, TPM_RIGHTBUTTON, WNDCLASSW,
        WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WM_TIMER,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    // ─── Menu constants ───────────────────────────────────────────────────────
    const MENU_CLOSE:        usize = 2001;
    const MENU_OPACITY_40:   usize = 2010;
    const MENU_OPACITY_70:   usize = 2011;
    const MENU_OPACITY_100:  usize = 2012;
    const MENU_SCALE_50:     usize = 2020;
    const MENU_SCALE_75:     usize = 2021;
    const MENU_SCALE_100:    usize = 2022;
    const MENU_SCALE_150:    usize = 2023;
    const MENU_SCALE_200:    usize = 2024;
    const MENU_TOGGLE_ALL:   usize = 2030;
    const MENU_TOGGLE_SUBAG: usize = 2032;
    const MENU_BRIGHT_80:    usize = 2040;
    const MENU_BRIGHT_100:   usize = 2041;
    const MENU_BRIGHT_130:   usize = 2042;
    const MENU_BRIGHT_160:   usize = 2043;

    // ─── Colors (COLORREF = 0x00BBGGRR) ──────────────────────────────────────
    const C_BG:      u32 = 0x060606;
    const C_BG_ALT:  u32 = 0x0D0D0D;
    const C_TITLE:   u32 = 0x00AAFF;
    const C_WHITE:   u32 = 0xE8E8E8;
    const C_HDR:     u32 = 0x909090;
    const C_SEP:     u32 = 0x2A2A2A;
    const C_ACTIVE:  u32 = 0x00E850;
    const C_IDLE:    u32 = 0x363636;
    const C_CLOCK:   u32 = 0x707070;
    const C_COST:    u32 = 0xA0A0A0;
    const C_DIM:     u32 = 0x606060;
    const C_AMBER:   u32 = 0x00BBFF;  // >50%: yellow (BGR)
    const C_WAIT:    u32 = 0x00DDFF;  // waiting-for-user yellow (BGR)
    const C_WAIT_DIM:u32 = 0x008CB0;  // dim phase for flashing
    const C_RED:     u32 = 0x3030FF;  // >90%: red (BGR)

    const TIMER_ID:     usize = 1;
    const TIMER_MS:     u32   = 1000;
    const REFRESH_SECS: u64   = 10;
    const TU_TIMEOUT_SECS: u64 = 8;
    const ACTIVE_MINS:  i64   = 15;
    const MAX_SESSIONS: usize = 10;

    // ─── Column X positions (base, at scale 1.0) ─────────────────────────────
    const CX_SRC:     i32 = 7;
    const CX_PROJECT: i32 = 60;
    const CX_MODEL:   i32 = 220;
    const CX_RUN:     i32 = 278;
    const CX_LAST:    i32 = 330;
    const CX_TOKENS:  i32 = 378;
    const CX_COST:    i32 = 440;
    const CX_DOT:     i32 = 495;
    const BOARD_BASE_W: i32 = 514;

    // ─── Session data ─────────────────────────────────────────────────────────
    #[derive(Clone)]
    struct Session {
        source:  String,
        project: String,
        project_full: String,
        session_id: String,
        model:   String,
        run:     String,
        last:    String,
        tokens:  String,
        cost:    f64,
        active:  bool,
        count:   u32,
        waiting: bool,
        /// Pulse the row only while it's a *recent* waiting state (LAST < 2m).
        /// After that, the row stays solid yellow so it doesn't keep flickering
        /// for the rest of the 15-minute "active" window.
        flashing: bool,
        is_subagent: bool,
    }

    /// Per-session timestamp of the last filesystem write, fed by the
    /// notify-rs watcher. Lets us flip a session into "cooking" the
    /// instant Claude appends a line, with no polling.
    type WatchMap = std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>;

    /// Snapshot produced by the background data worker so the UI thread never
    /// blocks on `tu session` subprocess calls or JSONL parsing.
    pub(super) struct Snapshot {
        all_sessions: Vec<Session>,
        footer_segs: Vec<(String, u32)>,
        mtd_cost: f64,
    }

    struct BoardApp {
        hwnd: HWND,
        x: i32, y: i32,
        dragging: bool,
        drag_offset_x: i32,
        drag_offset_y: i32,
        all_sessions: Vec<Session>,
        sessions: Vec<Session>,
        show_all: bool,
        footer_segs: Vec<(String, u32)>,
        mtd_cost: f64,
        tick: u32,
        opacity: u8,
        scale: f32,
        brightness: f32,
        font:   HFONT,
        font_b: HFONT,
        show_subagents: bool,
        watch_map: std::sync::Arc<WatchMap>,
        snapshot_rx: Option<std::sync::mpsc::Receiver<Snapshot>>,
        data_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        watch_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl Drop for BoardApp {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            if let Some(stop) = &self.data_stop  { stop.store(true, Ordering::Relaxed); }
            if let Some(stop) = &self.watch_stop { stop.store(true, Ordering::Relaxed); }
        }
    }

    impl BoardApp {
        fn new(hwnd: HWND) -> Self {
            let (x, y) = initial_pos();
            // Filesystem watcher gives us millisecond-accurate "last write per
            // session" without any polling — the OS notifies us.
            let watch_map: std::sync::Arc<WatchMap> =
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let watch_stop = spawn_jsonl_watcher(watch_map.clone());
            let (snapshot_rx, data_stop) = spawn_data_worker();
            let mut app = BoardApp {
                hwnd, x, y,
                dragging: false, drag_offset_x: 0, drag_offset_y: 0,
                all_sessions: vec![],
                sessions: vec![],
                show_all: false,
                footer_segs: vec![],
                mtd_cost: 0.0,
                tick: 0,
                opacity: 255,
                scale: 1.0,
                brightness: 1.0,
                font:   null_mut(),
                font_b: null_mut(),
                show_subagents: false,
                watch_map,
                snapshot_rx: Some(snapshot_rx),
                data_stop: Some(data_stop),
                watch_stop: Some(watch_stop),
            };
            app.rebuild_fonts();
            app
        }

        /// Drain any new snapshots produced by the background worker. Non-blocking.
        fn drain_snapshots(&mut self) {
            let Some(rx) = self.snapshot_rx.as_ref() else { return };
            let mut got_one = false;
            while let Ok(snap) = rx.try_recv() {
                self.all_sessions = snap.all_sessions;
                self.footer_segs = snap.footer_segs;
                self.mtd_cost = snap.mtd_cost;
                got_one = true;
            }
            if got_one {
                self.apply_filter();
                self.resize_window();
            }
        }

        fn rebuild_fonts(&mut self) {
            unsafe {
                if !self.font.is_null()   { DeleteObject(self.font as _); }
                if !self.font_b.is_null() { DeleteObject(self.font_b as _); }
            }
            let h = (-13.0 * self.scale) as i32;
            self.font   = make_font("MS Gothic", h, 400);
            self.font_b = make_font("MS Gothic", h - 1, 700);
        }

        fn board_w(&self) -> i32 { (BOARD_BASE_W as f32 * self.scale) as i32 }
        fn row_h(&self)   -> i32 { (21.0 * self.scale) as i32 }
        fn hdr_h(&self)   -> i32 { (30.0 * self.scale) as i32 }
        fn col_h(&self)   -> i32 { (18.0 * self.scale) as i32 }

        fn ftr_row(&self) -> i32 { (17.0 * self.scale) as i32 }

        fn board_h(&self) -> i32 {
            self.hdr_h() + 1 + self.col_h() + 1
                + (self.sessions.len().max(1) as i32) * self.row_h()
                + 1 + self.ftr_row()
                + (4.0 * self.scale) as i32
        }

        fn cx(&self, base: i32) -> i32 { (base as f32 * self.scale) as i32 }

        fn set_opacity(&mut self, v: u8) {
            self.opacity = v;
            unsafe { SetLayeredWindowAttributes(self.hwnd, 0, v, LWA_ALPHA); }
        }

        fn set_brightness(&mut self, b: f32) {
            self.brightness = b;
            unsafe { let hdc = GetDC(self.hwnd);
                if !hdc.is_null() { self.render(hdc); ReleaseDC(self.hwnd, hdc); }
            }
        }

        /// Scale every channel of a BGR color by `self.brightness`, clamped to 255.
        /// Used so the user can boost overall text brightness via the right-click menu.
        fn bright(&self, color: u32) -> u32 {
            if (self.brightness - 1.0).abs() < 0.01 { return color; }
            let f = self.brightness;
            let b = ((color & 0xFF) as f32 * f).min(255.0) as u32;
            let g = (((color >> 8) & 0xFF) as f32 * f).min(255.0) as u32;
            let r = (((color >> 16) & 0xFF) as f32 * f).min(255.0) as u32;
            (r << 16) | (g << 8) | b
        }

        fn set_scale(&mut self, s: f32) {
            self.scale = s.clamp(0.5, 2.0);
            self.rebuild_fonts();
            self.resize_window();
        }

        fn toggle_show_all(&mut self) {
            self.show_all = !self.show_all;
            self.apply_filter();
            self.resize_window();
        }

        fn toggle_subagents(&mut self) {
            self.show_subagents = !self.show_subagents;
            self.apply_filter();
            self.resize_window();
        }

        fn apply_filter(&mut self) {
            // 1. active / 24H filter
            // 2. subagent filter
            // 3. cap at MAX_SESSIONS rows
            // 4. compute (N) suffix labels on the visible set
            //
            // Real-time state from the filesystem watcher: if a JSONL was
            // written within the last few seconds, that session is currently
            // streaming ("cooking"). Between 5–900s it's waiting for user.
            let watch_snap: HashMap<String, std::time::Instant> = self.watch_map.lock()
                .ok().map(|m| m.clone()).unwrap_or_default();
            let now_inst = std::time::Instant::now();

            let mut filtered: Vec<Session> = self.all_sessions.iter()
                .filter(|s| self.show_all || s.active || s.waiting)
                .filter(|s| self.show_subagents || !s.is_subagent)
                .take(MAX_SESSIONS)
                .cloned()
                .collect();

            // Overlay watcher data into active/waiting flags. Thresholds match
            // what fmt_ago displays as LAST so the row color stays consistent
            // with what the user reads:
            //   LAST=今  (< 60s ago)        → cooking (white, green dot)
            //   LAST=1m..14m                → waiting (yellow). Pulses for the
            //                                 first 30s of waiting only — long
            //                                 enough to notice, short enough
            //                                 not to flicker indefinitely.
            //   LAST=15m+                   → idle (dim)
            for s in &mut filtered {
                if let Some(last_write) = watch_snap.get(&s.session_id) {
                    let secs = now_inst.duration_since(*last_write).as_secs();
                    if secs < 60 {
                        s.active = true;
                        s.waiting = false;
                        s.flashing = false;
                    } else if secs < (ACTIVE_MINS as u64) * 60 {
                        s.active = true;
                        s.waiting = true;
                        s.flashing = secs < 90; // flash window: 60s..90s
                    } else {
                        s.active = false;
                        s.waiting = false;
                        s.flashing = false;
                    }
                }
            }

            // Recompute duplicate counts on the filtered list.
            let mut project_total: HashMap<String, u32> = HashMap::new();
            for s in &filtered {
                *project_total.entry(s.project_full.clone()).or_insert(0) += 1;
            }
            let mut project_seen: HashMap<String, u32> = HashMap::new();
            for s in &mut filtered {
                let total = project_total.get(&s.project_full).copied().unwrap_or(1);
                if total > 1 {
                    let n = project_seen.entry(s.project_full.clone())
                        .and_modify(|v| *v += 1).or_insert(1);
                    s.count = *n;
                } else {
                    s.count = 0;
                }
            }
            self.sessions = filtered;
        }


        fn resize_window(&self) {
            unsafe {
                SetWindowPos(self.hwnd, null_mut(), self.x, self.y,
                    self.board_w(), self.board_h(), SWP_NOZORDER);
            }
        }

        fn on_timer(&mut self) {
            self.tick = self.tick.wrapping_add(1);
            self.drain_snapshots();
            // Re-apply filters every tick so watcher updates (cooking → waiting
            // transitions, new file appearing) flow into the UI within ~1s.
            self.apply_filter();
            unsafe {
                let hdc = GetDC(self.hwnd);
                if !hdc.is_null() { self.render(hdc); ReleaseDC(self.hwnd, hdc); }
            }
        }

    } // end impl BoardApp — free helpers below

    /// Sum the cost of every day from month-start up to (but not including)
    /// today. Past days are frozen — once cached, this never needs to be
    /// recomputed until the day rolls over at midnight.
    ///
    /// Uses `tu daily -j` instead of `tu session -j` because tu pre-aggregates
    /// per-day totals there, avoiding a per-session scan.
    fn fetch_past_days_cost() -> f64 {
        let now = chrono::Local::now();
        let month_start = now.format("%Y%m01").to_string();
        let today_str = now.format("%Y-%m-%d").to_string();

        let Some(jm) = run_tu_json(&["daily", "-j", "--since", &month_start]) else { return 0.0 };
        let Some(arr) = jm["daily"].as_array() else { return 0.0 };
        arr.iter()
            .filter(|d| d["date"].as_str().map(|s| s < today_str.as_str()).unwrap_or(false))
            .map(|d| d["totals"]["cost_usd"].as_f64().unwrap_or(0.0))
            .sum()
    }

    /// Read today's cost from tu's live-frame-cache.json — tiny file,
    /// already maintained by `tu live`, no subprocess needed.
    fn read_today_cost() -> f64 {
        let path = tokenusage_cache_path();
        let Ok(raw) = std::fs::read(path) else { return 0.0 };
        let Ok(j) = serde_json::from_slice::<serde_json::Value>(&raw) else { return 0.0 };
        j["today_totals"]["cost_usd"].as_f64().unwrap_or(0.0)
    }

    /// Build the today-only snapshot (sessions + quota footer). MTD cost is
    /// passed in by the caller because it changes slowly and is recomputed
    /// less frequently than the rest.
    fn build_snapshot(mtd_cost: f64) -> Option<Snapshot> {
        let today_str = chrono::Local::now().format("%Y%m%d").to_string();
        let json = run_tu_json(&["session", "-j", "--since", &today_str])?;
        let now = chrono::Local::now().naive_local();

            struct ProjEntry {
                session_id:      String,
                active_first_dt: Option<chrono::NaiveDateTime>,
                active_last_dt:  Option<chrono::NaiveDateTime>,
                last_dt:         chrono::NaiveDateTime,
                tokens:          u64,
                cost:            f64,
                sources:         std::collections::HashSet<String>,
                model:           String,
                count:           u32,
            }
            // Each tu session entry becomes its own row — no aggregation by project.
            // Two sessions in `code/psmux` will appear as two rows; the labeller
            // below adds (1)/(2) suffixes when there are duplicates.
            let mut entries: Vec<(String, ProjEntry)> = Vec::new();

            if let Some(arr) = json["sessions"].as_array() {
                for s in arr {
                    let project  = s["project"].as_str().unwrap_or("?").to_string();
                    let session_id = s["session_id"].as_str().unwrap_or("");
                    let last_str = s["last_activity"].as_str().unwrap_or("");
                    let Ok(last_dt) = chrono::NaiveDateTime::parse_from_str(
                        last_str, "%Y-%m-%d %H:%M:%S") else { continue; };

                    let tokens = s["totals"]["total_tokens"].as_u64().unwrap_or(0);
                    let cost   = s["totals"]["cost_usd"].as_f64().unwrap_or(0.0);
                    let src    = raw_source(&s["sources"]);
                    let model  = detect_model(&s["models"]);

                    // Per-session JSONL lookup — the file's mtime gives us the real
                    // streaming completion time, and the last user message gives us
                    // the real run_start. tu's last_activity alone collapses both to
                    // the same instant, making RUN = 0s.
                    let jsonl = jsonl_run_for_session(session_id);

                    // Always populate timing — even for inactive (>15min) sessions
                    // we want RUN to show the last actual duration, not "--".
                    let (active_first, active_last, effective_last) = match jsonl {
                        Some((start, end)) => (Some(start), Some(end), end.max(last_dt)),
                        None => (Some(last_dt), Some(last_dt), last_dt),
                    };

                    let mut sources = std::collections::HashSet::new();
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
            let sorted: Vec<(String, ProjEntry, bool)> = entries.into_iter()
                .take(MAX_SESSIONS * 4) // keep extras so subagent toggle has fuel
                .map(|(p, e)| {
                    let is_sub = e.session_id.contains("/subagents/");
                    (p, e, is_sub)
                })
                .collect();

        let all_sessions: Vec<Session> = sorted.into_iter().map(|(project, e, is_subagent)| {
                let last_secs = (now - e.last_dt).num_seconds().max(0);
                let active = last_secs <= ACTIVE_MINS * 60;
                // "Waiting" = active but not currently streaming. Threshold: a Claude
                // turn that finished within the last few seconds still counts as
                // running; once mtime hasn't advanced for 60s, the agent has clearly
                // stopped and is awaiting user input.
                let waiting = active && last_secs >= 60;
                let source = match (e.sources.contains("claude"), e.sources.contains("codex")) {
                    (true,  true)  => "BOTH".into(),
                    (true,  false) => "CLAUDE".into(),
                    (false, true)  => "CODEX".into(),
                    _              => "?".into(),
                };
                let run = match (e.active_first_dt, e.active_last_dt) {
                    (Some(start), Some(end)) => fmt_duration((end - start).num_seconds().max(0)),
                    _ => "--".into(),
                };
                // (N) suffix is computed in apply_filter() after subagent/active
                // filtering, so the numbering reflects only the visible rows.
                let flashing = waiting && last_secs < 90;
                Session {
                    source,
                    project: trunc(&project, 18),
                    project_full: project.clone(),
                    session_id: e.session_id.clone(),
                    model:   e.model,
                    run,
                    last:    fmt_ago(&e.last_dt, &now),
                    tokens:  fmt_tokens(e.tokens),
                    cost:    e.cost,
                    active,
                    count:   0,
                    waiting,
                    flashing,
                    is_subagent,
                }
        }).collect();

        // Quota footer (fast — small JSON file).
        let footer_segs = build_footer_segs(mtd_cost);

        Some(Snapshot { all_sessions, footer_segs, mtd_cost })
    }

    /// Read the live-frame-cache.json that `tu live` produces and turn it into
    /// the colored footer segments. Tiny file, safe to read every iteration.
    fn build_footer_segs(mtd_cost: f64) -> Vec<(String, u32)> {
        let cache = tokenusage_cache_path();
        let Ok(raw) = std::fs::read(cache) else { return Vec::new(); };
        let Ok(j) = serde_json::from_slice::<serde_json::Value>(&raw)
            else { return Vec::new(); };

        let cla_pct  = j["official_claude"]["primary_used_percent"].as_f64().unwrap_or(0.0);
        let cla_wk   = j["official_claude"]["secondary_used_percent"].as_f64().unwrap_or(0.0);
        let cod_pct  = j["official_codex"]["primary_used_percent"].as_f64().unwrap_or(0.0);
        let cod_wk   = j["official_codex"]["secondary_used_percent"].as_f64().unwrap_or(0.0);
        let today    = j["today_totals"]["cost_usd"].as_f64().unwrap_or(0.0);

        let reset_at = j["official_claude"]["primary_resets_at"].as_i64().unwrap_or(0);
        let now_unix = chrono::Local::now().timestamp();
        let left = (reset_at - now_unix).max(0);
        let reset = if left >= 3600 {
            format!("{}h{}m", left / 3600, (left % 3600) / 60)
        } else { format!("{}m", left / 60) };

        let cached = j["cached_at_unix"].as_i64().unwrap_or(0);
        let age_m = (now_unix - cached) / 60;
        let stale = if age_m > 5 { format!(" !{}m", age_m) } else { String::new() };

        vec![
            ("CC ".into(),                          C_HDR),
            (format!("{:.0}%", cla_pct),            pct_color(cla_pct)),
            ("/wk".into(),                          C_HDR),
            (format!("{:.0}%", cla_wk),             pct_color(cla_wk)),
            ("  CDX ".into(),                       C_HDR),
            (format!("{:.0}%", cod_pct),            pct_color(cod_pct)),
            ("/wk".into(),                          C_HDR),
            (format!("{:.0}%", cod_wk),             pct_color(cod_wk)),
            (format!("  RST {}  今${:.0}  MTD${}{}",
                reset, today, fmt_kilo(mtd_cost), stale), C_HDR),
        ]
    }

    /// Background worker thread — produces snapshots every REFRESH_SECS without
    /// blocking the UI thread. The UI thread polls the Receiver via try_recv.
    fn spawn_data_worker() -> (
        std::sync::mpsc::Receiver<Snapshot>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Snapshot>();
        let stop_w = stop.clone();
        std::thread::spawn(move || {
            // Past-days MTD is *frozen* (Jan 1 → yesterday will never change).
            // Compute once on startup, then only when the day rolls over.
            let mut past_days_cost = fetch_past_days_cost();
            let mut cached_for_day = chrono::Local::now().date_naive();

            // Today's cost comes from tu's tiny live-frame-cache.json — no
            // subprocess needed at all on subsequent refreshes.
            let mtd = past_days_cost + read_today_cost();

            if let Some(snap) = build_snapshot(mtd) {
                if tx.send(snap).is_err() { return; }
            }
            loop {
                if stop_w.load(Ordering::Relaxed) { break; }
                std::thread::sleep(Duration::from_secs(REFRESH_SECS));
                if stop_w.load(Ordering::Relaxed) { break; }

                let today = chrono::Local::now().date_naive();
                if today != cached_for_day {
                    past_days_cost = fetch_past_days_cost();
                    cached_for_day = today;
                }
                let mtd = past_days_cost + read_today_cost();
                if let Some(snap) = build_snapshot(mtd) {
                    if tx.send(snap).is_err() { break; }
                }
            }
        });
        (rx, stop)
    }

    fn tokenusage_cache_path() -> std::path::PathBuf {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        std::path::PathBuf::from(home)
            .join("AppData")
            .join("Local")
            .join("tokenusage")
            .join("live-frame-cache.json")
    }

    fn run_tu_json(args: &[&str]) -> Option<serde_json::Value> {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/c").arg("tu").args(args);
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

    fn kill_process_tree(pid: u32) {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.output();
    }

    impl BoardApp {
        fn start_drag(&mut self, rel_x: i32, rel_y: i32) {
            self.dragging = true;
            self.drag_offset_x = rel_x;
            self.drag_offset_y = rel_y;
            unsafe { SetCapture(self.hwnd); }
        }

        fn drag_to_cursor(&mut self) {
            unsafe {
                let mut pt = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut pt) == 0 { return; }
                self.x = pt.x - self.drag_offset_x;
                self.y = pt.y - self.drag_offset_y;
                self.clamp();
                SetWindowPos(self.hwnd, null_mut(), self.x, self.y, 0, 0,
                    SWP_NOZORDER | SWP_NOSIZE);
            }
        }

        fn finish_drag(&mut self) {
            self.dragging = false;
            unsafe { ReleaseCapture(); }
        }

        fn clamp(&mut self) {
            unsafe {
                let min_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
                let min_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
                let max_x = min_x + GetSystemMetrics(SM_CXVIRTUALSCREEN);
                let max_y = min_y + GetSystemMetrics(SM_CYVIRTUALSCREEN);
                self.x = self.x.clamp(min_x, max_x - self.board_w());
                self.y = self.y.clamp(min_y, max_y - self.board_h());
            }
        }

        fn render(&self, hdc: *mut std::ffi::c_void) {
            unsafe {
                let bw = self.board_w();
                let bh = self.board_h();
                let hh = self.hdr_h();
                let ch = self.col_h();
                let rh = self.row_h();
                let y_col  = hh + 1;
                let y_data = y_col + ch + 1;

                fill(hdc, 0, 0, bw, bh, C_BG);

                // ── Header ────────────────────────────────────────────────
                let old = SelectObject(hdc, self.font_b as HGDIOBJ);
                SetBkMode(hdc, 1);
                SetTextColor(hdc, self.bright(C_TITLE));
                let title = if self.show_all { "\u{25A0} AI SESSION BOARD \u{25A0}  [24H]" }
                            else             { "\u{25A0} AI SESSION BOARD \u{25A0}" };
                txt(hdc, self.cx(CX_SRC), (hh - (14.0 * self.scale) as i32) / 2, title);
                SetTextColor(hdc, self.bright(C_CLOCK));
                txt(hdc, self.cx(378), (hh - (14.0 * self.scale) as i32) / 2,
                    &chrono::Local::now().format("%H:%M:%S").to_string());

                // ── Separators ────────────────────────────────────────────
                fill(hdc, 0, hh, bw, 1, C_SEP);
                fill(hdc, 0, y_col + ch, bw, 1, C_SEP);

                // ── Column headers ────────────────────────────────────────
                SelectObject(hdc, self.font as HGDIOBJ);
                SetTextColor(hdc, self.bright(C_HDR));
                for &(base_x, label) in &[
                    (CX_SRC,     "SRC"),
                    (CX_PROJECT, "PROJECT"),
                    (CX_MODEL,   "MODEL"),
                    (CX_RUN,     "RUN"),
                    (CX_LAST,    "LAST"),
                    (CX_TOKENS,  "TOKENS"),
                    (CX_COST,    "COST"),
                    (CX_DOT,     "\u{25CF}"),
                ] {
                    txt(hdc, self.cx(base_x), y_col + 2, label);
                }

                // ── Rows ──────────────────────────────────────────────────
                if self.sessions.is_empty() {
                    SetTextColor(hdc, self.bright(C_DIM));
                    let msg = if self.show_all { "  NO SESSIONS TODAY" }
                              else { "  NO ACTIVE SESSIONS (右クリック→24H)" };
                    txt(hdc, self.cx(CX_SRC), y_data + 4, msg);
                } else {
                    // Tick alternates each second so waiting rows pulse between
                    // bright and dim yellow.
                    let flash_bright = (self.tick / 1) % 2 == 0;
                    for (i, s) in self.sessions.iter().enumerate() {
                        let ry = y_data + i as i32 * rh;
                        let ty = ry + 3;
                        if i % 2 == 1 { fill(hdc, 0, ry, bw, rh, C_BG_ALT); }

                        // Row color: pulse only while `flashing` (first 30s of
                        // waiting); after that, solid yellow until idle.
                        let row_color = if s.waiting {
                            if s.flashing && !flash_bright { C_WAIT_DIM } else { C_WAIT }
                        } else if s.active {
                            C_WHITE
                        } else {
                            C_DIM
                        };
                        let text_col = self.bright(row_color);
                        SetTextColor(hdc, text_col);
                        txt(hdc, self.cx(CX_SRC),     ty, &s.source);
                        // count == 0 → unique project (no suffix); count >= 1 → "(N)" duplicate label
                        let suffix = if s.count >= 1 { format!("({})", s.count) } else { String::new() };
                        let proj_label = format!("{}{}", trunc(&s.project, 18 - suffix.len()), suffix);
                        txt(hdc, self.cx(CX_PROJECT),  ty, &proj_label);
                        txt(hdc, self.cx(CX_MODEL),    ty, &s.model);

                        // RUN column: green when running, yellow when waiting, dim otherwise
                        let run_col = if s.waiting { row_color }
                            else if s.active { C_ACTIVE } else { C_DIM };
                        SetTextColor(hdc, self.bright(run_col));
                        txt(hdc, self.cx(CX_RUN),  ty, &s.run);

                        SetTextColor(hdc, text_col);
                        txt(hdc, self.cx(CX_LAST),   ty, &s.last);
                        txt(hdc, self.cx(CX_TOKENS), ty, &s.tokens);

                        let cost_col = if s.waiting { row_color }
                            else if s.active { C_COST } else { C_IDLE };
                        SetTextColor(hdc, self.bright(cost_col));
                        txt(hdc, self.cx(CX_COST), ty, &format!("{:.2}", s.cost));

                        // Indicator dot: green ● running, yellow ● waiting, grey ○ idle
                        let dot_col = if s.waiting { row_color }
                            else if s.active { C_ACTIVE } else { C_IDLE };
                        SetTextColor(hdc, self.bright(dot_col));
                        txt(hdc, self.cx(CX_DOT), ty,
                            if s.active { "\u{25CF}" } else { "\u{25CB}" });
                    }
                }
                // ── Footer ───────────────────────────────────────────────
                let y_sep = y_data + (self.sessions.len().max(1) as i32) * rh;
                fill(hdc, 0, y_sep, bw, 1, C_SEP);
                SelectObject(hdc, self.font as HGDIOBJ);
                if !self.footer_segs.is_empty() {
                    let fy = y_sep + 2;
                    let mut fx = self.cx(CX_SRC);
                    for (text, color) in &self.footer_segs {
                        SetTextColor(hdc, self.bright(*color));
                        txt(hdc, fx, fy, text);
                        let wtext: Vec<u16> = text.encode_utf16().collect();
                        let mut sz = SIZE { cx: 0, cy: 0 };
                        GetTextExtentPoint32W(hdc, wtext.as_ptr(), wtext.len() as i32, &mut sz);
                        fx += sz.cx;
                    }
                }

                SelectObject(hdc, old);
            }
        }
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    unsafe fn fill(hdc: *mut std::ffi::c_void, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let br = CreateSolidBrush(c);
        FillRect(hdc, &RECT { left: x, top: y, right: x + w, bottom: y + h }, br);
        DeleteObject(br as _);
    }

    unsafe fn txt(hdc: *mut std::ffi::c_void, x: i32, y: i32, s: &str) {
        let w = wide(s);
        TextOutW(hdc, x, y, w.as_ptr(), (w.len() - 1) as i32);
    }

    fn pct_color(v: f64) -> u32 {
        if v >= 90.0 { C_RED } else if v >= 50.0 { C_AMBER } else { C_HDR }
    }

    fn make_font(name: &str, height: i32, weight: i32) -> HFONT {
        unsafe { CreateFontW(height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 4, 0x31,
            wide(name).as_ptr()) }
    }

    fn initial_pos() -> (i32, i32) {
        unsafe {
            let min_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let min_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w     = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            (min_x + w - 530, min_y + 20)
        }
    }

    /// Mtime-keyed cache of (run_start, run_end) per JSONL file. Refresh
    /// reads ~/.claude/projects/ for *every* tu session; without this cache
    /// idle sessions cause megabytes of disk IO every 5 seconds, which makes
    /// the host (psmux) feel sluggish.
    static JSONL_CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<
            std::path::PathBuf,
            (std::time::SystemTime, chrono::NaiveDateTime, chrono::NaiveDateTime),
        >>
    > = std::sync::OnceLock::new();

    /// Look up a single tu session_id's JSONL file and return (run_start, run_end).
    /// Caches results by mtime so unchanged files are never re-parsed.
    fn jsonl_run_for_session(session_id: &str)
        -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>
    {
        use chrono::TimeZone;
        let home = std::env::var("USERPROFILE").ok()?;
        let rel = session_id.replace('/', "\\");
        let path = std::path::PathBuf::from(format!(r"{}\.claude\projects\{}.jsonl", home, rel));

        let meta = std::fs::metadata(&path).ok()?;
        let mtime_sys = meta.modified().ok()?;

        // Cache hit: mtime hasn't changed since last parse.
        let cache = JSONL_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        if let Ok(c) = cache.lock() {
            if let Some((cached_mtime, s, e)) = c.get(&path) {
                if *cached_mtime == mtime_sys { return Some((*s, *e)); }
            }
        }

        // Cache miss: read and parse.
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

    /// True only for messages the human actually typed.
    /// Skips tool_result entries (which are also stored with role "user").
    fn is_real_user_input(content: &serde_json::Value) -> bool {
        // Plain string content = real user typing
        if content.is_string() { return true; }
        // Array form: real if ANY item is type=text and NO item is tool_result
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

    /// Spawn a filesystem watcher on `~/.claude/projects/` recursively. Every
    /// time Claude appends to a JSONL, the OS notifies us; we record the
    /// session_id (= relative path without .jsonl) → Instant in the map.
    /// The UI thread reads this map to flip rows into "cooking" within ms,
    /// without any polling.
    fn spawn_jsonl_watcher(map: std::sync::Arc<WatchMap>)
        -> std::sync::Arc<std::sync::atomic::AtomicBool>
    {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::time::Instant;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        std::thread::spawn(move || {
            use notify::{Watcher, RecursiveMode, EventKind};
            let Ok(home) = std::env::var("USERPROFILE") else { return };
            let projects_root = std::path::PathBuf::from(home)
                .join(".claude").join("projects");

            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            }) {
                Ok(w) => w,
                Err(_) => return,
            };
            if watcher.watch(&projects_root, RecursiveMode::Recursive).is_err() {
                return;
            }

            // Codex sessions live elsewhere; watch that too if present.
            let _ = std::env::var("USERPROFILE").ok().map(|h| {
                let codex_root = std::path::PathBuf::from(h).join(".codex").join("sessions");
                let _ = watcher.watch(&codex_root, RecursiveMode::Recursive);
            });

            for evt in rx {
                if stop_w.load(std::sync::atomic::Ordering::Relaxed) { break; }
                let Ok(event) = evt else { continue };
                if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) { continue; }
                for path in &event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
                    if let Some(session_id) = session_id_from_path(path, &projects_root) {
                        if let Ok(mut m) = map.lock() {
                            m.insert(session_id, Instant::now());
                        }
                    }
                }
            }
        });
        stop
    }

    /// Convert an absolute JSONL path back into the `session_id` form tu uses.
    /// e.g. `~/.claude/projects/d--code-x/UUID.jsonl` → `d--code-x/UUID`
    ///      `.../d--code-x/UUID/subagents/agent-y.jsonl` → `d--code-x/UUID/subagents/agent-y`
    fn session_id_from_path(path: &std::path::Path, root: &std::path::Path) -> Option<String> {
        let rel = path.strip_prefix(root).ok()?;
        let s = rel.with_extension("").to_string_lossy().replace('\\', "/");
        if s.is_empty() { None } else { Some(s) }
    }

    fn raw_source(v: &serde_json::Value) -> String {
        if v.get("claude").is_some() { "claude".into() }
        else if v.get("codex").is_some() { "codex".into() }
        else { "?".into() }
    }

    fn detect_model(v: &serde_json::Value) -> String {
        // Prefer the non-Haiku model with most tokens (Haiku is usually background tasks
        // like /compact, title generation, summarization). Fall back to Haiku only if
        // it's the only model present.
        let obj = v.as_object();
        let key = obj.and_then(|m| m.iter()
                .filter(|(k, _)| !k.to_ascii_lowercase().contains("haiku"))
                .max_by_key(|(_, val)| val["total_tokens"].as_u64().unwrap_or(0)))
            .or_else(|| obj.and_then(|m| m.iter()
                .max_by_key(|(_, val)| val["total_tokens"].as_u64().unwrap_or(0))))
            .map(|(k, _)| k.to_ascii_lowercase())
            .unwrap_or_default();

        // Claude: extract family + major.minor version
        // e.g. "claude-sonnet-4-6" → "SNT4.6", "claude-opus-4-7" → "OPS4.7"
        for (needle, abbr) in &[("opus","OPS"), ("sonnet","SNT"), ("haiku","HAI")] {
            if let Some(pos) = key.find(needle) {
                let after = &key[pos + needle.len()..];
                // find digits like "-4-6" or "-4-7"
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
        if key.contains("codex")  { return "CODEX".into(); }

        // GPT / O-series: extract version number precisely
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

    fn fmt_ago(dt: &chrono::NaiveDateTime, now: &chrono::NaiveDateTime) -> String {
        let secs = (*now - *dt).num_seconds().max(0);
        if secs < 60        { "今".into() }
        else if secs < 3600 { format!("{}m", secs / 60) }
        else                { format!("{}h", secs / 3600) }
    }

    fn fmt_duration(secs: i64) -> String {
        if secs < 60        { format!("{}s", secs) }
        else if secs < 3600 { format!("{}m", secs / 60) }
        else {
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            if m == 0 { format!("{}h", h) } else { format!("{}h{}m", h, m) }
        }
    }

    fn fmt_tokens(n: u64) -> String {
        if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
        else if n >= 1_000 { format!("{:.1}K", n as f64 / 1_000.0) }
        else { n.to_string() }
    }

    fn trunc(s: &str, max: usize) -> String {
        let v: Vec<char> = s.chars().collect();
        if v.len() <= max { s.into() }
        else { v[..max - 1].iter().collect::<String>() + "\u{2026}" }
    }

    fn fmt_kilo(v: f64) -> String {
        if v >= 10_000.0      { format!("{:.0}K", v / 1000.0) }
        else if v >= 1_000.0  { format!("{:.1}K", v / 1000.0) }
        else                  { format!("{:.0}", v) }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ─── Context menu ─────────────────────────────────────────────────────────

    unsafe fn show_context_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        if menu.is_null() { return; }

        let (opacity, scale, show_all, brightness, show_subagents) = app_ref(hwnd)
            .map(|a| (a.opacity, a.scale, a.show_all, a.brightness, a.show_subagents))
            .unwrap_or((255, 1.0, false, 1.0, false));

        // Toggle 24h / active-only
        let toggle_label = if show_all { "表示: 24H \u{2714}" } else { "表示: Active Only" };
        AppendMenuW(menu, MF_STRING, MENU_TOGGLE_ALL, wide(toggle_label).as_ptr());

        // Toggle subagent visibility
        let sub_label = if show_subagents { "Subagents: ON \u{2714}" } else { "Subagents: OFF" };
        AppendMenuW(menu, MF_STRING, MENU_TOGGLE_SUBAG, wide(sub_label).as_ptr());

        // Opacity submenu
        let opacity_menu = CreatePopupMenu();
        if !opacity_menu.is_null() {
            let chk = |v: u8| if opacity == v { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(opacity_menu, MF_STRING | chk(102), MENU_OPACITY_40,  wide("40%").as_ptr());
            AppendMenuW(opacity_menu, MF_STRING | chk(178), MENU_OPACITY_70,  wide("70%").as_ptr());
            AppendMenuW(opacity_menu, MF_STRING | chk(255), MENU_OPACITY_100, wide("100%").as_ptr());
            AppendMenuW(menu, MF_POPUP, opacity_menu as usize, wide("Opacity").as_ptr());
        }

        // Scale submenu
        let scale_menu = CreatePopupMenu();
        if !scale_menu.is_null() {
            let chk = |s: f32| if (scale - s).abs() < 0.05 { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(scale_menu, MF_STRING | chk(0.5),  MENU_SCALE_50,  wide("50%").as_ptr());
            AppendMenuW(scale_menu, MF_STRING | chk(0.75), MENU_SCALE_75,  wide("75%").as_ptr());
            AppendMenuW(scale_menu, MF_STRING | chk(1.0),  MENU_SCALE_100, wide("100%").as_ptr());
            AppendMenuW(scale_menu, MF_STRING | chk(1.5),  MENU_SCALE_150, wide("150%").as_ptr());
            AppendMenuW(scale_menu, MF_STRING | chk(2.0),  MENU_SCALE_200, wide("200%").as_ptr());
            AppendMenuW(menu, MF_POPUP, scale_menu as usize, wide("Size").as_ptr());
        }

        // Brightness submenu — multiplies all text colors so text reads brighter
        // even when window opacity is at 100% but the dark colors look muted.
        let bright_menu = CreatePopupMenu();
        if !bright_menu.is_null() {
            let chk = |v: f32| if (brightness - v).abs() < 0.05 { MF_CHECKED } else { MF_UNCHECKED };
            AppendMenuW(bright_menu, MF_STRING | chk(0.8), MENU_BRIGHT_80,  wide("80%").as_ptr());
            AppendMenuW(bright_menu, MF_STRING | chk(1.0), MENU_BRIGHT_100, wide("100%").as_ptr());
            AppendMenuW(bright_menu, MF_STRING | chk(1.3), MENU_BRIGHT_130, wide("130%").as_ptr());
            AppendMenuW(bright_menu, MF_STRING | chk(1.6), MENU_BRIGHT_160, wide("160%").as_ptr());
            AppendMenuW(menu, MF_POPUP, bright_menu as usize, wide("Brightness").as_ptr());
        }

        AppendMenuW(menu, MF_STRING, MENU_CLOSE, wide("Close").as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            SetForegroundWindow(hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
            PostMessageW(hwnd, 0, 0, 0);
        }
        DestroyMenu(menu);
    }

    // ─── Window proc ──────────────────────────────────────────────────────────

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_ERASEBKGND => 1,

            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(app) = app_ref(hwnd) { app.render(hdc); }
                EndPaint(hwnd, &ps);
                0
            }

            WM_LBUTTONDOWN => {
                if let Some(app) = app_mut(hwnd) {
                    let (x, y) = lp_xy(lparam);
                    app.start_drag(x, y);
                }
                0
            }

            WM_MOUSEMOVE => {
                if let Some(app) = app_mut(hwnd) {
                    if app.dragging { app.drag_to_cursor(); }
                }
                0
            }

            WM_LBUTTONUP => {
                if let Some(app) = app_mut(hwnd) {
                    if app.dragging { app.finish_drag(); }
                }
                0
            }

            WM_RBUTTONUP => { show_context_menu(hwnd); 0 }

            WM_COMMAND => {
                let id = wparam & 0xffff;
                match id {
                    MENU_CLOSE        => { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd); }
                    MENU_TOGGLE_ALL   => { if let Some(a) = app_mut(hwnd) { a.toggle_show_all(); } }
                    MENU_TOGGLE_SUBAG  => { if let Some(a) = app_mut(hwnd) { a.toggle_subagents(); } }
                    MENU_OPACITY_40   => { if let Some(a) = app_mut(hwnd) { a.set_opacity(102); } }
                    MENU_OPACITY_70   => { if let Some(a) = app_mut(hwnd) { a.set_opacity(178); } }
                    MENU_OPACITY_100  => { if let Some(a) = app_mut(hwnd) { a.set_opacity(255); } }
                    MENU_BRIGHT_80    => { if let Some(a) = app_mut(hwnd) { a.set_brightness(0.8); } }
                    MENU_BRIGHT_100   => { if let Some(a) = app_mut(hwnd) { a.set_brightness(1.0); } }
                    MENU_BRIGHT_130   => { if let Some(a) = app_mut(hwnd) { a.set_brightness(1.3); } }
                    MENU_BRIGHT_160   => { if let Some(a) = app_mut(hwnd) { a.set_brightness(1.6); } }
                    MENU_SCALE_50     => { if let Some(a) = app_mut(hwnd) { a.set_scale(0.5);  } }
                    MENU_SCALE_75     => { if let Some(a) = app_mut(hwnd) { a.set_scale(0.75); } }
                    MENU_SCALE_100    => { if let Some(a) = app_mut(hwnd) { a.set_scale(1.0);  } }
                    MENU_SCALE_150    => { if let Some(a) = app_mut(hwnd) { a.set_scale(1.5);  } }
                    MENU_SCALE_200    => { if let Some(a) = app_mut(hwnd) { a.set_scale(2.0);  } }
                    _ => {}
                }
                0
            }

            WM_TIMER => {
                if wparam == TIMER_ID {
                    if let Some(app) = app_mut(hwnd) { app.on_timer(); }
                }
                0
            }

            WM_DESTROY => { PostQuitMessage(0); 0 }

            WM_NCDESTROY => {
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if ptr != 0 { drop(Box::from_raw(ptr as *mut BoardApp)); }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn app_mut(hwnd: HWND) -> Option<&'static mut BoardApp> {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 { None } else { Some(&mut *(p as *mut BoardApp)) }
    }

    unsafe fn app_ref(hwnd: HWND) -> Option<&'static BoardApp> {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 { None } else { Some(&*(p as *const BoardApp)) }
    }

    fn lp_xy(lparam: LPARAM) -> (i32, i32) {
        let raw = lparam as u32;
        ((raw as u16) as i16 as i32, ((raw >> 16) as u16) as i16 as i32)
    }

    pub fn main() {
        unsafe {
            let instance   = GetModuleHandleW(null());
            let class_name = wide("psmux_board_widget");
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..zeroed()
            };
            RegisterClassW(&wc);

            let (x, y) = initial_pos();
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                wide("psmux board").as_ptr(),
                WS_POPUP,
                x, y, 630, 100,
                null_mut(), null_mut(), instance, null_mut(),
            );
            if hwnd.is_null() { return; }

            SetLayeredWindowAttributes(hwnd, 0, 220, LWA_ALPHA);

            let app = Box::new(BoardApp::new(hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
            ShowWindow(hwnd, SW_SHOW);
            SetTimer(hwnd, TIMER_ID, TIMER_MS, None);

            let mut msg: MSG = zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

