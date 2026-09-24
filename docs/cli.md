# Wake CLI (`wake-cli`)

`wake-cli` is a small, read-only command-line tool that ships with Wake. It answers the same three questions the [MCP server](mcp.md) answers — what did I discuss, what did I work on here, what happened in that session — from a terminal, a script, or any agent that can run a shell command.

It prints what an MCP client is given: the two surfaces call the same code, and the test suite asserts their output byte for byte apart from a trailing newline the CLI adds (see [Output](#output)). So a `wake-cli show …` transcript and a `wake_get_session` reply are the same text, and this page and [docs/mcp.md](mcp.md) describe one format, not two.

Nothing here can modify a session. The CLI opens Wake's index without write access and never touches the agents' own files except to read a transcript. Two commands write to Wake's own index, and only while the app is closed: [`wake-cli index`](#index) builds one when there is none yet, and [`wake-cli refresh`](#refresh) brings an existing one up to date. Neither ever rebuilds an index — that stays the app's job.

## Where it lives

| Platform | Path |
|---|---|
| macOS | `/Applications/Wake.app/Contents/MacOS/wake-cli` |
| Linux | next to `wake`: `~/.local/bin/wake-cli` (tar.gz install) or `/usr/bin/wake-cli` (deb) |
| Windows | next to `Wake.exe` in the unpacked zip |

On Linux the deb puts it in `/usr/bin`; the tar.gz installer puts it in `~/.local/bin`, which you may need to add to your `PATH`. On macOS it lives inside the app bundle, so link it once:

```bash
sudo mkdir -p /usr/local/bin && sudo ln -sf '/Applications/Wake.app/Contents/MacOS/wake-cli' /usr/local/bin/wake-cli
```

`/usr/local/bin` is root-owned on a stock macOS, hence the `sudo`. If you would rather not, skip the link entirely — the full path works anywhere, and `wake-cli setup` prints it.

`wake-cli setup` prints the path for your install, the right link command, and a block you can paste into a project's `CLAUDE.md` / `AGENTS.md` to teach an agent to use it. Updating Wake keeps the path, so there is nothing to redo after an update.

The binary is not named `wake`: that name already belongs to the GUI. Symlink it to whatever short name you like.

## Commands

```bash
wake-cli search QUERY [OPTIONS]
wake-cli sessions [OPTIONS]
wake-cli show KEY [OPTIONS]
wake-cli projects [OPTIONS]
wake-cli memories [OPTIONS]
wake-cli setup
wake-cli index
wake-cli refresh
wake-cli --help | --version
```

Global options, valid anywhere on the line: `--db PATH` (another index database), `--help` / `-h`, `--version` / `-V`, and `--` to end option parsing. Command options must come after their command. Every option that takes a value also accepts the `--flag=value` form, which is how you pass a value that starts with a dash. The switches (`--starred`, `--tools`, `--thinking`) take no value at all.

There is no default project scope. Pass `--project "$PWD"` when you mean "here" — otherwise every project on the machine is in scope. A relative path (`.`, `../x`) is refused rather than guessed at, because inside a symlinked checkout the shell's `$PWD` and the process's working directory disagree.

### `search`

Full-text search across every indexed session — titles, user prompts, assistant replies, tool names and inputs. A title match is listed as `title` with a reference to the start of the session. Terms are ANDed; CJK text and code substrings like `useEffect(` work. Returns matching sessions with up to three snippets each, and a `wake://session/<key>#<seq>` reference per snippet.

| Option | Value | Notes |
|---|---|---|
| `--project` | path or name | absolute path, or a project name |
| `--agent` | agent id | repeatable, or comma-separated |
| `--since` | when | see [When](#when) |
| `--limit` | N | default 10, max 30 |

```bash
wake-cli search "rate limiter" --project "$PWD" --since 7d
wake-cli search "useEffect(" --agent codex --agent claude-code
wake-cli search -- --dry-run          # a query that starts with a dash
```

### `sessions`

The most recently updated sessions. Sub-agents that their agent records as sessions of their own (Codex `spawn_agent`, Grok) are folded into their parents — `show` on the parent lists them by key; Claude Code and Cursor subagent transcripts live inside their session and are read with `show --subagent`; Codex's background threads (auto-review, `/review`, compaction, memory consolidation) are not indexed at all. Archived sessions are excluded. This is what to run when you want the key of "the session from yesterday".

Same options as `search`, plus `--starred` (only sessions you starred in Wake). `--limit` defaults to 20, max 100.

```bash
wake-cli sessions --project "$PWD" --limit 5
wake-cli sessions --agent codex --since 24h
```

### `show`

One session's transcript, parsed live from the agent's own files, as compact Markdown: user and assistant messages with `[seq N]` markers, tool calls folded to one line each, injected context omitted. Because it reads the file rather than the index, it does not depend on when Wake last scanned. One exception: a session mirrored from a remote host is read from the local mirror, so it reflects Wake's last successful sync rather than the remote machine right now.

| Option | Value | Notes |
|---|---|---|
| `--from` | SEQ | start at this message (min 0); a `wake://…#N` reference sets it too, and an explicit `--from` wins over the reference's seq |
| `--messages` | N | messages per page, default 60, range 1–200 |
| `--chars` | N | character budget per page, default 20000, range 200–100000 |
| `--message-chars` | N | truncate each message, default 4000, range 100–50000 |
| `--tools` | — | include tool inputs and outputs (verbose) |
| `--thinking` | — | include the assistant's thinking where the agent recorded it |
| `--subagent` | ID | read this subagent transcript instead of the main one; the main transcript lists the ids at its end (at most 30), and `'*'` lists them all |

`show` also accepts a bare native id (the one the agent's own `--resume` wants) and falls back to looking it up. When the same id exists on more than one host it lists the candidates instead of guessing.

Long transcripts are paginated: when the reply ends with a `from_seq` hint, run it again with `--from <that number>`.

```bash
wake-cli show 'claude-code:1b2c3d4e-…'
wake-cli show 'wake://session/claude-code:1b2c3d4e-…#42' --tools
wake-cli show 'codex:0195c2a1-…' --from 120 --messages 40
wake-cli show 'claude-code:1b2c3d4e-…' --subagent agent-a1b2c3d4
wake-cli show 'claude-code:1b2c3d4e-…' --subagent '*'
```

### `projects`

Working directories that have session history, most recently active first, with session counts. Use it to find the right `--project` value. Options: `--since`, `--limit` (default 50, max 200).

### `memories`

The memory files agents keep for themselves — Claude Code's per-project auto-memory (`~/.claude/projects/<project>/memory/`), Codex's memories (`~/.codex/memories/`, plus its per-session summaries), ZCode's per-project memory (`~/.zcode/cli/memories/projects/<project>/memory/`) — read-only, grouped by project with *user memory* (the notes that apply to every project) last. User memory is listed under any `--project` because it applies everywhere, even one that matches no indexed project. Each entry ends with a `wake://memory/…` reference; pass it to `show` to read the file, which is read live from disk. Instruction files (CLAUDE.md, AGENTS.md, GEMINI.md, `.cursor/rules`, `.kiro/steering`, `copilot-instructions.md`, from the agents' homes and from every indexed project root) are listed alongside; Settings → Memory locations decides which sources are read. Options: `--project`, `--agent`, `--limit` (default 50, max 100; it caps the project memory — user memory is always included).

```bash
wake-cli memories --project "$PWD"
wake-cli show 'wake://memory/claude-code:/Users/me/.claude/projects/-Users-me-app/memory/MEMORY.md'
```

`search` appends up to five memory files that mention the query, with the same references; `--project`, `--agent` and `--since` apply to them too.

### `setup`

Prints this binary's path, the index path, how to put it on your `PATH`, a paste-able block that tells an agent when and how to use it, and — when `wake-mcp` sits next to it — the one-line MCP setup for Claude Code. It installs nothing and never edits another tool's config files. The one thing it can write is Wake's own: resolving the default index path creates Wake's data directory and migrates an index left by the old `vibex` builds (`--db` skips that).

To make this automatic instead of per-project, install the skill (next section).

## Teaching an agent to use it

Two ways to teach it, same content — and a third that skips the teaching by handing the
agent this project's recent sessions before it asks.

**A skill, once, for every project** — the repository ships one at `skills/wake/`:

```bash
npx skills add iAmCorey/Wake
```

That works for Claude Code, Codex and anything else that reads the skills format. To do
it by hand, copy `skills/wake/` into `~/.claude/skills/wake/`. The skill tells the agent
*when* to look back — earlier conversations, past decisions, where work stopped, whether
an error has been seen before — and how to read keys, references and pages.

**A paste block, for one project** — `wake-cli setup` prints a short version you can drop
into that project's `CLAUDE.md` or `AGENTS.md`. Use this when you would rather not
install anything, or want the guidance to live in the repository.

**Automatically, at session start (Claude Code)** — a `SessionStart` hook runs a command
when a session begins and hands its output to the agent as context. This one lists the
project's recent sessions, so the agent already knows what happened here before you say
"continue from yesterday":

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "wake-cli sessions --project \"$CLAUDE_PROJECT_DIR\" --limit 5 --since 14d 2>/dev/null || true",
            "timeout": 30
          }
        ]
      }
    ]
  }
}
```

Put it in `~/.claude/settings.json` to get it in every project, or in a project's
`.claude/settings.json`. A few things to know:

- If `wake-cli` is not on your `PATH`, use its full path (see *Where it lives*).
- Keep the trailing `|| true`. A hook that exits `2` stops the session from starting, and
  `wake-cli` exits `2` when there is no index yet.
- What gets injected is titles, keys and dates — a handful of lines, not transcripts. The
  agent still reads a session with `show` when it needs the details.
- `startup|resume` skips `/clear` and compaction; drop the matcher to run on those too.
- A lookup made by the hook is not the agent asking Wake, so it does not show up in
  Insights under *Agents asking Wake*; the lookups the agent then makes on its own do.
- Only Claude Code has this hook. Codex and Gemini CLI have no equivalent, so there the
  skill is the way in.

None of the three writes to another tool's configuration; Wake only ever prints, and the
hook is yours to add.

### `index`

Builds the index once, for the case where Wake is installed but has never been launched:

```bash
wake-cli index
```

It scans the agents' files and writes Wake's index, then tells you what it found. It only
does this when there is **no index yet** — if one already exists it says so and changes
nothing; to bring an existing index up to date use [`refresh`](#refresh). There is
deliberately no `--force`: rebuilding an index that exists is the app's job. If Wake is
running (and about to build the index itself), `index` steps aside the same way. In a
terminal it shows progress while it scans; when piped or run by an agent, stderr stays
quiet until the summary.

### `refresh`

Brings an existing index up to date, for the case where Wake is closed most of the time —
you use it through the MCP server or this CLI and only open the app to browse:

```bash
wake-cli refresh
```

It does the incremental pass the app does on launch — picks up new and changed sessions,
drops the deleted ones and re-reads the memory files — and then says what the index holds.
(The app's Refresh button re-reads every session; `refresh` only reads what changed.) It does not sync remote hosts; those mirrors update when the app
runs. Like `index`, it shows progress in a terminal and stays quiet when piped. Point it at
Wake's own database — the path `wake-cli setup` prints — rather than a copy or an alias: it
refuses a file that is not a Wake index, and a path whose remote-host mirrors are not next to it.

It steps aside while Wake has its window open or is still scanning: closing the window
releases the index once any scan in flight finishes, so Wake left in the Dock without a window
does not block it. If Wake or another writer holds the index it says so and exits `0` without
touching anything: the app is already keeping the index current, and two writers with
possibly different environments would undo each other's work. The app and the CLI share one
lock on the index for this — Wake holds it while its window is open, `refresh` and `index`
hold it while they work, and Wake waits up to a minute for a running `refresh` to finish
before it opens the index (if the index is still busy after that, or another Wake holds it,
Wake explains and exits).
The lock is three small files next to the index (`wake.db.lock`, `wake.db.lock.app`,
`wake.db.lock.holder`); they are safe to leave alone.

#### Keeping the index fresh without the app

Point a scheduler at the copy inside the app bundle, so it updates together with Wake.

**macOS** — save this as `~/Library/LaunchAgents/dev.corey.wake.refresh.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.corey.wake.refresh</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/Wake.app/Contents/MacOS/wake-cli</string>
    <string>refresh</string>
  </array>
  <key>StartInterval</key>
  <integer>600</integer>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
