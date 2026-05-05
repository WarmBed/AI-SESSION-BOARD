# AI Session Board

[English](README.md) · [繁體中文](README.zh-TW.md) · [简体中文](README.zh-CN.md)

Windows 上的 **Claude Code** 與 **Codex CLI** 即時 session 儀表板。

![AI Session Board 截圖](docs/screenshot.png)

一個 binary 兩種顯示模式：
- **Win32 浮動視窗** — 永遠置頂、可拖曳、不會搶走鍵盤焦點
- **終端機 TUI** — 可以塞進 tmux/psmux 的一個 pane 裡常駐

顯示每個 project 的 session 活動、實際任務執行時間、OAuth 配額用量、本月累計花費，以及 `claude*` 和 `codex*` process 的即時 CPU/RAM 折線圖。

## 狀態一目了然

| 指示符 | 意義 |
|---|---|
| 綠 ● + 綠色 RUN | Agent 正在串流 token（cooking） |
| **黃 ● + 整列閃爍** | Agent 已停下 — **等你回覆** |
| 灰 ○ + 暗色 | 閒置（過去 15 分鐘無活動） |

同一個 project 有多個 session 時，會分別顯示為獨立的列並標上 `(1)`、`(2)`，方便一眼分辨同時進行中的對話。

## 安裝

```bash
cargo install ai-session-board
```

或從原始碼 build：

```bash
git clone https://github.com/warmbed/ai-session-board
cd ai-session-board
cargo build --release
# Binary: target/release/ai-board.exe
```

## 使用方式

```bash
ai-board              # Win32 浮動視窗
ai-board --tui        # 在當前 terminal 跑 ratatui TUI
ai-board -t           # --tui 的縮寫
ai-board -h           # 說明
```

### TUI 快捷鍵

| 按鍵 | 功能 |
|---|---|
| `q` / `Esc` / `Ctrl+C` | 結束 |
| `a` | 切換 24H / 只顯示 Active |
| `c` | 切換 CPU/RAM 圖表 |
| `r` | 立即重新整理 |

### 浮動視窗操作

- 在視窗任意位置按住拖曳以移動
- 右鍵打開選單：
  - **24H / Active Only** — 顯示今天全部 session vs 只顯示最近 15 分鐘有活動的 session
  - **Charts ON / OFF** — 切換 CPU/RAM 圖表
  - **Opacity** — 40% / 70% / 100% 透明度
  - **Brightness** — 80% / 100% / 130% / 160% 文字亮度倍率
  - **Size** — 50% / 75% / 100% / 150% / 200% DPI 縮放
  - **Close** — 關閉

視窗使用 `WS_EX_NOACTIVATE` 屬性，所以點它不會搶走當前 terminal 的鍵盤焦點。

## 資料來源

Board **不需要任何 server**，它直接讀取：

| 來源 | 用途 |
|---|---|
| `tu session -j --since YYYYMMDD`（subprocess 呼叫） | session 列表、token 數、花費、模型 |
| `~/.claude/projects/**/*.jsonl` | 最後一筆 user 訊息的 timestamp + 檔案 mtime → 精準的 RUN 時長 |
| `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`（透過 `tu`） | Codex sessions |
| `%LOCALAPPDATA%\tokenusage\live-frame-cache.json` | 底部配額（CC / CDX 百分比） |
| `sysinfo` crate | 名稱含 `claude` / `codex` 的 process 的 CPU/RAM |

### 為什麼用 JSONL 的 mtime？

`tu session` 記錄每筆訊息**收到**的時間，但 Claude 一個任務可以串流好幾分鐘的 token。檔案的 mtime 在每次寫入時都會更新，所以：

```
RUN = 檔案 mtime − 最後一筆「真使用者輸入」的 timestamp
```

可以拿到實際的 cooking 時長（對應 Claude Code 那個 「Cooked for 10m 26s」），而不是只算「user 送出 → assistant 開始回應」的瞬間。

JSONL 裡 `type:"user"` 但實際是 tool result 的條目會被過濾掉 — 只有真人輸入會 reset `run_start`。

## 系統需求

- Windows 10 / 11（Win32 浮動視窗只支援 Windows；TUI 在任何 terminal 都能跑）
- `PATH` 中有 [`tu`](https://github.com/...) CLI，用來抓 session 資料與配額
- 從原始碼 build 需要 Rust 1.74+

## 架構

```
src/
├── main.rs     # CLI 派發（--tui flag）
├── lib.rs      # 對外 API: run_floating(), run_tui()
├── widget.rs   # Win32 GDI 浮動視窗
└── tui.rs      # ratatui TUI + sysinfo 採樣 thread
```

目前 widget.rs 與 tui.rs 各自有一份資料抓取邏輯；之後會把它整合進共用的 `data` 模組。

## 授權

MIT — 詳見 [LICENSE](LICENSE)。
