# AI Session Board

[English](README.md) · [繁體中文](README.zh-TW.md) · [简体中文](README.zh-CN.md)

Windows 上的 **Claude Code** 与 **Codex CLI** 实时 session 仪表板。

![AI Session Board 截图](docs/screenshot.png)

一个 binary 两种显示模式：
- **Win32 浮动窗口** — 永远置顶、可拖动、不会抢走键盘焦点
- **终端 TUI** — 可以塞进 tmux/psmux 的一个 pane 里常驻

显示每个 project 的 session 活动、实际任务执行时间、OAuth 配额用量、本月累计花费，以及 `claude*` 和 `codex*` 进程的实时 CPU/RAM 折线图。

## 状态一目了然

| 指示符 | 含义 |
|---|---|
| 绿 ● + 绿色 RUN | Agent 正在流式输出 token（cooking） |
| **黄 ● + 整行闪烁** | Agent 已停下 — **等你回复** |
| 灰 ○ + 暗色 | 空闲（过去 15 分钟无活动） |

同一个 project 有多个 session 时，会分别显示为独立的行并标上 `(1)`、`(2)`，方便一眼区分同时进行中的对话。

## 安装

```bash
cargo install ai-session-board
```

或从源码 build：

```bash
git clone https://github.com/warmbed/ai-session-board
cd ai-session-board
cargo build --release
# Binary: target/release/ai-board.exe
```

## 使用方式

```bash
ai-board              # Win32 浮动窗口
ai-board --tui        # 在当前终端运行 ratatui TUI
ai-board -t           # --tui 的简写
ai-board -h           # 帮助
```

### TUI 快捷键

| 按键 | 功能 |
|---|---|
| `q` / `Esc` / `Ctrl+C` | 退出 |
| `a` | 切换 24H / 仅显示 Active |
| `c` | 切换 CPU/RAM 图表 |
| `r` | 立即刷新 |

### 浮动窗口操作

- 在窗口任意位置按住拖动以移动
- 右键打开菜单：
  - **24H / Active Only** — 显示今天全部 session vs 只显示最近 15 分钟有活动的 session
  - **Charts ON / OFF** — 切换 CPU/RAM 图表
  - **Opacity** — 40% / 70% / 100% 透明度
  - **Brightness** — 80% / 100% / 130% / 160% 文字亮度倍率
  - **Size** — 50% / 75% / 100% / 150% / 200% DPI 缩放
  - **Close** — 关闭

窗口使用 `WS_EX_NOACTIVATE` 属性,所以点它不会抢走当前终端的键盘焦点。

## 数据来源

Board **不需要任何 server**，它直接读取：

| 来源 | 用途 |
|---|---|
| `tu session -j --since YYYYMMDD`（subprocess 调用） | session 列表、token 数、花费、模型 |
| `~/.claude/projects/**/*.jsonl` | 最后一条 user 消息的 timestamp + 文件 mtime → 精准的 RUN 时长 |
| `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`（通过 `tu`） | Codex sessions |
| `%LOCALAPPDATA%\tokenusage\live-frame-cache.json` | 底部配额（CC / CDX 百分比） |
| `sysinfo` crate | 名称含 `claude` / `codex` 的进程的 CPU/RAM |

### 为什么用 JSONL 的 mtime？

`tu session` 记录每条消息**收到**的时间，但 Claude 一个任务可以流式输出好几分钟的 token。文件的 mtime 在每次写入时都会更新，所以：

```
RUN = 文件 mtime − 最后一条"真用户输入"的 timestamp
```

可以拿到实际的 cooking 时长（对应 Claude Code 那个 "Cooked for 10m 26s"），而不是只算"用户发送 → assistant 开始回应"的瞬间。

JSONL 里 `type:"user"` 但实际是 tool result 的条目会被过滤掉 — 只有真人输入才 reset `run_start`。

## 系统要求

- Windows 10 / 11（Win32 浮动窗口仅支持 Windows；TUI 在任何终端都能跑）
- `PATH` 中有 [`tu`](https://github.com/...) CLI，用来抓 session 数据与配额
- 从源码 build 需要 Rust 1.74+

## 架构

```
src/
├── main.rs     # CLI 派发（--tui flag）
├── lib.rs      # 对外 API: run_floating(), run_tui()
├── widget.rs   # Win32 GDI 浮动窗口
└── tui.rs      # ratatui TUI + sysinfo 采样 thread
```

目前 widget.rs 与 tui.rs 各自有一份数据抓取逻辑；之后会把它整合进共用的 `data` 模块。

## 许可证

MIT — 详见 [LICENSE](LICENSE)。