```

then load it once:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.corey.wake.refresh.plist
```

**Linux** — a systemd user timer. `~/.config/systemd/user/wake-refresh.service`:

```ini
[Unit]
Description=Refresh Wake's session index

[Service]
Type=oneshot
ExecStart=/usr/bin/wake-cli refresh
```

and `~/.config/systemd/user/wake-refresh.timer`:

```ini
[Unit]
Description=Refresh Wake's session index every 10 minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=10min

[Install]
WantedBy=timers.target
```

then `systemctl --user enable --now wake-refresh.timer`. Use `%h/.local/bin/wake-cli` in
`ExecStart` for a tar.gz install.

**Windows** — Task Scheduler: `schtasks /Create /SC MINUTE /MO 10 /TN "Wake refresh" /TR "\"C:\path\to\Wake\wake-cli.exe\" refresh"`.

Two things to know:

- A scheduler does not see your shell's exports. That is also the environment the app sees
  when it starts from the Dock or a launcher, so both resolve the same data directories. If
  you set `CODEX_HOME`, `XDG_DATA_HOME` or `WAKE_HOME` for the app, set them for the
  scheduled job too — an index kept by two processes with different roots would flip-flop.
- Ten minutes is a sensible interval: a refresh with nothing new takes well under a second,
  and every reply already says how fresh the index is.

