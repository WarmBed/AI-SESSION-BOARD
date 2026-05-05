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
    const MENU_TOGGLE_CHARTS:usize = 2031;
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
    const REFRESH_SECS: u64   = 5;
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
        project_full: String,  // untruncated, used for duplicate counting after filter
        model:   String,
        run:     String,
        last:    String,
        tokens:  String,
        cost:    f64,
        active:  bool,
        count:   u32,           // (N) suffix; 0 = no suffix. Set by apply_filter().
        waiting: bool,
        is_subagent: bool,      // detected from session_id containing "/subagents/"
    }

    // ─── App ──────────────────────────────────────────────────────────────────
    const HISTORY_LEN: usize = 60;
    const CHART_TARGETS: &[(&str, u32)] = &[
        ("claude", 0x00CCAA00), // cyan-ish (BGR)
        ("codex",  0x0000DDDD), // yellow-ish (BGR)
    ];

    struct BoardApp {
        hwnd: HWND,
        x: i32, y: i32,
        dragging: bool,
        drag_offset_x: i32,
        drag_offset_y: i32,
        all_sessions: Vec<Session>,
        sessions: Vec<Session>,
        show_all: bool,
        footer_segs: Vec<(String, u32)>, // colored quota segments
        mtd_cost: f64,                 // month-to-date cost from session records
        last_refresh: Instant,
        tick: u32,
        opacity: u8,
        scale: f32,
        brightness: f32,
        font:   HFONT,
        font_b: HFONT,
        show_subagents: bool,   // when false, sessions with "/subagents/" in id are hidden
        // CPU/RAM charts
        show_charts: bool,
        cpu_history: Vec<Vec<f64>>,
        ram_history: Vec<Vec<f64>>,
        proc_counts: Vec<usize>,
        sample_rx: Option<std::sync::mpsc::Receiver<(Vec<f64>, Vec<f64>, Vec<usize>)>>,
        sample_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl BoardApp {
        fn new(hwnd: HWND) -> Self {
            let (x, y) = initial_pos();
            let n = CHART_TARGETS.len();
            let (sample_rx, sample_stop) = spawn_proc_sampler();
            let mut app = BoardApp {
                hwnd, x, y,
                dragging: false, drag_offset_x: 0, drag_offset_y: 0,
                all_sessions: vec![],
                sessions: vec![],
                show_all: false,
                footer_segs: vec![],
                mtd_cost: 0.0,
                last_refresh: Instant::now() - Duration::from_secs(10),
                tick: 0,
                opacity: 255,
                scale: 1.0,
                brightness: 1.0,
                font:   null_mut(),
                font_b: null_mut(),
                show_charts: true,
                show_subagents: false,
                cpu_history: vec![vec![0.0; HISTORY_LEN]; n],
                ram_history: vec![vec![0.0; HISTORY_LEN]; n],
                proc_counts: vec![0; n],
                sample_rx: Some(sample_rx),
                sample_stop: Some(sample_stop),
            };
            app.rebuild_fonts();
            app
        }

        unsafe fn draw_chart(&self, hdc: *mut std::ffi::c_void,
                             x: i32, y: i32, w: i32, h: i32, is_cpu: bool, max_y: f64) {
            // Title with colored legend: "CPU %  ●claude 5.0%  ●codex 0.3%"
            let title = if is_cpu { "CPU %" } else { "RAM %" };
            SetTextColor(hdc, self.bright(C_HDR));
            txt(hdc, x + 4, y + 1, title);

            // Draw colored dots + labels for legend
            let title_w: i32 = {
                let wt: Vec<u16> = title.encode_utf16().collect();
                let mut sz = SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(hdc, wt.as_ptr(), wt.len() as i32, &mut sz);
                sz.cx
            };
            // Show the most recent value for each target instead of process counts.
            let history_set = if is_cpu { &self.cpu_history } else { &self.ram_history };
            let mut lx = x + 4 + title_w + 8;
            for (i, (name, color)) in CHART_TARGETS.iter().enumerate() {
                let v = history_set.get(i).and_then(|h| h.last().copied()).unwrap_or(0.0);
                let label = format!("\u{25CF} {} {:.1}%", name, v);
                SetTextColor(hdc, self.bright(*color));
                txt(hdc, lx, y + 1, &label);
                let lw: Vec<u16> = label.encode_utf16().collect();
                let mut sz = SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(hdc, lw.as_ptr(), lw.len() as i32, &mut sz);
                lx += sz.cx + 8;
            }

            // Border
            fill(hdc, x, y + (15.0 * self.scale) as i32, w, 1, C_SEP);

            let plot_x = x + 4;
            let plot_y = y + (16.0 * self.scale) as i32;
            let plot_w = w - 8;
            let plot_h = h - (18.0 * self.scale) as i32;
            if plot_w < 4 || plot_h < 4 { return; }

            // Y-axis labels
            SetTextColor(hdc, self.bright(C_DIM));
            txt(hdc, x + 4, plot_y + plot_h - (12.0 * self.scale) as i32, "0");
            let max_label = format!("{:.0}", max_y);
            txt(hdc, x + 4, plot_y, &max_label);

            let n = HISTORY_LEN;
            for (i, history) in history_set.iter().enumerate() {
                let color = CHART_TARGETS[i].1;
                use windows_sys::Win32::Graphics::Gdi::{CreatePen, MoveToEx, LineTo, PS_SOLID};
                let pen = CreatePen(PS_SOLID as i32, 1, color);
                let old_pen = SelectObject(hdc, pen as _);
                let mut first = true;
                for (k, &val) in history.iter().enumerate() {
                    let px = plot_x + (k as i32 * plot_w) / (n as i32 - 1).max(1);
                    let py = plot_y + plot_h
                        - ((val / max_y).clamp(0.0, 1.0) * plot_h as f64) as i32;
                    if first {
                        MoveToEx(hdc, px, py, std::ptr::null_mut());
                        first = false;
                    } else {
                        LineTo(hdc, px, py);
                    }
                }
                SelectObject(hdc, old_pen);
                DeleteObject(pen as _);
            }
        }

        fn drain_samples(&mut self) {
            let Some(rx) = self.sample_rx.as_ref() else { return };
            while let Ok((cpu, ram, counts)) = rx.try_recv() {
                for i in 0..CHART_TARGETS.len() {
                    let prev_cpu = std::mem::take(&mut self.cpu_history[i]);
                    self.cpu_history[i] = prev_cpu.into_iter().skip(1)
                        .chain(std::iter::once(cpu[i])).collect();
                    let prev_ram = std::mem::take(&mut self.ram_history[i]);
                    self.ram_history[i] = prev_ram.into_iter().skip(1)
                        .chain(std::iter::once(ram[i])).collect();
                }
                self.proc_counts = counts;
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

        fn chart_h(&self) -> i32 {
            if self.show_charts { (90.0 * self.scale) as i32 } else { 0 }
        }

        fn board_h(&self) -> i32 {
            self.hdr_h() + 1 + self.col_h() + 1
                + (self.sessions.len().max(1) as i32) * self.row_h()
                + 1 + self.chart_h()
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

        fn toggle_charts(&mut self) {
            self.show_charts = !self.show_charts;
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
            let mut filtered: Vec<Session> = self.all_sessions.iter()
                .filter(|s| self.show_all || s.active)
                .filter(|s| self.show_subagents || !s.is_subagent)
                .take(MAX_SESSIONS)
                .cloned()
                .collect();

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

        fn refresh_quota(&mut self) {
            // Read tu's live-frame-cache.json directly — same data source as `tu live`
            let cache = r"C:\Users\mike2\AppData\Local\tokenusage\live-frame-cache.json";
            let Ok(raw) = std::fs::read(cache) else { return; };
            let Ok(j) = serde_json::from_slice::<serde_json::Value>(&raw) else { return; };

            let cla_pct  = j["official_claude"]["primary_used_percent"].as_f64().unwrap_or(0.0);
            let cla_wk   = j["official_claude"]["secondary_used_percent"].as_f64().unwrap_or(0.0);
            let cod_pct  = j["official_codex"]["primary_used_percent"].as_f64().unwrap_or(0.0);
            let cod_wk   = j["official_codex"]["secondary_used_percent"].as_f64().unwrap_or(0.0);
            let today = j["today_totals"]["cost_usd"].as_f64().unwrap_or(0.0);

            let reset_at = j["official_claude"]["primary_resets_at"].as_i64().unwrap_or(0);
            let now_unix = chrono::Local::now().timestamp();
            let left = (reset_at - now_unix).max(0);
            let reset = if left >= 3600 {
                format!("{}h{}m", left / 3600, (left % 3600) / 60)
            } else {
                format!("{}m", left / 60)
            };

            let cached = j["cached_at_unix"].as_i64().unwrap_or(0);
            let age_m = (now_unix - cached) / 60;
            let stale = if age_m > 5 { format!(" !{}m", age_m) } else { String::new() };

            self.footer_segs = vec![
                ("CC ".into(),                          C_HDR),
                (format!("{:.0}%", cla_pct),            pct_color(cla_pct)),
                ("/wk".into(),                          C_HDR),
                (format!("{:.0}%", cla_wk),             pct_color(cla_wk)),
                ("  CDX ".into(),                       C_HDR),
                (format!("{:.0}%", cod_pct),            pct_color(cod_pct)),
                ("/wk".into(),                          C_HDR),
                (format!("{:.0}%", cod_wk),             pct_color(cod_wk)),
                (format!("  RST {}  今${:.0}  MTD${}{}",
                    reset, today, fmt_kilo(self.mtd_cost), stale), C_HDR),
            ];
        }

        fn resize_window(&self) {
            unsafe {
                SetWindowPos(self.hwnd, null_mut(), self.x, self.y,
                    self.board_w(), self.board_h(), SWP_NOZORDER);
            }
        }

        fn on_timer(&mut self) {
            self.tick = self.tick.wrapping_add(1);
            self.drain_samples();
            if self.last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
                self.refresh();
                self.last_refresh = Instant::now();
                self.resize_window();
            }
            // quota refresh every 30s
            if self.tick % 30 == 1 { self.refresh_quota(); }
            unsafe {
                let hdc = GetDC(self.hwnd);
                if !hdc.is_null() { self.render(hdc); ReleaseDC(self.hwnd, hdc); }
            }
        }

        fn refresh(&mut self) {
            let today      = chrono::Local::now().format("%Y%m%d").to_string();
            let month_start = chrono::Local::now().format("%Y%m01").to_string();

            // MTD cost: sum all session costs since the 1st of this month
            if let Ok(mo) = std::process::Command::new("cmd")
                .args(["/c", "tu", "session", "-j", "--since", &month_start])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                let raw_mo = if !mo.stdout.is_empty() { mo.stdout } else { mo.stderr };
                if let Ok(jm) = serde_json::from_slice::<serde_json::Value>(&raw_mo) {
                    if let Some(arr) = jm["sessions"].as_array() {
                        self.mtd_cost = arr.iter()
                            .map(|s| s["totals"]["cost_usd"].as_f64().unwrap_or(0.0))
                            .sum();
                    }
                }
            }

            let Ok(out) = std::process::Command::new("cmd")
                .args(["/c", "tu", "session", "-j", "--since", &today])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            else { return; };

            let raw = if !out.stdout.is_empty() { out.stdout } else { out.stderr };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&raw)
            else { return; };

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

            self.all_sessions = sorted.into_iter().map(|(project, e, is_subagent)| {
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
                Session {
                    source,
                    project: trunc(&project, 18),
                    project_full: project.clone(),
                    model:   e.model,
                    run,
                    last:    fmt_ago(&e.last_dt, &now),
                    tokens:  fmt_tokens(e.tokens),
                    cost:    e.cost,
                    active,
                    count:   0,
                    waiting,
                    is_subagent,
                }
            }).collect();

            self.apply_filter();
        }

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

                        // Choose row text color: waiting > active > idle.
                        let row_color = if s.waiting {
                            if flash_bright { C_WAIT } else { C_WAIT_DIM }
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
                // ── Charts (CPU / RAM) ───────────────────────────────────
                let y_after_rows = y_data + (self.sessions.len().max(1) as i32) * rh;
                let mut y_chart_end = y_after_rows;
                if self.show_charts {
                    fill(hdc, 0, y_after_rows, bw, 1, C_SEP);
                    let ch_h = self.chart_h();
                    let half_w = bw / 2;
                    // Shared Y scale across CPU and RAM so the 0 baseline and
                    // visual scale match. Floor at 20% so small values remain
                    // visible without the line glued to the bottom edge.
                    let max_cpu = self.cpu_history.iter()
                        .flat_map(|h| h.iter().copied())
                        .fold(0.0_f64, f64::max);
                    let max_ram = self.ram_history.iter()
                        .flat_map(|h| h.iter().copied())
                        .fold(0.0_f64, f64::max);
                    let shared_max = max_cpu.max(max_ram).ceil().max(20.0);
                    self.draw_chart(hdc, 0, y_after_rows + 1, half_w, ch_h, true, shared_max);
                    self.draw_chart(hdc, half_w, y_after_rows + 1, bw - half_w, ch_h, false, shared_max);
                    y_chart_end = y_after_rows + 1 + ch_h;
                }

                // ── Footer ───────────────────────────────────────────────
                let y_sep = y_chart_end;
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

    /// Look up a single tu session_id's JSONL file and return (run_start, run_end).
    ///
    /// `session_id` from `tu session -j` looks like:
    ///   - "d--code-psmux/UUID"                           (main conversation)
    ///   - "d--code-psmux/UUID/subagents/agent-XXXX"      (subagent)
    ///
    /// Both map directly to a JSONL path:
    ///   ~/.claude/projects/<session_id>.jsonl
    ///
    /// run_start = timestamp of the last real user message in that file
    /// run_end   = file mtime (Claude appends as it streams; mtime ≈ last token written)
    fn jsonl_run_for_session(session_id: &str)
        -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>
    {
        use chrono::TimeZone;
        let home = std::env::var("USERPROFILE").ok()?;
        let rel = session_id.replace('/', "\\");
        let path = format!(r"{}\.claude\projects\{}.jsonl", home, rel);
        let path = std::path::PathBuf::from(path);

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

    /// Spawn a 1Hz background thread that samples CPU% and RAM% (as % of total
    /// system memory) for processes whose name contains one of CHART_TARGETS.
    /// Each tick sends `(cpu_pcts, ram_pcts, counts)` for the targets via channel.
    fn spawn_proc_sampler() -> (
        std::sync::mpsc::Receiver<(Vec<f64>, Vec<f64>, Vec<usize>)>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_w = stop.clone();
        std::thread::spawn(move || {
            // System::new() doesn't refresh memory, so total_memory() returns 0
            // and every RAM percentage divides by zero. new_all() fully primes
            // the system info on first construction.
            let mut sys = sysinfo::System::new_all();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let total_ram_mb = sys.total_memory() as f64 / 1024.0 / 1024.0;
            // sysinfo's cpu_usage() returns % of one core; divide by core count
            // so a fully-pegged 16-thread process is shown as 100% (system CPU).
            let core_count = sys.cpus().len().max(1) as f64;
            loop {
                if stop_w.load(Ordering::Relaxed) { break; }
                std::thread::sleep(Duration::from_millis(1000));
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                let n = CHART_TARGETS.len();
                let mut cpu = vec![0.0f64; n];
                let mut ram = vec![0.0f64; n];
                let mut counts = vec![0usize; n];
                for proc in sys.processes().values() {
                    let name = proc.name().to_string_lossy().to_lowercase();
                    for (i, (target, _)) in CHART_TARGETS.iter().enumerate() {
                        if name.contains(target) {
                            cpu[i] += proc.cpu_usage() as f64 / core_count;
                            let rss_mb = proc.memory() as f64 / 1024.0 / 1024.0;
                            if total_ram_mb > 0.0 {
                                ram[i] += rss_mb / total_ram_mb * 100.0;
                            }
                            counts[i] += 1;
                            break;
                        }
                    }
                }
                if tx.send((cpu, ram, counts)).is_err() { break; }
            }
        });
        (rx, stop)
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

        let (opacity, scale, show_all, show_charts, brightness, show_subagents) = app_ref(hwnd)
            .map(|a| (a.opacity, a.scale, a.show_all, a.show_charts, a.brightness, a.show_subagents))
            .unwrap_or((255, 1.0, false, true, 1.0, false));

        // Toggle 24h / active-only
        let toggle_label = if show_all { "表示: 24H \u{2714}" } else { "表示: Active Only" };
        AppendMenuW(menu, MF_STRING, MENU_TOGGLE_ALL, wide(toggle_label).as_ptr());

        // Toggle CPU/RAM charts
        let chart_label = if show_charts { "Charts: ON \u{2714}" } else { "Charts: OFF" };
        AppendMenuW(menu, MF_STRING, MENU_TOGGLE_CHARTS, wide(chart_label).as_ptr());

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
                    MENU_TOGGLE_CHARTS => { if let Some(a) = app_mut(hwnd) { a.toggle_charts(); } }
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

