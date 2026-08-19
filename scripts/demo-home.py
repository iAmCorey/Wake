#!/usr/bin/env python3
"""生成 Wake 截图/演示用的假家目录——纯合成数据,不含任何真实会话。

用法:
    python3 scripts/demo-home.py                 # 生成到 /tmp/wake-demo-home
    HOME=/tmp/wake-demo-home dist/Wake.app/Contents/MacOS/Wake

app 的索引库与各家 agent 扫描目录全部落在假 HOME 内,真实数据不显示也不被读。
"""
import json
import os
import shutil
import sqlite3
import sys
import time
import uuid
from datetime import datetime, timedelta, timezone
from urllib.parse import quote

HOME = sys.argv[1] if len(sys.argv) > 1 else "/tmp/wake-demo-home"
NOW = datetime.now(timezone.utc)
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURES = os.path.join(REPO, "crates", "wake-core", "tests", "fixtures")


def iso(minutes_ago, offset_s=0):
    t = NOW - timedelta(minutes=minutes_ago) + timedelta(seconds=offset_s)
    return t.strftime("%Y-%m-%dT%H:%M:%S.000Z")


def sid():
    return str(uuid.uuid4())


# ---------------------------------------------------------------- Claude

def claude_session(proj, title_msg, turns, age_min):
    """turns: list of ("user"|"assistant", text) 或 ("tool", name, input, result)"""
    s = sid()
    enc = proj.replace("/", "-")
    d = os.path.join(HOME, ".claude", "projects", enc)
    os.makedirs(d, exist_ok=True)
    lines = []
    prev = None
    step = 0

    def push(obj):
        nonlocal prev, step
        u = f"u-{step}"
        obj.update({"parentUuid": prev, "isSidechain": False, "cwd": proj,
                    "gitBranch": "main", "sessionId": s, "uuid": u,
                    "timestamp": iso(age_min, step * 20)})
        lines.append(json.dumps(obj, ensure_ascii=False))
        prev = u
        step += 1

    push({"type": "user", "message": {"role": "user", "content": title_msg}})
    msg_n = 0
    for t in turns:
        if t[0] == "user":
            push({"type": "user", "message": {"role": "user", "content": t[1]}})
        elif t[0] == "assistant":
            msg_n += 1
            push({"type": "assistant", "message": {
                "id": f"msg_{s[:4]}_{msg_n}", "type": "message", "role": "assistant",
                "model": "claude-fable-5",
                "content": [{"type": "text", "text": t[1]}],
                "usage": {"input_tokens": 900 + step * 37, "output_tokens": 240 + step * 11}}})
        elif t[0] == "thinking":
            msg_n += 1
            push({"type": "assistant", "message": {
                "id": f"msg_{s[:4]}_{msg_n}", "type": "message", "role": "assistant",
                "model": "claude-fable-5",
                "content": [{"type": "thinking", "thinking": t[1]},
                            {"type": "text", "text": t[2]}],
                "usage": {"input_tokens": 1200, "output_tokens": 300}}})
        elif t[0] == "tool":
            _, name, tin, tout = t
            msg_n += 1
            tid = f"toolu_{s[:4]}_{msg_n}"
            push({"type": "assistant", "message": {
                "id": f"msg_{s[:4]}_{msg_n}", "type": "message", "role": "assistant",
                "model": "claude-fable-5",
                "content": [{"type": "tool_use", "id": tid, "name": name, "input": tin}]}})
            push({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tid,
                 "content": [{"type": "text", "text": tout}]}]}})
    path = os.path.join(d, f"{s}.jsonl")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


# ---------------------------------------------------------------- Codex