## When

`--since` accepts a relative window — `30m`, `24h`, `7d`, `2w` — or an absolute date/time: `2026-09-01`, `"2026-09-01 09:30"`, `2026-09-01T09:30:00Z`. Date-times without a zone are read in local time.

Numeric options outside their range are clamped, not rejected: `--limit 999` on `sessions` gives you 100.

## Agent ids

`claude-code`, `codex`, `grok`, `dsh`, `cursor`, `opencode`, `pi`, `omp`, `kiro`, `kimi`, `gemini`, `copilot`, `antigravity`, `qoder`, `hermes`, `openclaw`, `codebuddy`, `workbuddy`, `zcode`, `craft-agents`. Display names (`"Claude Code"`) and a few aliases (`claude`, `deepseek`, `craft`) work too.

## Session keys and references

- Local sessions: `<agent>:<native id>`, for example `codex:0195c2a1-…`.
- Sessions mirrored from a remote host: `<agent>:<host>:<native id>`.
- References: `wake://session/<key>#<seq>` point at one message; `show` accepts them and starts the page there.

The native id is the one the agent's own `--resume` flag expects.

## Output

The text is Markdown, written for an agent to read, and identical to what the MCP tools return. One consequence is visible: the closing hint in a listing names the MCP tool (`wake_get_session`) rather than `wake-cli show`, because the same string serves both surfaces. Read it as "the command that reads a session".

