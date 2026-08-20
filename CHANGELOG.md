# Changelog

## [0.3.0] — 2026-08-20

- New: session list prefers each agent's renamed title (Grok `/rename`, Claude `custom-title`, Codex `thread_name`, Cursor IDE title, Pi session title, …) over the first prompt
- New: deleting a parent session also moves its nested sessions to Trash
- Fix: Cursor project paths with underscores (e.g. `app_av4`) no longer decode as extra path segments
- Change: sidebar agent / project / All Sessions counts list top-level rows only; nested children show on the parent row

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