def codex_session(proj, title_msg, reply, age_min, tool=None):
    s = sid()
    t0 = NOW - timedelta(minutes=age_min)
    d = os.path.join(HOME, ".codex", "sessions", t0.strftime("%Y/%m/%d"))
    os.makedirs(d, exist_ok=True)
    stamp = t0.strftime("%Y-%m-%dT%H-%M-%S")
    lines = [
        {"timestamp": iso(age_min), "type": "session_meta", "payload": {
            "id": s, "timestamp": iso(age_min), "cwd": proj,
            "originator": "codex_cli_rs", "cli_version": "0.21.0", "source": "cli",
            "git": {"branch": "main"}}},
        {"timestamp": iso(age_min, 1), "type": "turn_context", "payload": {
            "cwd": proj, "model": "gpt-5.2-codex", "summary": "auto"}},
        {"timestamp": iso(age_min, 2), "type": "response_item", "payload": {
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": title_msg}]}},
    ]
    if tool:
        cmd, out = tool
        lines += [
            {"timestamp": iso(age_min, 4), "type": "response_item", "payload": {
                "type": "function_call", "name": "shell",
                "arguments": json.dumps({"command": cmd}), "call_id": "call_1"}},
            {"timestamp": iso(age_min, 6), "type": "response_item", "payload": {
                "type": "function_call_output", "call_id": "call_1", "output": out}},
        ]
    lines.append({"timestamp": iso(age_min, 9), "type": "response_item", "payload": {
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": reply}]}})
    with open(os.path.join(d, f"rollout-{stamp}-{s}.jsonl"), "w") as f:
        f.write("\n".join(json.dumps(l, ensure_ascii=False) for l in lines) + "\n")


# ---------------------------------------------------------------- Grok Build

def grok_session(proj, title_msg, reply, age_min):
    s = sid()
    enc = quote(proj, safe="")
    d = os.path.join(HOME, ".grok", "sessions", enc, s)
    os.makedirs(d, exist_ok=True)
    t0 = iso(age_min)
    # t1 比最后一条事件(+12s)更晚,adapter 取 max(summary, 流) → updated_at 用它
    t1 = iso(age_min, 20)
    base_ms = int((NOW - timedelta(minutes=age_min)).timestamp() * 1000)

    def ev(update, offset):
        return {
            "timestamp": base_ms // 1000 + offset,
            "method": "session/update",
            "params": {
                "sessionId": s,
                "update": update,
                "_meta": {"agentTimestampMs": base_ms + offset * 1000},
            },
        }

    lines = [
        ev({"sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": title_msg},
            "_meta": {"modelId": "grok-4.6"}}, 0),
        ev({"sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "Scan session layout then add an adapter."},
            "_meta": {"modelId": "grok-4.6"}}, 5),
        ev({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": reply},
            "_meta": {"modelId": "grok-4.6"}}, 9),
        ev({"sessionUpdate": "turn_completed",
            "usage": {"totalTokens": 1800, "outputTokens": 240}}, 12),
    ]
    with open(os.path.join(d, "updates.jsonl"), "w") as f:
        f.write("\n".join(json.dumps(l, ensure_ascii=False) for l in lines) + "\n")
    with open(os.path.join(d, "summary.json"), "w") as f:
        json.dump({
            "info": {"id": s, "cwd": proj},
            "generated_title": title_msg[:80],
            "created_at": t0,
            "updated_at": t1,
            "current_model_id": "grok-4.6",
            "head_branch": "main",
        }, f)


# ---------------------------------------------------------------- 其余五家

def copy_fixture_tree(rel_src, rel_dst, replacements):
    src = os.path.join(FIXTURES, rel_src)
    dst = os.path.join(HOME, rel_dst)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(src) as f:
        text = f.read()
    for a, b in replacements:
        text = text.replace(a, b)
    with open(dst, "w") as f:
        f.write(text)


def build_sqlite_agents():
    cop_dir = os.path.join(HOME, ".copilot")
    os.makedirs(cop_dir, exist_ok=True)
    c = sqlite3.connect(os.path.join(cop_dir, "session-store.db"))
    c.executescript("""
        CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, branch TEXT, summary TEXT,
            created_at TEXT, updated_at TEXT);
        CREATE TABLE turns (id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER,
            user_message TEXT, assistant_response TEXT, timestamp TEXT);
    """)
    ts = (NOW - timedelta(days=4)).strftime("%Y-%m-%d %H:%M:%S")
    c.execute("INSERT INTO sessions VALUES ('cop-demo-1','/Users/demo/Github/oss-metrics','main','Review PR #42 error handling',?,?)", (ts, ts))
    c.execute("INSERT INTO turns VALUES (1,'cop-demo-1',0,'Review the error handling in PR #42','The retry loop swallows the root cause; surfaced it in the log line.',?)", (ts,))
    c.commit(); c.close()

    oc_dir = os.path.join(HOME, ".local", "share", "opencode")
    os.makedirs(oc_dir, exist_ok=True)
    c = sqlite3.connect(os.path.join(oc_dir, "opencode.db"))
    c.executescript("""
        CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT,
            time_created INTEGER, time_updated INTEGER, model TEXT,
            tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER, time_archived INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
        CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
    """)
    t = int((NOW - timedelta(days=6)).timestamp() * 1000)
    c.execute("INSERT INTO session VALUES ('oc-demo-1',NULL,'/Users/demo/Github/blog-engine','Trace the N+1 query in the feed',?,?,'{\"providerID\":\"anthropic\",\"id\":\"claude-sonnet-5\"}',800,400,120,NULL)", (t, t + 60000))
    c.execute("INSERT INTO message VALUES ('m1','oc-demo-1','{\"id\":\"m1\",\"role\":\"user\",\"time\":{\"created\":%d}}',?)" % (t + 1000), (t + 1000,))
    c.execute("""INSERT INTO part VALUES ('p1','oc-demo-1','m1','{"type":"text","text":"Trace the N+1 query in the feed endpoint"}')""")
    c.commit(); c.close()


# ---------------------------------------------------------------- 数据集

def main():
    if os.path.exists(HOME):
        shutil.rmtree(HOME)
    os.makedirs(HOME)

    g = "/Users/demo/Github"

    # hero 会话(最新,详情页展示用):多轮 + thinking + 工具簇 + markdown
    claude_session(f"{g}/acme-web", "Add full-text search to the sessions list — needs to handle CJK and code substrings like useEffect(", [
        ("thinking", "FTS5 的默认 tokenizer 对 CJK 无能为力,trigram 能同时覆盖中文和代码子串。先看现有 schema 再决定迁移方案。",
         "Good constraint — plain FTS5 tokenizers won't handle CJK or `useEffect(` substrings. I'll use a **trigram tokenizer**. Let me check the current schema first."),
        ("tool", "Read", {"file_path": f"{g}/acme-web/src/db.rs"},
         "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_key TEXT, seq INTEGER, text TEXT);"),
        ("tool", "Grep", {"pattern": "messages_fts", "path": "src/"},
         "no matches"),
        ("assistant", "No FTS table yet. Here's the migration:\n\n```sql\nCREATE VIRTUAL TABLE messages_fts USING fts5(\n  text,\n  content = 'messages',\n  content_rowid = 'id',\n  tokenize = 'trigram'\n);\n```\n\nTrigram indexes every 3-char window, so `二维码` and `useEffect(` both match without language-specific tokenizers. Queries under 3 chars fall back to `LIKE` — I'll flag those as degraded in the UI."),
        ("user", "中文短词比如两个字的怎么办?"),
        ("assistant", "两个字(两码点)低于 trigram 下限,走 `LIKE '%..%'` 降级路径——在 546 万行的表上实测 ~40ms,可接受。UI 上会提示「短查询已降级,长关键词更快」。我把这条也写进测试。"),
        ("tool", "Edit", {"file_path": f"{g}/acme-web/src/db.rs", "old_string": "// TODO fts", "new_string": "CREATE VIRTUAL TABLE messages_fts USING fts5(...)"},
         "ok"),
        ("assistant", "Done — migration applied, 16 tests green, search returns in under a millisecond on 280k rows."),
    ], 25)

    claude_session(f"{g}/acme-web", "Fix flaky WebSocket reconnect after wake from sleep", [
        ("tool", "Grep", {"pattern": "reconnect", "path": "src/net/"}, "src/net/ws.ts:88: setTimeout(reconnect, backoff)"),
        ("assistant", "The backoff timer survives sleep, so on wake it fires with a stale socket. Switching to an event-driven reconnect on `online` + visibility change."),
    ], 180)
    claude_session(f"{g}/acme-web", "Dark mode: design tokens for the settings page", [
        ("assistant", "Mapped all 14 hardcoded grays to semantic tokens; both palettes now pass 4.5:1 contrast."),
    ], 60 * 26)
    claude_session(f"{g}/rusty-search", "Explain this borrow checker error in indexer.rs", [
        ("assistant", "You're holding an immutable borrow of `self.index` across the `&mut self` call in the loop — clone the key list first, or restructure with `split_at_mut`."),
    ], 60 * 5)
    claude_session(f"{g}/rusty-search", "Benchmark FTS5 trigram vs LIKE fallback", [
        ("tool", "Bash", {"command": "cargo bench --bench search"}, "trigram: 0.9ms  like: 41ms  (280k rows)"),
        ("assistant", "Trigram wins by ~45x on realistic data; keeping LIKE only as the short-query fallback."),
    ], 60 * 49),
    claude_session(f"{g}/rusty-search", "重构索引重建的进度上报", [
        ("assistant", "把进度事件改为 Drop guard 兜底,任何提前返回路径都保证发出终态——UI 不会再卡在加载态。"),
    ], 60 * 96)
    claude_session(f"{g}/blog-engine", "Migrate CI from CircleCI to GitHub Actions", [
        ("tool", "Write", {"file_path": ".github/workflows/ci.yml", "content": "name: CI"}, "ok"),
        ("assistant", "Workflow ported; cache hit brings the build from 9m to 80s."),
    ], 60 * 72)
    claude_session(f"{g}/blog-engine", "Write integration tests for the RSS feed", [
        ("assistant", "Added 6 cases incl. CDATA titles and pubDate timezones — found one real bug in the encoder."),
    ], 60 * 24 * 6)
    claude_session(f"{g}/dotfiles", "Set up tmux + zsh keybindings", [
        ("assistant", "Prefix moved to C-a, vi-mode copy bindings, and fzf history on C-r."),
    ], 60 * 24 * 8)
    claude_session(f"{g}/oss-metrics", "Debug memory leak in the image pipeline", [
        ("tool", "Bash", {"command": "leaks --atExit -- ./target/debug/pipeline"}, "3 leaks for 48KB: decoder ring buffer"),
        ("assistant", "The decoder ring buffer is never drained on the error path — fixed with a Drop impl."),
    ], 60 * 24 * 10)
    claude_session(f"{g}/oss-metrics", "Paginate the contributors API", [
        ("assistant", "Cursor-based pagination with a stable sort key; off-by-one on the last page fixed."),
    ], 60 * 24 * 12)
    claude_session(f"{g}/oss-metrics", "优化首屏加载速度", [
        ("assistant", "关键路径上的三个串行请求并行化,LCP 从 2.8s 降到 1.1s;字体子集化又省了 180KB。"),
    ], 60 * 24 * 15)

    codex_session(f"{g}/acme-web", "Refactor the plugin loader to async", "Loader is fully async now; startup no longer blocks on plugin IO.", 60 * 7,
                  tool=(["rg", "load_plugin", "src"], "src/plugins.rs:41: pub fn load_plugin(...)"))
    codex_session(f"{g}/acme-web", "Fix off-by-one in pagination", "The cursor skipped the boundary row; inclusive compare fixes it.", 60 * 36)
    codex_session(f"{g}/rusty-search", "Add a benchmark harness for the scanner", "Criterion harness added with three fixed-load scenarios.", 60 * 48,
                  tool=(["cargo", "bench"], "scan_cold: 4.1s  scan_warm: 0.09s"))
    codex_session(f"{g}/blog-engine", "Upgrade to Tailwind v4", "Migrated config to CSS-first; purge list no longer needed.", 60 * 24 * 5)
    codex_session(f"{g}/oss-metrics", "Profile the slow cold start", "Cold start was dominated by sync font loading; now deferred.", 60 * 24 * 9)

    # cursor / kiro / gemini:复制 fixture 骨架,替换成演示文案
    copy_fixture_tree(
        "cursor/projects/wakefx-cursor-proj/agent-transcripts/33333333-aaaa-bbbb-cccc-000000000003/33333333-aaaa-bbbb-cccc-000000000003.jsonl",
        ".cursor/projects/demo-acme-web/agent-transcripts/33333333-aaaa-bbbb-cccc-000000000003/33333333-aaaa-bbbb-cccc-000000000003.jsonl",
        [("wakefx", "acme-web"),
         ("把二维码扫描组件抽出来,注意 useEffect() 的清理", "Extract the QR scanner into a reusable hook"),
         ("先看现有组件结构,再抽公共 hook。", "Looking at the component structure first, then extracting a shared hook.")])
    copy_fixture_tree(
        "kiro/sessions/cli/44444444-aaaa-bbbb-cccc-000000000004.jsonl",
        ".kiro/sessions/cli/44444444-aaaa-bbbb-cccc-000000000004.jsonl",
        [("用 Kiro 重构二维码扫描,注意 useEffect() 清理", "Fix the CLI arg parser on quoted paths"),
         ("已拆分扫描组件并补上清理逻辑。", "Quoted paths now round-trip through the parser; added a regression test.")])
    copy_fixture_tree(
        "kiro/sessions/cli/44444444-aaaa-bbbb-cccc-000000000004.json",
        ".kiro/sessions/cli/44444444-aaaa-bbbb-cccc-000000000004.json",
        [("/Users/tester/Github/wakefx", f"{g}/dotfiles"),
         ("Kiro QR session", "Fix the CLI arg parser on quoted paths")])
    copy_fixture_tree(
        "gemini/tmp/wakefx-gem/chats/session-2026-08-04T12-00-00.jsonl",
        ".gemini/tmp/demo-gem/chats/session-2026-08-04T12-00-00.jsonl",
        [("wakefx-gem", "demo-gem"),
         ("Gemini 帮我调试二维码解码,顺带看 useEffect()", "Why does the sitemap generator skip drafts?"),
         ("可以,先在解码回调里打日志。", "Drafts carry a null publish date — the generator filters on it; I added a flag to include them.")])
    os.makedirs(os.path.join(HOME, ".gemini"), exist_ok=True)
    with open(os.path.join(HOME, ".gemini", "projects.json"), "w") as f:
        json.dump({"projects": {f"{g}/blog-engine": "demo-gem"}}, f)

    grok_session(f"{g}/acme-web", "Wire Grok Build into the session library",
                 "Adapter reads ~/.grok/sessions; resume is grok --resume <id>.", 40)

    build_sqlite_agents()
    print(f"demo home ready: {HOME}")
    print(f"run:  HOME={HOME} {REPO}/dist/Wake.app/Contents/MacOS/Wake")


if __name__ == "__main__":
    main()
