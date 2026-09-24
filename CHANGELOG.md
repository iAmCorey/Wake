# Changelog

## [Unreleased]

- New: Devin sessions are indexed from its local database (`~/.local/share/devin/cli/sessions.db`, read-only) — titles, models, thinking, tool calls and token counts included; the visible transcript follows each session's main chain, so retries edited out of the conversation and hidden helper sessions stay out. Open In resumes a session with `devin --resume` in its project directory (#46)

## [0.8.1] — 2026-09-23

- New: `wake-cli refresh` updates an existing index from the terminal, so a scheduled task (launchd, systemd timer) keeps search and session lists fresh while Wake is closed; it steps aside while Wake has its window open, and Wake waits for a running refresh before opening the index (#43, thanks @aka-kika)

## [0.8.0] — 2026-09-22

- New: Memory page — the notes coding agents keep for themselves (Claude Code's auto-memory, Codex's memories and per-session summaries, ZCode's project memory) and the instruction files you write for them (CLAUDE.md, AGENTS.md, GEMINI.md, `.cursor/rules`, `.kiro/steering`, `copilot-instructions.md`, global and per project) in one read-only place, searchable, filtered by agent, project or user memory
- New: Settings → Memory locations shows every source the Memory page reads, with a switch per source, your own folders or files, and Restore defaults
- New: `wake_list_memories` and `wake-cli memories` list those files for other agents; `wake_get_session` reads one by its `wake://memory/…` reference and `wake_search` also surfaces memory files that mention the query
- New: Insights ends with an "Agents asking Wake" board — how often each agent used Wake's MCP tools or `wake-cli` in the last 7 days, switchable between All / MCP / CLI
- New: Copy Handoff in the session detail's more menu — a ready-to-paste note with the session id and both ways to read it (MCP and `wake-cli`), for handing a conversation to another agent
- Update: docs/cli.md shows a Claude Code SessionStart hook that puts the project's recent sessions into context at startup
- Update: the sidebar switches between Sessions, Memory, Insights and Clean Up from an icon strip at the bottom; Refresh moved into each page header, and on the Memory page it re-reads only the memory locations
- Update: sidebar polish — a spinner and progress bar while a refresh runs, collapse arrows in front of the Agents and Projects headers, and one consistent gap between icons and labels throughout the app
- Fix: a search term of one or two characters no longer crashes Wake when a matching message contains characters such as Ω, İ or K before the match
- Update: Codex sub-agents started with `spawn_agent` are indexed again, nested under the session that spawned them and titled with their task name; the parent conversation Codex copies into each one is folded away so it is indexed once, and the guardian auto-review, `/review`, compaction and memory-consolidation threads stay excluded. In the MCP tools and `wake-cli`, a session now names its parent and lists its child sessions (#42, thanks @ShadowySpirits). Existing indexes are re-read once after upgrading
- New: ZCode (Z.ai's GLM-5.3 desktop harness) sessions are indexed from its local database — titles, models, tool calls and token counts included; forks and side chats are listed, sub-agent runs and conversations imported from Claude Code are left out. Read-only, and no Open In: the app has no command line to resume from

## [0.7.0] — 2026-09-16

- New: session cleanup from the sidebar — filter by date, agent or project, sort by size, and preview before moving session files to the system trash. Includes cleanup history.
- New: Copy Session Path in the session detail toolbar copies the source file path directly, with inline confirmation. Database-backed sessions copy the shared database path; remote sessions copy the local mirror path, as indicated in the tooltip (#33, thanks @ItsRyanWu)
- Fix: Pi and Oh My Pi token usage now adds up each assistant call instead of keeping only the last one, including calls with no visible reply. The shared OpenClaw reader also adds up the calls in its active transcript. Existing indexes are recalculated once after upgrading (#34, thanks @AwadYoo for the report and proposed fix in #35)
- Fix: Cursor transcript sessions preserve spaces in project paths, using workspace metadata and filesystem matching. Git roots no longer override subdirectory workspaces, and later metadata updates refresh project grouping even when the transcript is unchanged (#38, based on #39 by @YTwsy)

## [0.6.7] — 2026-09-15

- New: Cursor's IDE chats — the Chat / Composer panel — are indexed from Cursor's own database next to the CLI transcripts, so chats Wake never showed before are listed and searchable like any other session; a chat that also has a full transcript keeps being read from the transcript, so it stays under the right project. Older chats that Cursor stored without a workspace show under Unknown project (#32, thanks @junqingyongyuanbusi)

## [0.6.6] — 2026-09-15

- Fix: Codex's background threads — the guardian auto-review, `/review`, compaction, memory consolidation and spawned sub-agents — no longer show up as sessions; they were listed as `Untitled` or as a copy of their parent, and reached search, project counts and the MCP tools too. Existing indexes drop them on the next scan

## [0.6.5] — 2026-09-14

- New: `wake_get_session` and `wake-cli show` can read a subagent transcript — the main transcript now lists their ids; pass one as `subagent` / `--subagent`, or `*` to list them all
- Update: search favours recently active sessions a little — an equally good match from last week now outranks one from last year, in ⌘K / Ctrl+K, `wake_search` and `wake-cli search`
- Update: search now matches session titles too, in ⌘K / Ctrl+K, `wake_search` and `wake-cli search` — title hits come first and open the session at its start
- Fix: "Copy SSH command" for a remote session now starts a login shell on the remote, so agents installed with nvm or another version manager are found
- Fix: ⌘K / Ctrl+K results show the `@host` badge for sessions mirrored from a remote host
- Fix: Insights no longer counts a session twice when it is mirrored from more than one host
- Fix: Settings → Data includes the remote mirrors in the storage size
- Fix: an agent's own Wake lookups (the `wake_*` tools, `wake-cli` commands) no longer show up as search hits in its session; existing indexes are re-derived once on the first launch after upgrading

## [0.6.4] — 2026-09-14

- New: CodeBuddy and WorkBuddy support — sessions from Tencent's CodeBuddy Code CLI and the WorkBuddy desktop agent now show up alongside every other agent, with search and the transcript view, plus one-click resume for CodeBuddy
- Update: `wake-cli index` shows its progress in the terminal while it scans; when piped or run by an agent it stays quiet until the summary

## [0.6.3] — 2026-09-13

- New: `wake-cli index` builds Wake's index from the terminal, so an agent that finds Wake installed but never launched can get itself working instead of stopping to ask you — it only ever builds a missing index, never touches one the app is already keeping current

## [0.6.2] — 2026-09-12

- Fix: On macOS, conversation file links open in Finder instead of showing an application error. Directories open directly, files are selected, and unavailable or remote paths show an in-app notice (#28, #29).
- Fix: Bare relative file links with line numbers, such as `main.rs:11`, resolve against the session project and respect remote-session restrictions.

## [0.6.1] — 2026-09-11

- New: Settings → Connect now covers all three ways in — the MCP server, the `wake-cli` command line and the Wake skill each get their own block, with the path to copy, the command that puts `wake-cli` on your PATH (where one applies — not on Windows, and not when an installer already put it there), and the one-liner that installs the skill; the MCP and command-line blocks link straight to their reference docs
- Fix: Settings → Connect is fully translated — the per-client hints and the Copy buttons were showing English next to Chinese headings, and a Copy button flipped back to English after confirming

## [0.6.0] — 2026-09-11

- New: `wake-cli` — search your session history, list what you worked on in a project and read a transcript straight from the terminal, so any agent that can run a shell command gets the same answers a connected MCP client does; run `wake-cli setup` for the path and how to point an agent at it (see docs/cli.md)
- New: a Wake skill, so agents reach for your session history on their own — `npx skills add iAmCorey/Wake` teaches Claude Code, Codex and anything else that reads skills when to look back at earlier conversations, decisions and errors instead of guessing from git

## [0.5.2] — 2026-09-10

- New: Wake speaks your language — Simplified Chinese included, and the UI follows your system language on first launch; pick a fixed language under Settings → General
- New: adding a language takes one JSON file — drop it in Wake's config folder to use it right away, or send it as a pull request; anything left untranslated stays English (see crates/wake/locales/README.md)

## [0.5.1] — 2026-09-08

- Update: the MCP tools now tell agents when to use Wake — earlier conversations, decisions, where work stopped, "have we seen this error" — so a fresh session reaches for session history instead of answering from git log
- Fix: Copy buttons on Settings → Connect confirm inline with a brief "Copied" instead of a notification that could stay on screen

## [0.5.0] — 2026-09-08

- New: Connect your coding agents to Wake over MCP — the bundled read-only `wake-mcp` server lets Claude Code, Codex, Cursor and any other MCP client search your session history, list recent sessions per project and read transcripts page by page
- New: Settings → Connect shows the server path and one-click copy of the Claude Code / Codex / Cursor setup, with Show to peek at each snippet; `wake-mcp setup` prints the same from a terminal
- New: a guide and full reference for the MCP server at docs/mcp.md — how to connect, what to ask, every tool and parameter, troubleshooting

## [0.4.3] — 2026-09-03

- New: Insights shows a "Last 7 days" row — sessions, prompts and active days with the change against the previous 7 days
- New: Insights "Over time" chart — prompts per week for the past year stacked by agent in brand colors, aligned with the activity heatmap

## [0.4.2] — 2026-09-03

- Fix: the Hermes Agent icon is readable in dark mode instead of showing as a white blob

## [0.4.1] — 2026-09-03

- New: Hermes Agent sessions — Wake reads `~/.hermes/state.db` (and every `profiles/*/state.db`), including tool calls, reasoning and the channel a session came from; `HERMES_HOME` is respected
- New: OpenClaw sessions — both the current per-agent SQLite store and legacy `sessions/*.jsonl` transcripts are scanned, showing only the active branch of each conversation; `OPENCLAW_STATE_DIR` is respected
- Update: remote hosts mirror Hermes session databases and OpenClaw legacy transcripts too — OpenClaw's SQLite store stays on the remote machine because it also holds credentials

## [0.4.0] — 2026-09-03

- New: remote hosts — mirror agent sessions from other machines over SSH into the index. Add a host in Settings → Remote hosts (an alias from `~/.ssh/config` or `user@host`); sessions sync on launch, refresh, and Sync now, then browse and search them alongside local ones with an `@host` badge
- New: remote sessions offer "Copy SSH command" — a ready-to-paste `ssh -t` command that resumes the session in its original project directory on the remote machine
- Update: remote mirroring is strictly read-only — only session data is synced (never credentials), nothing on the remote host is ever written, and remote sessions can't be deleted from Wake
- Fix: the blue focus outline on text fields is no longer clipped inside dialogs and settings pages
- Credits: remote sessions over SSH were requested in [issue #21](https://github.com/iAmCorey/Wake/issues/21). Thanks to [@megrxu](https://github.com/megrxu) for the proposal and workflow sketch

## [0.3.6] — 2026-09-02

- New: standard macOS Edit and Window menus, so Cut, Copy and Paste work inside system dialogs and Hide, Minimize, Zoom and Full Screen shortcuts do what they should
- New: Window → Main Window brings the main window back when only Settings is left open
- Fix: Close Window in the File menu and ⌘W reliably close the window
- Fix: About, Settings and Check for Updates stay usable in the menu bar after the main window is closed
- Fix: new sessions no longer go missing until the next refresh when the system drops file change notifications
- Fix: the Settings window now opens on the same screen as the Wake window instead of always on the primary display
- New: Wake remembers which screen its window was on, its position and size, and whether it was maximized or full screen, and opens it there next time, including after a restart or a Dock reopen
- Fix: your own messages in the conversation view no longer collapse into a one-character-wide column
- Fix: the close button in the Add location and Edit location dialogs is visible again
- Fix: clicking outside the Add / Edit location dialog or the search palette closes it again
- New: Export as Markdown and Save image ask where to save the file and start from the folder you used last time

## [0.3.5] — 2026-09-02

- New: Open in now offers Claude Desktop for Claude Code sessions and Codex Desktop for Codex sessions, jumping straight to that session in the desktop app
- New: Open in remembers your last choice per agent and keeps it across restarts
- Fix: opening a session in Claude Desktop no longer creates a duplicate copy when the conversation already lives there
- Update: Codex Desktop now shows the Codex brand icon in Open in instead of the ChatGPT app icon

## [0.3.4] — 2026-09-01

- New: inline images from every readable agent transcript format now appear with compact thumbnails, full-window zoom, copy, and collision-safe save-to-Downloads actions; unsupported image formats can still be saved in their original bytes
- Performance and privacy: image bytes are decoded only when a transcript is opened, each image is capped at 12 MiB with a 64 MiB transcript budget, remote URLs and transcript-supplied local paths are never dereferenced, and background indexing keeps lightweight `[image]` placeholders instead of base64 payloads
- Fix: global search now closes reliably when clicking outside its panel while remaining open when switching to another application
- Credits: inline-image support was inspired by [PR #14](https://github.com/iAmCorey/Wake/pull/14). Thanks to [@Tespera](https://github.com/Tespera) for investigating the Claude Code and Codex formats; the final implementation generalized that work across all agent sources that expose readable image blocks

## [0.3.3] — 2026-09-01

- New: Grok Build `execute-plan` and subagent sessions now nest under their root parent, with expandable child counts, root-level sidebar totals, source-project recovery for worktrees, flat Starred/search results, and cascading Trash deletion
- Credits: Grok subagent nesting was inspired by [PR #3](https://github.com/iAmCorey/Wake/pull/3). Thanks to [@syanbo](https://github.com/syanbo) for mapping Grok's parent metadata and worktree behavior; the final implementation was independently rebuilt on the current codebase

## [0.3.2] — 2026-09-01

- Fix: in a Codex branch or subagent thread, the parent conversation's transcript is injected as a `user` message — its assistant replies were being shown as if you had typed them, and were indexed for search. They are now treated as injected context, like the other `<environment_context>`-style blocks
- Update: sessions sorted by newest creation or update time are grouped into Pinned, Today, Yesterday, Earlier this week, and month sections
- Fix: the session list now loads additional pages near the bottom instead of stopping after the first batch, and search hits beyond that batch are loaded and selected correctly
- Fix: long mixed-language titles now end with a Unicode-width-aware ellipsis in the session list while wrapping in full in the detail header
- Update: the project badge in session detail opens its folder, while empty, `HEAD`, and detached branch labels stay hidden
- Fix: session detail now shows the concrete missing-adapter or transcript parsing error instead of a blank reader, with an action to reveal the source file
- Update: conversation Markdown now has clearer h1–h4 hierarchy and framed code blocks with language labels, copy actions, and immediate light/dark syntax colors
- Update: conversation text has roomier line spacing and window-level selection, including Shift extension across messages and edge auto-scroll while dragging a selection
- Update: the destructive session action in the detail overflow menu now uses the danger color for both its trash icon and label
- Fix: Codex code-review results now render as a readable review with findings, locations, and confidence instead of exposing the structured JSON payload
- Update: Thinking attached to assistant replies is now a collapsible panel with a one-line summary and full expanded content, independently of tool-call expansion
- Update: tool calls now use expandable cards with Unicode-width-aware summaries, complete available inputs, successful and failed outputs, and full-content copy actions beyond the 600-character preview
- Update: Insights uses 28px overview values, 24px leaderboard rows, 32px section spacing, and one shared 2px radius for chart and heatmap cells
- Credits: Several UI and interaction improvements were inspired by [PR #12](https://github.com/iAmCorey/Wake/pull/12). Thanks to [@Tespera](https://github.com/Tespera) for the thoughtful exploration and contribution; the final implementations were independently rebuilt on the current codebase

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
