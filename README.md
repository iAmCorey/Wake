# Wake

[![License](https://img.shields.io/github/license/iAmCorey/Wake?style=flat-square)](LICENSE)
[![Release](https://img.shields.io/github/v/release/iAmCorey/Wake?style=flat-square)](https://github.com/iAmCorey/Wake/releases/latest)
[![Platform](https://img.shields.io/badge/platform-macOS%2014%2B%20%7C%20Windows%2010%2B-007AFF?style=flat-square)](https://github.com/iAmCorey/Wake/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/iAmCorey/Wake/total?style=flat-square)](https://github.com/iAmCorey/Wake/releases)
[![Stars](https://img.shields.io/github/stars/iAmCorey/Wake?style=flat-square)](https://github.com/iAmCorey/Wake/stargazers)
[![CI](https://img.shields.io/github/actions/workflow/status/iAmCorey/Wake/ci.yml?branch=main&label=CI&style=flat-square)](https://github.com/iAmCorey/Wake/actions/workflows/ci.yml)

A native desktop app for macOS and Windows that gathers every coding-agent session on your machine into one place — browse, full-text search, and resume any conversation in seconds. Built with **Rust + GPUI** (gpui 0.2 + gpui-component 0.5).

Your agent history is scattered across `~/.claude`, `~/.codex`, and nine other private directories. Wake reads them all, read-only, and gives you one fast window into it. Everything stays local: no network requests, ever.

![Wake — sessions list and transcript view](imgs/screenshot-1.webp)

## Features

- **Unified browsing** — all sessions grouped by agent / project, live file watching for incremental updates
- **Full-text search** (⌘K on macOS, Ctrl+K on Windows) — SQLite FTS5 trigram index; handles CJK text and code substrings (like `useEffect(`) equally well; jumps straight to the matched message in the transcript
- **Transcript view** — per-message rendering with user/assistant bubbles, collapsible tool-call clusters, thinking summaries, tree-sitter code highlighting (30+ languages)
- **One-click resume** — reopens the session in Terminal/iTerm on macOS, or Windows Terminal/PowerShell on Windows, at the original project directory (`claude --resume`, `codex resume`, …)
- **Manage** — star/pin (stored in Wake's own DB, original files untouched), export to Markdown, delete (system Trash/Recycle Bin + tombstone so deleted sessions stay deleted)

![Full-text search across every agent's sessions](imgs/screenshot-2.webp)

## Supported agents

| Agent | Data source | Model | Via |
|---|---|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` | ✅ | — |
| Codex CLI | `~/.codex/sessions` + `state_5.sqlite` (read-only) | ✅ | ✅ |
| Copilot CLI | `~/.copilot/session-store.db` | — | — |
| Cursor (CLI transcripts) | `~/.cursor/projects/**/agent-transcripts` | — | — |
| OpenCode | `~/.local/share/opencode/opencode.db` | ✅ | — |
| OpenCode 2 (`opencode2`, beta) | same DB as v1, new `session_v2` tables | ✅ | — |
| Kiro | `~/.kiro/sessions/cli` | ✅ | — |
| Gemini CLI | `~/.gemini/tmp/**/chats` | — | — |
| Pi | `~/.pi/agent/sessions/**/*.jsonl` | ✅ | — |
| Oh My Pi | `~/.omp/agent/sessions/**/*.jsonl` | ✅ | — |
| Grok Build | `~/.grok/sessions/**/updates.jsonl` | ✅ | — |
| Kimi Code | `~/.kimi-code/sessions/**/wire.jsonl` | — | — |
| Antigravity CLI | `~/.gemini/antigravity-cli/conversation_summaries.db` (metadata only — transcripts are encrypted) | — | — |

**Model** = whether Wake shows which LLM a session used (the model the session last used). **Via** = whether Wake shows where the session was started from (CLI, IDE extension, desktop app) — only Codex records this in its local data. A "—" means the agent's local data simply doesn't record that field, not a missing feature.

Cursor IDE chats, Windsurf, and Trae encrypt their local data; Amp, Factory (Droid), and Warp keep sessions in the cloud — none of those are supported. Reasonix stores sessions locally but hasn't been mapped yet.

## Privacy stance

- Agent data directories are opened **read-only**; Wake never writes to another tool's files or databases
- Credential files (`auth.json` and friends) are never read
- Zero network requests — Wake never constructs or calls an HTTP client (GPUI's dependency tree bundles one; Wake doesn't reach for it)
- Wake's own index lives at the platform data directory (`~/Library/Application Support/wake/wake.db` on macOS, `%LOCALAPPDATA%\wake\wake.db` on Windows) and can be rebuilt from scratch at any time (stars/pins live in a separate table and survive rebuilds)

## Performance

On the author's machine (~310 sessions, ~800 MB of JSONL): full index ~5 s, subsequent launches are instant (mtime-based incremental scan), search results in under 1 ms.

## Install

Build from source (requires a Rust toolchain):

```bash
git clone https://github.com/iAmCorey/Wake && cd Wake
scripts/make-app.sh          # builds dist/Wake.app (icon + Info.plist, ad-hoc signed)
open dist/Wake.app
```

On Windows, run PowerShell from the repository root:

```powershell
.\scripts\build.ps1
.\scripts\build.ps1 -Run
```

The Windows build produces `dist\Wake.exe`. Session resume uses Windows Terminal when available, with PowerShell and Command Prompt as fallbacks. Session deletion uses the Windows Recycle Bin.

To create an installable Windows MSI with the Wake icon, run:

```powershell
.\scripts\package-windows.ps1
```

This produces `dist\Wake-0.2.1-x64.msi`, installs Wake under `Program Files`, and creates Start Menu and desktop shortcuts. The script bootstraps the WiX 6 CLI with the .NET SDK when it is not already installed. The source artwork is `crates\wake\assets\icon-source.png`; `scripts\make-icon.py` creates the multi-resolution `icon.ico` used by the EXE and installer.

On macOS, the app is ad-hoc signed, so Gatekeeper may block the first launch — right-click the app and choose *Open*, or run `xattr -d com.apple.quarantine Wake.app`.

## Development

```bash
cargo run -p wake                      # run in dev mode
scripts/test.sh                        # one-command test entry: data-layer tests + UI compile gate
.\scripts\test.ps1                    # Windows test entry: data-layer tests + UI compile gate
scripts/test.sh --smoke                # adds a real-data scan baseline (reads your local agent dirs, read-only)
cargo test -p wake-core                # data-layer tests only (adapter contracts, FTS, scanner)
cargo run -p wake-core --bin scan      # data-layer smoke test: scan and print stats
cargo run -p wake-core --bin scan -- --search "useEffect("   # search smoke test
WAKE_THEME=dark cargo run -p wake      # force dark/light (defaults to system)
git config core.hooksPath scripts/hooks   # optional: run tests before every commit
python3 scripts/demo-home.py           # build a synthetic fake-home dataset for screenshots/demos
```

CI runs `cargo test -p wake-core` plus a full app build on every push to main and every PR. The test suite parses synthetic fixture sessions only — your real agent data is never touched.

## Architecture

```
crates/
├── wake-core        # pure data layer, no UI dependencies
│   ├── adapters/    #   claude / codex / copilot / cursor / opencode / kiro / gemini
│   │                #   pi / omp / grok / kimi / antigravity
│   │                #   (AgentAdapter trait — add an adapter, get the whole UI for free)
│   ├── scanner.rs   #   single-pass scan: meta + FTS in one go, mtime incremental
│   ├── watcher.rs   #   notify-based file watching → per-file incremental updates
│   ├── db.rs        #   rusqlite (WAL): sessions / messages / messages_fts / user_data / tombstones
│   └── services/    #   platform terminal resume / export / system trash
└── wake             # GPUI app (three-pane workbench + ⌘K palette)
```

Design notes live in [DESIGN.md](DESIGN.md), product decisions in [PRODUCT.md](PRODUCT.md).

## Star History

[![Star History Chart](https://api.star-history.com/chart?repos=iAmCorey/Wake&type=date&legend=top-left&sealed_token=ZX5h8laOXIE38b__FRNpP7ae52yRupThIRrcgidF7RI0OOzVcsKIo1iJ_iDp6UcMoxzNCL99N3RY__N7TFUszIgxzljBSBRRiAPYPt9QC9lKf7X3ShAQJg)](https://www.star-history.com/?type=date&repos=iAmCorey%2FWake)

## License

[MIT](LICENSE). Brand icons are from [lobe-icons](https://github.com/lobehub/lobe-icons) (MIT); agent names and logos belong to their respective owners.
