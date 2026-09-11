# Wake MCP server (`wake-mcp`)

`wake-mcp` is a small, read-only [Model Context Protocol](https://modelcontextprotocol.io) server that ships with Wake. It exposes Wake's session index to any MCP client — Claude Code, Codex, Cursor, or anything else that speaks MCP over stdio — so an agent can search your history, list what you worked on recently, and read a past transcript page by page.

It covers everything Wake indexes: sessions from every supported agent on this machine, plus the local mirrors of any remote hosts you configured. Nothing here can modify a session. The server opens Wake's index without write access, never scans or rebuilds it, and never touches the agents' own files except to read a transcript.

## Where it lives

| Platform | Path |
|---|---|
| macOS | `/Applications/Wake.app/Contents/MacOS/wake-mcp` |
| Linux | next to `wake`: `~/.local/bin/wake-mcp` (tar.gz install) or `/usr/bin/wake-mcp` (deb) |
| Windows | next to `Wake.exe` in the unpacked zip |

Settings → Connect in Wake shows the exact path for your install with a *Copy path* button, and one *Copy command* / *Copy config* button per client (*Show* reveals the snippet before you copy it). `wake-mcp setup` prints the same snippets from a terminal. Updating Wake keeps the path, so there is nothing to redo after an update.

## Setup

### Claude Code

```bash
claude mcp add --scope user wake -- "/Applications/Wake.app/Contents/MacOS/wake-mcp"
```

### Codex

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.wake]
command = "/Applications/Wake.app/Contents/MacOS/wake-mcp"
```

### Cursor

Merge into `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "wake": { "command": "/Applications/Wake.app/Contents/MacOS/wake-mcp" }
  }
}
```

### Any other client

Run the binary as a stdio MCP server with no arguments. It reads Wake's index from the default location (`~/Library/Application Support/wake/wake.db` on macOS, `~/.local/share/wake/wake.db` on Linux, `%LOCALAPPDATA%\wake\wake.db` on Windows); pass `--db PATH` to point it elsewhere.

If the index does not exist yet, the server exits with status 2 and a message on stderr asking you to launch Wake once — Wake builds the index on first run.

### Removing it

`claude mcp remove wake` for Claude Code; for Codex and Cursor delete the `wake` block from the config file. Nothing else was installed.

## Using it

### Check that it is connected

- **Claude Code**: `claude mcp list` shows `wake` as connected; inside a session, `/mcp` lists the server and its four tools.
- **Codex**: restart Codex after editing `config.toml`; the `wake_*` tools show up in its tool list.
- **Cursor**: Settings → MCP shows `wake` with a green status dot.
- **No client at hand**: `wake-mcp call wake_list_projects` in a terminal prints exactly what an agent would see.

### Just ask

You do not need new commands. Ask in plain language and the agent picks the tool:

| You say | What the agent does |
|---|---|
| "What did I work on in this repo last week?" | `wake_list_sessions` with its working directory and `since: "7d"` |
| "Have I hit this inotify overflow error before?" | `wake_search` across every agent's transcripts |
| "Pick up where yesterday's Cursor session left off on the watcher" | `wake_list_sessions` (`agents: ["cursor"]`), then `wake_get_session` to read it, then it starts working |
| "Why did we choose rsync over a custom sync protocol?" | `wake_search` for the discussion, `wake_get_session` at the cited `seq` to read the reasoning |
| "Which projects have I touched this month?" | `wake_list_projects` with `since: "30d"` |

Answers can cite `wake://session/<key>#<seq>` references; the `seq` numbers are the same ones Wake shows in its transcript view and search results.

### Make the agent reach for it

Agents rarely call a tool nobody told them about. One line in the project's `CLAUDE.md`, `AGENTS.md` or Cursor rules turns it into a habit:

```
Before asking me for context about this repository, check earlier sessions with the wake MCP tools:
wake_list_sessions with the current directory, then wake_get_session for the relevant one.
```

### A worked example

```
You:    What was I doing in this repo yesterday with Codex? I want to continue.

Agent:  [wake_list_sessions {"project": "/Users/me/Github/app", "agents": ["codex"], "since": "2d"}]
        [wake_get_session {"key": "codex:0195c2a1-…", "max_messages": 40}]

        Yesterday's Codex session (14:10–15:32) reworked the retry loop in sync.rs:
        it moved the backoff into `Retry::next_delay`, added the jitter test, and
        stopped at a failing assertion in `tests/sync.rs:88` (seq 61 in that session).
        Want me to start from that failing test?
```

Everything above happens locally; Wake itself does not change and the other agent's files are only read.

### What it does not do

