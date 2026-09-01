# Changelog

## Unreleased

- Fix: in a Codex branch or subagent thread, the parent conversation's transcript is injected as a `user` message — its assistant replies were being shown as if you had typed them, and were indexed for search. They are now treated as injected context, like the other `<environment_context>`-style blocks

## [0.3.1] — 2026-08-30

- Fixed: agent CLI detection now ignores login-shell rc output, including unterminated ANSI title escapes, by parsing only newline-framed Wake probe records
- Fixed: agent CLIs installed while Wake is running are detected on the next resume attempt; successful lookups remain cached while misses are re-probed
- Fixed: resume errors now identify the missing agent CLI and agent instead of appearing to blame the selected terminal app

## [0.3.0] — 2026-08-27

- New: Insights page — a new sidebar entry showing your coding agent activity at a glance: sessions, tokens, prompts, agents, projects, and active days
- New: a GitHub-style heatmap charts your prompts day by day across the past year, with your current streak, longest streak, and busiest day
- New: activity breakdowns by hour, weekday, or month — flip between views with the arrows
- New: Agents, Projects, and Models leaderboards, each switchable between sessions, tokens, and prompts

## [0.2.11] — 2026-08-26

- Update: Update UI
- New: Add Intel Mac support

## [0.2.10] — 2026-08-26

- New: Qoder CLI support — sessions under `~/.qoder/projects` are searchable, branch-aware, and resumable with `qoder --resume`; `QODER_CONFIG_DIR` and custom locations are supported
- New: Settings has a dedicated Updates page that checks the latest GitHub Release on demand and opens the release page when an update is available; the macOS Wake menu can start the same check
- Update: the View Update action now uses a taller primary button so available releases are easier to spot
- Fix: update checks run outside GPUI's async runtime instead of getting stuck at Checking

## [0.2.9] — 2026-08-25

- Update: Session location management now lives in a dedicated Settings window, available from the sidebar gear, the Wake menu, or `⌘,`
- Update: locations are grouped by agent, undetected agents stay collapsed by default, and row actions move into a compact overflow menu
- New: Settings now includes General, Locations, Data, and About pages, with a persistent System / Light / Dark appearance choice
- New: About Wake mirrors the Kooky/Birth information hierarchy with Wake's icon, version, tagline, GitHub link, license, and author credit; the Wake menu opens the same page
- New: the Data page shows Wake's local storage path and size and opens it in the file manager; session refresh remains in the main sidebar
- Update: Settings buttons and the appearance selector now share Wake's compact sizing, corner radius, and quiet secondary treatment
- Fix: location scans continue to reach their terminal state when the main window closes while Settings remains open

## [0.2.8] — 2026-08-25

- New: every Session location has its own on/off switch — disabled paths stop scanning and disappear from browse/search results without losing their configuration
- Update: Remove is reserved for custom locations; built-in locations can be disabled and re-enabled in place
- Fix: disabled locations remain part of duplicate-path validation, and Restore defaults updates immediately as switches change

## [0.2.7] — 2026-08-25

- New: experimental Windows support — browse, search and resume sessions on Windows desktops; build from source with `scripts/make-windows.ps1`, or grab a prebuilt zip from the manual "Windows artifact" workflow
- New: on Windows, resume opens sessions in Windows Terminal, PowerShell, Windows PowerShell, Command Prompt, Alacritty or WezTerm; deleted sessions go to the Recycle Bin
- New: `WAKE_HOME` environment variable redirects where Wake looks for agent data (portable installs and testing)
- Update: platform-correct wording and paths throughout — File Explorer / Recycle Bin naming, drive-letter paths in Session locations, Wake's index in `%LOCALAPPDATA%\wake` on Windows

## [0.2.6] — 2026-08-25

- Fix: OpenCode 2 next-channel sessions are discovered in `opencode-next.db` and parsed from the real `session` + `session_message` schema, while the original `opencode.db` path remains enabled
- Fix: OpenCode stable and next database paths can be scanned, edited, and removed independently in Session locations

## [0.2.5] — 2026-08-25

- New: experimental Linux support — browse, search and resume sessions on Linux desktops; prebuilt arm64 packages (.deb and tar.gz) attached to the release
- New: on Linux, resume opens sessions in GNOME Terminal, Console, Konsole, Ghostty, kitty, Alacritty, WezTerm, Xfce Terminal or XTerm
- New: keyboard shortcuts follow the platform — ⌘ on macOS, Ctrl on Linux
- Fix: resume failure notices only say "copied to clipboard" when the copy really happened; otherwise the command is shown in the message

## [0.2.4] — 2026-08-24

- New: Session locations is now a full manager — every location, built-in or custom, can be edited, removed, or pointed at a different folder
- New: add custom session folders for any agent, like backups, synced copies, or non-standard installs
- New: Restore defaults brings all locations back to the built-in paths in one click
- Update: location rows open an edit form on click, with an agent picker and a folder browser
- Update: refined spacing, dialog styling and button alignment across the app
- Fix: the delete confirmation now shows real buttons — before, it could only be confirmed with the Enter key
- Fix: a session that exists in two locations no longer flips between copies; the newest copy wins
- Fix: deleted sessions stay deleted even when another location holds a copy of them
- Fix: sessions from a removed location leave the list right away

## [0.2.3] — 2026-08-24

- New: Session locations — a sidebar button listing every folder Wake reads, with per-location session counts; click a row to open it in Finder
- New: custom data locations are respected — `CODEX_HOME` for Codex, `XDG_DATA_HOME` for OpenCode
- Update: the refresh button moved to the sidebar footer
- Update: sidebar counts are now badges
- Fix: an agent installed while Wake is running now appears after a refresh, no relaunch needed

## [0.2.2] — 2026-08-22

- New: DeepSeek Harness (`dsh`) support — 13 agents total, resumable, with its compressed session logs read transparently
- Update: sidebar agent order
- Update: the Open In button now names the app it will open

## [0.2.1] — 2026-08-20

- Fix: resuming OpenCode sessions in your terminal now works (broken in the 0.2.0 build)

## [0.2.0] — 2026-08-20

- New: 5 new supported agents — Pi, Oh My Pi, Grok Build, Kimi Code, Antigravity CLI (12 total), all resumable from the terminal
- New: OpenCode 2 (beta) support, with an `opencode2` badge and correct resume
- New: session detail shows the session file path — click to reveal in Finder
- New: Kiro sessions show the model used
- Fix: sidebar agent list keeps a fixed order, no more reshuffling on refresh
- Fix: "Reveal in Finder" for database-backed agents (Copilot, OpenCode, Antigravity)
- Update: README supported-agents table now lists data source, model and via per agent

## [0.1.0] — 2026-08-18

- Initial release: browse and search local sessions from 7 coding agents (Claude Code, Codex, Copilot CLI, Cursor, OpenCode, Kiro, Gemini CLI)
- Full-text search with jump-to-message
- Session detail with tool calls, thinking and markdown rendering
- Resume sessions in your terminal; star, pin, export, delete to Trash
- Live updates and light & dark themes