`search`, `sessions` and `projects` normally end with a line saying how fresh the index is (`Index covers activity up to …`) — that is the newest activity Wake has indexed, so a stale-looking time usually means Wake has not been running, though agents stored in SQLite (Copilot, OpenCode, Antigravity, Hermes, OpenClaw) only refresh on launch or Refresh either way. Two replies skip the line: an unrecognised `--project`, which returns early with the list of known projects, and an empty index, which says so instead. `show` never has one because it reads the agent's file rather than the index. For a session mirrored from a remote host that file is the local mirror, current as of the last successful sync — Settings → Remote hosts shows whether the last one succeeded.

Exactly one trailing newline is added when the text does not already end with one — nothing else is added or removed. Piping is safe: `wake-cli show <key> --tools | head -3` prints three lines and exits 0.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | it ran — **including** "no matches", "no such project" and "no sessions" |
| `1` | it ran and failed — unknown or ambiguous key, a transcript that would not parse, a query that errored, or a failed write to stdout |
| `2` | the command line was wrong, or the index is missing / unreadable / too old |

Empty results are never an error, so `wake-cli search x || fallback` does not fire just because nothing was ever discussed about `x`. Diagnostics go to stderr, prefixed `wake-cli: `, so `wake-cli show <key> > out.md` cannot capture one. Two commands treat a missing index as normal rather than as the `2` above. `setup` always exits `0`, even with no index, and reports a missing one as a `Note:` line on stdout — "not set up yet" is the normal state for someone running it. `index` exits `0` both when it builds one and when it declines because one already exists or because Wake or another writer holds the index; a build that starts and then fails exits `1`. `refresh` exits `0` when it updates the index and when it declines because Wake or another writer holds the index; it exits `2` when there is no index, when `--db` is not a Wake index, or when the index holds remote-host sessions or memories whose mirrors are not next to that path (a copy or an alias in another directory); a scan that starts and then fails exits `1`.