- No write tools: nothing can delete, star, rename or resume a session. Resuming stays in Wake.
- Subagent transcripts that live inside a session (Claude Code sidechains, Cursor subagents) are not included in `wake_get_session`; their count is reported so the agent knows they exist. Subagent sessions that Wake tracks as separate sessions (Grok) are folded under their parent in `wake_list_sessions` but can be read by key.
- Antigravity sessions are metadata only — their transcripts are encrypted on disk, so an agent gets the same preview card Wake shows.
- Archived Codex sessions appear in search results but not in `wake_list_sessions` or `wake_list_projects`.

## Keeping results fresh

Search and lists come from Wake's index. Wake keeps the index current while it is running (file watching for JSONL-based agents; SQLite-based agents such as Copilot and OpenCode refresh on launch and on manual refresh). Every reply ends with a line like

```
Index covers activity up to 2026-09-08 09:41:37 (local time); Wake keeps it fresh while it is running.
```

so an agent can tell how recent the data is. Reading a transcript with `wake_get_session` parses the agent's own files rather than the index, so it does not depend on when Wake last scanned. Sessions mirrored from a remote host are read from their local mirror, so they are current as of the last successful sync.

## Tools

All four tools are read-only and return Markdown text (`content[0].text`). Parameters are JSON; every parameter except the ones marked required is optional.

### Shared parameters

| Parameter | Type | Meaning |
|---|---|---|
| `project` | string | Scope to one project. Pass an absolute path — the agent's working directory is ideal — or a project name. Path matching is three-tier: an exact match of an indexed project path; otherwise the longest indexed project that contains the path (you are in a subdirectory); otherwise every indexed project below the path (a monorepo root or a parent folder). A bare name matches the project name case-insensitively. When nothing matches, the reply lists the known projects instead of erroring. |
| `agents` | string[] | Only these agents. Ids: `claude-code`, `codex`, `grok`, `dsh`, `cursor`, `opencode`, `pi`, `omp`, `kiro`, `kimi`, `gemini`, `copilot`, `antigravity`, `qoder`, `hermes`, `openclaw`. Display names (`"Claude Code"`, `"Gemini CLI"`) and a few aliases (`claude`, `deepseek`, `opencode2`) are accepted too. |
| `since` | string | Only sessions updated at or after this time. Relative: `30m`, `24h`, `7d`, `2w`. Absolute: `2026-09-01`, `2026-09-01 09:30`, `2026-09-01T09:30:00Z`. Naive date-times are read in local time. |
| `limit` | integer | Maximum items to return; values outside the allowed range are clamped. |

### `wake_search`

Full-text search across every indexed transcript: user prompts, assistant replies, tool names and tool inputs (tool outputs are not indexed). Terms are ANDed. CJK text and code substrings such as `useEffect(` both work; terms shorter than three characters fall back to a slower substring scan.

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `query` | string | required | search terms |
| `project`, `agents`, `since` | | | see above |
| `limit` | integer 1–30 | 10 | maximum sessions returned |

Results are grouped by session, best matches first, with up to three snippets per session. Each snippet carries a reference of the form `wake://session/<key>#<seq>` that `wake_get_session` accepts directly. Archived sessions are included.

```
26 sessions match `二维码` (project /Users/me/Github/app) — showing 3, best matches first.

1. `claude-code:28ffac16-…` · Claude Code · "Session manager for coding agents"
  /Users/me/Github/app · updated 2026-08-14 19:45:47 · 524 messages · claude-opus-5
  - seq 209 (assistant, 2026-08-14 14:45:03): …search "**二维码**" 2>&1 |…
    ref: wake://session/claude-code:28ffac16-…#209
```

### `wake_list_sessions`

