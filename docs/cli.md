# Wake CLI (`wake-cli`)

`wake-cli` is a small, read-only command-line tool that ships with Wake. It answers the same three questions the [MCP server](mcp.md) answers — what did I discuss, what did I work on here, what happened in that session — from a terminal, a script, or any agent that can run a shell command.

It prints what an MCP client is given: the two surfaces call the same code, and the test suite asserts their output byte for byte apart from a trailing newline the CLI adds (see [Output](#output)). So a `wake-cli show …` transcript and a `wake_get_session` reply are the same text, and this page and [docs/mcp.md](mcp.md) describe one format, not two.

Nothing here can modify a session. The CLI opens Wake's index without write access, never scans or rebuilds it, and never touches the agents' own files except to read a transcript.

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
wake-cli setup
wake-cli --help | --version
```

Global options, valid anywhere on the line: `--db PATH` (another index database), `--help` / `-h`, `--version` / `-V`, and `--` to end option parsing. Command options must come after their command. Every option that takes a value also accepts the `--flag=value` form, which is how you pass a value that starts with a dash. The switches (`--starred`, `--tools`, `--thinking`) take no value at all.

There is no default project scope. Pass `--project "$PWD"` when you mean "here" — otherwise every project on the machine is in scope. A relative path (`.`, `../x`) is refused rather than guessed at, because inside a symlinked checkout the shell's `$PWD` and the process's working directory disagree.

### `search`

Full-text search across every indexed session — user prompts, assistant replies, tool names and inputs. Terms are ANDed; CJK text and code substrings like `useEffect(` work. Returns matching sessions with up to three snippets each, and a `wake://session/<key>#<seq>` reference per snippet.

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

The most recently updated sessions. Subagent sessions are folded into their parents; archived sessions are excluded. This is what to run when you want the key of "the session from yesterday".

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

`show` also accepts a bare native id (the one the agent's own `--resume` wants) and falls back to looking it up. When the same id exists on more than one host it lists the candidates instead of guessing.

Long transcripts are paginated: when the reply ends with a `from_seq` hint, run it again with `--from <that number>`.

```bash
wake-cli show 'claude-code:1b2c3d4e-…'
wake-cli show 'wake://session/claude-code:1b2c3d4e-…#42' --tools
wake-cli show 'codex:0195c2a1-…' --from 120 --messages 40
```

### `projects`

Working directories that have session history, most recently active first, with session counts. Use it to find the right `--project` value. Options: `--since`, `--limit` (default 50, max 200).

### `setup`

Prints this binary's path, the index path, how to put it on your `PATH`, a paste-able block that tells an agent when and how to use it, and — when `wake-mcp` sits next to it — the one-line MCP setup for Claude Code. It installs nothing and never edits another tool's config files. The one thing it can write is Wake's own: resolving the default index path creates Wake's data directory and migrates an index left by the old `vibex` builds (`--db` skips that).

To make this automatic instead of per-project, install the skill (next section).

## Teaching an agent to use it

Two ways, same content.

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

Neither writes to another tool's configuration; Wake only ever prints.

## When

`--since` accepts a relative window — `30m`, `24h`, `7d`, `2w` — or an absolute date/time: `2026-09-01`, `"2026-09-01 09:30"`, `2026-09-01T09:30:00Z`. Date-times without a zone are read in local time.

Numeric options outside their range are clamped, not rejected: `--limit 999` on `sessions` gives you 100.

## Agent ids

`claude-code`, `codex`, `grok`, `dsh`, `cursor`, `opencode`, `pi`, `omp`, `kiro`, `kimi`, `gemini`, `copilot`, `antigravity`, `qoder`, `hermes`, `openclaw`. Display names (`"Claude Code"`) and a few aliases (`claude`, `deepseek`) work too.

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

Empty results are never an error, so `wake-cli search x || fallback` does not fire just because nothing was ever discussed about `x`. Diagnostics go to stderr, prefixed `wake-cli: `, so `wake-cli show <key> > out.md` cannot capture one. `setup` is the exception in both directions: it always exits `0`, even with no index, and reports a missing one as a `Note:` line on stdout — "not set up yet" is the normal state for someone running it.

Both a mistyped option (`--sinse 7d`) and a value the tools reject (`--since 7dd`) exit `2` — from a user's seat they are the same mistake, and the layer that caught it is not visible. This is a deliberate difference from `wake-mcp call`, which exits `1` for anything the tool layer rejects — its JSON-RPC envelope already carries the classification, so the exit code never had to. (`wake-mcp` still uses `2` for its own argv problems: a missing tool name, unparsable JSON, or an index it cannot open.)

## Privacy and scope

- Everything runs on this machine. The CLI makes no network requests.
- It reads the same session files Wake indexes — local agents' data directories plus the local mirrors of any remote hosts you configured in Wake. Nothing leaves the machine.
- Wake's read-only rules apply: other agents' directories and databases are opened read-only and credential files are never read. The index itself is opened without write access; a test asserts its bytes never change.
- On its first run against the default database path, Wake's shared path helper migrates an index left by the old `vibex` builds. `wake-mcp` does the same; `--db` skips it entirely.

## Troubleshooting

- **`no Wake index at … — launch Wake once to build it`.** Wake has never run on this machine, or `--db` points at the wrong file. `… is empty or from an older version` means the index predates this Wake version; launching Wake once upgrades it.
- **`No indexed project matches …`.** Pass the absolute path of the repository, or its name. `wake-cli projects` shows the paths Wake knows.
- **Results look stale.** Keep Wake running; the freshness line at the end of every reply says what the index covers. Copilot / OpenCode / Antigravity / Hermes / OpenClaw databases refresh when Wake launches or when you click Refresh.
- **macOS refuses to run it ("cannot be opened because the developer cannot be verified").** Wake is signed but not notarized. Clear the quarantine flag for the whole bundle once: `xattr -dr com.apple.quarantine /Applications/Wake.app`.

## See also

[docs/mcp.md](mcp.md) — the same index over MCP, for clients that call tools rather than run commands.