Both a mistyped option (`--sinse 7d`) and a value the tools reject (`--since 7dd`) exit `2` — from a user's seat they are the same mistake, and the layer that caught it is not visible. This is a deliberate difference from `wake-mcp call`, which exits `1` for anything the tool layer rejects — its JSON-RPC envelope already carries the classification, so the exit code never had to. (`wake-mcp` still uses `2` for its own argv problems: a missing tool name, unparsable JSON, or an index it cannot open.)

## Privacy and scope

- Everything runs on this machine. The CLI makes no network requests.
- It reads the same session files Wake indexes — local agents' data directories plus the local mirrors of any remote hosts you configured in Wake. Nothing leaves the machine.
- Wake's read-only rules apply: other agents' directories and databases are opened read-only and credential files are never read. Every command except `index` and `refresh` opens Wake's own index without write access too, and a test asserts that the query commands never change an index by a byte — nor does `index` when one already exists, nor `refresh` while Wake is running.
- On its first run against the default database path, Wake's shared path helper migrates an index left by the old `vibex` builds. `wake-mcp` does the same; `--db` skips it entirely.

## Troubleshooting

- **`no Wake index at … — launch Wake once to build it`.** Wake has never run on this machine, or `--db` points at the wrong file. If Wake is installed but has never been launched, `wake-cli index` builds the index from a terminal instead. `… is empty or from an older version` means the index predates this Wake version; launching Wake once upgrades it, and so does `wake-cli refresh`.
- **`No indexed project matches …`.** Pass the absolute path of the repository, or its name. `wake-cli projects` shows the paths Wake knows.
- **Results look stale.** Keep Wake running, or schedule [`wake-cli refresh`](#refresh) for the times it is closed; the freshness line at the end of every reply says what the index covers. Copilot / OpenCode / Antigravity / Hermes / OpenClaw databases refresh when Wake launches, when you click Refresh, or on `wake-cli refresh`.
- **macOS refuses to run it ("cannot be opened because the developer cannot be verified").** Wake is signed but not notarized. Clear the quarantine flag for the whole bundle once: `xattr -dr com.apple.quarantine /Applications/Wake.app`.

## See also

[docs/mcp.md](mcp.md) — the same index over MCP, for clients that call tools rather than run commands.