Most recently updated sessions, newest first. Subagent sessions are folded into their parents and archived sessions are excluded. Pinned sessions are not moved to the top here (they are in Wake's own list), so `limit: 1` really is the newest session.

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `project`, `agents`, `since` | | | see above |
| `starred` | boolean | false | only sessions starred in Wake |
| `limit` | integer 1–100 | 20 | |

Each line shows the session key, agent, title, project, last update, message count, model, and badges such as `@host` (remote host), `via vscode` (source) or `★ starred`.

### `wake_get_session`

One transcript as compact Markdown, parsed live from the agent's files.

| Parameter | Type | Default | Notes |
|---|---|---|---|
| `key` | string | required | a session key such as `claude-code:1b2c…`, or a `wake://session/<key>#<seq>` reference (the seq becomes the starting point) |
| `from_seq` | integer ≥ 0 | 0 | start at this message |
| `max_messages` | integer 1–200 | 60 | messages per page |
| `max_chars` | integer 200–100000 | 20000 | character budget per page |
| `max_message_chars` | integer 100–50000 | 4000 | budget per message, shared by its text, thinking and tool calls; longer content is truncated and marked |
| `include_tools` | boolean | false | include tool inputs and outputs (verbose) |
| `include_thinking` | boolean | false | include the assistant's recorded thinking |

The reply starts with a header (title, key, agent, host, project and branch, model, time range, message count), then one block per message:

```
### [seq 12] User · 2026-09-05 11:02:14
Why does the watcher drop events on Linux?

### [seq 13] Assistant · 2026-09-05 11:02:40
inotify queues overflow when …
- 🔧 Read: crates/wake-core/src/watcher.rs
- 🔧 Grep: need_rescan
```

Tool calls are folded to one line each (name plus input preview) unless `include_tools` is set; at most 40 tool calls are listed per message, the rest are counted. Injected context (system reminders, IDE context) is skipped and counted. Compaction summaries are kept as quotes. Images are noted, not included. Subagent transcripts are not included; their count is reported.

The footer says which seqs were shown and either `End of transcript.` or a `from_seq=<n>` hint for the next page. Seq numbers are the same ones Wake's own search results and transcript view use.

If the key is unknown the tool returns an error result (`isError: true`) explaining what keys look like; if only a bare native id is given, the server looks it up and, when several hosts share that id, lists the candidates.

### `wake_list_projects`

Projects (working directories) that have indexed sessions, most recently active first, with session counts and last-activity time. Archived-only projects are not listed.

| Parameter | Type | Default |
|---|---|---|
| `since` | string | |
| `limit` | integer 1–200 | 50 |

## Session keys and references

- Local sessions: `<agent>:<native id>`, for example `codex:0195c2a1-…`.
- Sessions mirrored from a remote host: `<agent>:<host>:<native id>`.
- References: `wake://session/<key>#<seq>` point at one message; `wake_get_session` accepts them as `key` and starts the page there.

The native id is the one the agent's own `--resume` flag expects.

## Errors

| Situation | Response |
|---|---|
| Unknown tool, wrong parameter type, unparsable `since` | JSON-RPC error `-32602` |
| Unknown method (for example `resources/list`) | JSON-RPC error `-32601` |
| Invalid JSON on the wire | JSON-RPC error `-32700` |
| Session key not found, transcript unreadable | normal result with `isError: true` and an explanation |
| No project matches, no search hits | normal result whose text says so |

## Command line

```bash
wake-mcp                       # serve MCP over stdio (what clients run)
wake-mcp --db PATH             # use another index database
wake-mcp setup                 # print the setup snippets for Claude Code / Codex / Cursor
wake-mcp call wake_search '{"query":"useEffect(","limit":3}'   # run one tool and print its text
wake-mcp --version
```

`call` is handy for checking what an agent would see. It exits non-zero when the tool reports an error.

## Protocol details

- Transport: stdio, one JSON-RPC 2.0 message per line; logs go to stderr only.
- Methods: `initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`. No resources, prompts or sampling.
- Protocol versions: `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05` (the client's requested version is echoed when it is one of these).
- Capabilities: `tools` only; every tool is annotated `readOnlyHint: true`.
- Several clients can each run their own `wake-mcp` at the same time; they are independent read-only processes over the same index.

## Privacy and scope

- Everything runs on this machine. The server makes no network requests.
- Agents see the same session files Wake indexes — local agents' data directories plus the local mirrors of any remote hosts you configured in Wake. Nothing leaves the machine.
- Wake's read-only rules apply: other agents' directories and databases are opened read-only and credential files are never read.
- Because an agent's own session is also indexed by Wake, a `wake_search` call it makes today (the tool name and query) will show up in tomorrow's search results for the same term. Tool outputs are not indexed, so the results themselves do not.

## See also

[docs/cli.md](cli.md) — `wake-cli`, the same index from a shell, for agents that run commands rather than call tools. It prints the identical text.

## Troubleshooting

- **The client reports the server failed to start.** Run the binary in a terminal: `no Wake index at … — launch Wake once to build it` means Wake has never run on this machine (or `--db` points to the wrong place). `… is empty or from an older version` means the index predates this Wake version; launching Wake once upgrades it.
- **Results look stale.** Keep Wake running; the freshness line at the end of every reply tells you what the index covers. Copilot / OpenCode / Antigravity / Hermes / OpenClaw databases refresh when Wake launches or when you click Refresh.
- **A project path is not matched.** Pass the absolute path of the repository, or its name. `wake_list_projects` shows the paths Wake knows.
- **macOS refuses to run it ("cannot be opened because the developer cannot be verified").** Wake is signed but not notarized, and a client launching `wake-mcp` hits the same first-run gate as opening Wake itself. Clear the quarantine flag for the whole bundle once: `xattr -dr com.apple.quarantine /Applications/Wake.app`, then restart the client.
