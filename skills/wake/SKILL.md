---
name: wake
description: Search and read past coding-agent sessions on this machine. Use when the user refers to earlier conversations, decisions, why something was done, where work stopped, or whether an error was seen before.
---

# Wake — past coding-agent sessions

Wake indexes every coding-agent session on this machine — Claude Code, Codex, Cursor,
Gemini CLI, OpenCode and a dozen more — and `wake-cli` reads that index from a shell.

Reach for it whenever the answer lives in an earlier conversation rather than in the
code: what was discussed, what was decided and why, where previous work stopped, which
approaches were already tried and rejected, whether an error has been seen before. Git
history and the working tree do not hold any of that.

Everything below is read-only. Nothing here can modify or delete a session.

## Finding the binary

```bash
command -v wake-cli || ls /Applications/Wake.app/Contents/MacOS/wake-cli
```

Use whichever resolves. On Linux it is on `PATH` after a deb install, or at
`~/.local/bin/wake-cli` from the tarball; on Windows it sits next to `Wake.exe`. If
neither exists, say so — the user needs [Wake](https://github.com/iAmCorey/Wake), and it
has to have run once to build the index. Do not try to install it yourself.

## Commands

Always scope to the current repository with `--project "$PWD"` unless the user clearly
means "everywhere". Without it, every project on the machine is in scope.

**What happened in this project recently**

```bash
wake-cli sessions --project "$PWD" --limit 10
```

Add `--agent codex` (repeatable), `--since 7d`, or `--starred` to narrow it.

**Where was this discussed**

```bash
wake-cli search "rate limiter" --project "$PWD"
```

Terms are ANDed. CJK text and code substrings like `useEffect(` work. Drop `--project`
when the user asks about something they might have worked on elsewhere.

**Read a session**

```bash
wake-cli show 'claude-code:1b2c3d4e-…'
```

Add `--tools` for tool inputs and outputs, `--thinking` for recorded reasoning. Both are
verbose — start without them.

**Which projects have history**

```bash
wake-cli projects --limit 20
```

## Keys, references and paging

`sessions` and `search` print a key for every session — `<agent>:<id>`, or
`<agent>:<host>:<id>` for a session mirrored from a remote machine. Pass a key to `show`.

`search` also prints `ref: wake://session/<key>#<seq>` under each snippet. Pass the whole
reference (quoted) to `show` and the transcript starts at that message — that is the fast
path from "this term was mentioned" to "here is the surrounding conversation".

Long transcripts are paginated. When the output ends with a `from_seq` hint, run again
with `--from <that number>` to continue.

## Reading the results

- Exit code `0` means it ran — **including "no matches" and "no such project"**. Do not
  treat an empty result as a failure; report that nothing was found.
- Exit `1` means a session could not be read; exit `2` means the command line was wrong
  or there is no index yet. Both print an explanation to stderr.
- Every listing ends with the index freshness (`Index covers activity up to …`). If it
  looks stale, Wake has not been running. `show` does not depend on that line — it reads
  the agent's own file rather than the index. The exception is a session from a remote
  host (`<agent>:<host>:<id>`): that file is a local mirror, current as of Wake's last
  successful sync. Say so rather than reporting it as the other machine's latest state.
- Values out of range are clamped, not rejected: `--limit 999` returns the maximum.

## Sensible defaults

- Start with `sessions --project "$PWD"` when the user says "last time" or "yesterday",
  and with `search` when they name a topic, an error or a decision.
- Read one or two transcripts, not ten — `show` output is large.
- Quote keys and `wake://` references: they contain characters the shell will eat.
