# Wake

[![Release](https://img.shields.io/github/v/release/iAmCorey/Wake)](https://github.com/iAmCorey/Wake/releases)
[![CI](https://github.com/iAmCorey/Wake/actions/workflows/ci.yml/badge.svg)](https://github.com/iAmCorey/Wake/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-macOS%2014%2B-black?logo=apple)](https://github.com/iAmCorey/Wake/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A native macOS app that gathers every coding-agent session on your machine into one place — browse, full-text search, and resume any conversation in seconds. Built with **Rust + GPUI** (gpui 0.2 + gpui-component 0.5).

Your agent history is scattered across `~/.claude`, `~/.codex`, and six other private directories. Wake reads them all, read-only, and gives you one fast window into it. Everything stays local: no network requests, ever.

![Wake — sessions list and transcript view](imgs/screenshot-1.webp)

## Features

- **Unified browsing** — all sessions grouped by agent / project, live file watching for incremental updates
- **Full-text search** (⌘K) — SQLite FTS5 trigram index; handles CJK text and code substrings (like `useEffect(`) equally well; jumps straight to the matched message in the transcript
- **Transcript view** — per-message rendering with user/assistant bubbles, collapsible tool-call clusters, thinking summaries, tree-sitter code highlighting (30+ languages)
- **One-click resume** — reopens the session in Terminal/iTerm at the original project directory (`claude --resume`, `codex resume`, …)
- **Manage** — star/pin (stored in Wake's own DB, original files untouched), export to Markdown, delete (system Trash + tombstone so deleted sessions stay deleted)

![Full-text search across every agent's sessions](imgs/screenshot-2.webp)

## Supported agents

| Agent | Data source |
|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` |
| Codex CLI | `~/.codex/sessions` + `state_5.sqlite` (read-only) |
| Copilot CLI | `~/.copilot/session-store.db` |
| Cursor (CLI transcripts) | `~/.cursor/projects/**/agent-transcripts` |
| OpenCode | `~/.local/share/opencode/opencode.db` |
| Kiro | `~/.kiro/sessions/cli` |
| Gemini CLI | `~/.gemini/tmp/**/chats` |
| Grok Build | `~/.grok/sessions/**/updates.jsonl` (+ `summary.json`; `GROK_HOME` override) |

Cursor IDE chats, Windsurf, and Trae encrypt their local data; Amp, Factory, and Warp keep sessions in the cloud — none of those are supported.

## Privacy stance

- Agent data directories are opened **read-only**; Wake never writes to another tool's files or databases
- Credential files (`auth.json` and friends) are never read
- Zero network requests — Wake never constructs or calls an HTTP client (GPUI's dependency tree bundles one; Wake doesn't reach for it)
- Wake's own index lives at `~/Library/Application Support/wake/wake.db` and can be rebuilt from scratch at any time (stars/pins live in a separate table and survive rebuilds)

## Performance

On the author's machine (~289 sessions, ~800 MB of JSONL): full index ~4 s, subsequent launches are instant (mtime-based incremental scan), search results in under 1 ms.

## Install

Build from source (requires a Rust toolchain):

```bash
git clone https://github.com/iAmCorey/Wake && cd Wake
scripts/make-app.sh          # builds dist/Wake.app (icon + Info.plist, ad-hoc signed)
open dist/Wake.app
```

The app is ad-hoc signed, so if you download a prebuilt copy instead of building it yourself, macOS Gatekeeper will block the first launch — right-click the app and choose *Open*, or run `xattr -d com.apple.quarantine Wake.app`.

## Development

```bash
cargo run -p wake                      # run in dev mode
scripts/test.sh                        # one-command test entry: data-layer tests + UI compile gate
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
│   ├── adapters/    #   claude / codex / copilot / cursor / opencode / kiro / gemini / grok
│   │                #   (AgentAdapter trait — add an adapter, get the whole UI for free)
│   ├── scanner.rs   #   single-pass scan: meta + FTS in one go, mtime incremental
│   ├── watcher.rs   #   notify-based file watching → per-file incremental updates
│   ├── db.rs        #   rusqlite (WAL): sessions / messages / messages_fts / user_data / tombstones
│   └── services/    #   terminal resume (AppleScript) / export / trash
└── wake             # GPUI app (three-pane workbench + ⌘K palette)
```

Design notes live in [DESIGN.md](DESIGN.md), product decisions in [PRODUCT.md](PRODUCT.md).

## License

[MIT](LICENSE). Brand icons are from [lobe-icons](https://github.com/lobehub/lobe-icons) (MIT); agent names and logos belong to their respective owners.
