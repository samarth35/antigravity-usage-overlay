# Antigravity Usage Overlay

A lightweight, always-on-top Windows floating overlay built in Rust (`eframe` / `egui`) to monitor your **Antigravity** 5-Hour and Weekly quota remaining, live token burn, and model usage.

## Download

Grab the latest standalone Windows executable from the **[Releases](https://github.com/samarth35/antigravity-usage-overlay/releases/latest)** page (`antigravity-usage-overlay.exe` — no installer or runtime dependencies required).

## Features

- **Collapsible Glass Orb (`62×62`):** Dual concentric progress rings (outer = **Weekly**, inner = **5-Hour**) colored green (`≥ 50%`), amber (`20%–49%`), or rose (`< 20%`). Drag anywhere on screen; click to expand.
- **Hover Peek (`232×66`):** Hover over the orb to preview 5-Hour & Weekly remaining percentages and reset countdowns.
- **Compact Expanded Card (`258×224`):**
  - **`Usage` tab:** 5-Hour & Weekly remaining bars, reset countdowns, **`Auto Sync`**, **`Manual`**, and **`Refresh`** buttons, plus today's token & turn summary.
  - **`Details` tab:** Today's `Input / Output / Cache` breakdown, Cache Hit %, active 5h & 7d totals, live burn rate (`tokens/hr`), and top 3 models.
  - **`Options` tab:** **`Auto`** background sync toggle, **`Opacity`** slider (`65%–100%`), **`Top`** (Always-on-Top), **`Peek`** (hover peek), **`Poll`** interval (`3s / 5s / 10s`), plus manual/clipboard quota sync.
- **Automatic Live Sync:** Simply open the **Models** page in `Antigravity.exe` once — **Auto Sync** automatically captures your 5-Hour & Weekly percentages and reset timers via Windows UI Automation (`IUIAutomation`) and built-in Windows OCR (`Windows.Media.Ocr`). After that one capture, it continues tracking your live usage automatically from `~/.gemini/antigravity/brain/*/.system_generated/logs/transcript.jsonl` even when you leave the Models page (and re-syncs automatically whenever you visit it again).

## Controls

| Action | Result |
| --- | --- |
| **Hover orb** | Show compact 5h / Weekly peek pill |
| **Click orb** | Expand into compact card |
| **Drag orb or card title** | Reposition anywhere on screen |
| **Click mini-orb / `-` / `Esc`** | Collapse back to orb |
| **Click `Sync` / `Auto Sync`** | Scan open Antigravity window for live quota |
| **`Ctrl+V` (when expanded)** | Paste-sync copied Antigravity Settings quota text |
| **Right-click** | Open context menu (`Expand`, `Auto-Sync`, `Details`, `Options`, `Refresh`, `Quit`) |

## Build from Source

Requires Rust (`x86_64-pc-windows-msvc` + MSVC Build Tools or `xwin` SDK):

```powershell
cargo test --release
cargo build --release
```

The compiled binary is output to `target\release\antigravity-usage-overlay.exe`.
