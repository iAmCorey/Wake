#!/usr/bin/env python3
"""生成 Wake 截图/演示用的假家目录——纯合成数据,不含任何真实会话。

用法:
    python3 scripts/demo-home.py                 # 生成到 /tmp/wake-demo-home
    HOME=/tmp/wake-demo-home dist/Wake.app/Contents/MacOS/Wake

app 的索引库与十六家 agent 扫描目录全部落在假 HOME 内,真实数据不显示也不被读。
"""
import json
import os
import random
import shutil
import sqlite3
import sys
import time
import uuid
from datetime import datetime, timedelta, timezone

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

def claude_session(proj, title_msg, turns, age_min, model="claude-fable-5"):
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
                "model": model,
                "content": [{"type": "text", "text": t[1]}],
                "usage": {"input_tokens": 900 + step * 37, "output_tokens": 240 + step * 11}}})
        elif t[0] == "thinking":
            msg_n += 1
            push({"type": "assistant", "message": {
                "id": f"msg_{s[:4]}_{msg_n}", "type": "message", "role": "assistant",
                "model": model,
                "content": [{"type": "thinking", "thinking": t[1]},
                            {"type": "text", "text": t[2]}],
                "usage": {"input_tokens": 1200, "output_tokens": 300}}})
        elif t[0] == "tool":
            _, name, tin, tout = t
            msg_n += 1
            tid = f"toolu_{s[:4]}_{msg_n}"
            push({"type": "assistant", "message": {
                "id": f"msg_{s[:4]}_{msg_n}", "type": "message", "role": "assistant",
                "model": model,
                "content": [{"type": "tool_use", "id": tid, "name": name, "input": tin}]}})
            push({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tid,
                 "content": [{"type": "text", "text": tout}]}]}})
    path = os.path.join(d, f"{s}.jsonl")
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


# ---------------------------------------------------------------- Codex

def codex_session(proj, title_msg, reply, age_min, tool=None, model="gpt-5.2-codex", extra=None):
    """extra: list of (user, assistant) 追加轮次"""
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
            "cwd": proj, "model": model, "summary": "auto"}},
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
    off = 10
    for u, a in extra or []:
        lines.append({"timestamp": iso(age_min, off), "type": "response_item", "payload": {
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": u}]}})
        lines.append({"timestamp": iso(age_min, off + 2), "type": "response_item", "payload": {
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": a}]}})
        off += 4
    with open(os.path.join(d, f"rollout-{stamp}-{s}.jsonl"), "w") as f:
        f.write("\n".join(json.dumps(l, ensure_ascii=False) for l in lines) + "\n")


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
            tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
            time_archived INTEGER, version TEXT);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
        CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
    """)
    t = int((NOW - timedelta(days=6)).timestamp() * 1000)
    c.execute("INSERT INTO session VALUES ('oc-demo-1',NULL,'/Users/demo/Github/blog-engine','Trace the N+1 query in the feed',?,?,'{\"providerID\":\"anthropic\",\"id\":\"claude-sonnet-5\"}',800,400,120,NULL,'1.0.63')", (t, t + 60000))
    c.execute("INSERT INTO message VALUES ('m1','oc-demo-1','{\"id\":\"m1\",\"role\":\"user\",\"time\":{\"created\":%d}}',?)" % (t + 1000), (t + 1000,))
    c.execute("""INSERT INTO part VALUES ('p1','oc-demo-1','m1','{"type":"text","text":"Trace the N+1 query in the feed endpoint"}')""")
    c.commit(); c.close()


# ---------------------------------------------------------------- 其余各家
# fixture 搬运(路径 + 文案替换)+ antigravity 建库:让侧栏 Agents 全亮。
# 各家时间戳保留 fixture 原值(数周前),在列表里自然沉底。

def build_long_tail_agents(g):
    # qoder:主会话 + 子代理边车
    copy_fixture_tree(
        "qoder/projects/-Users-tester-Github-wakefx/abababab-aaaa-bbbb-cccc-000000000014.jsonl",
        ".qoder/projects/-Users-demo-Github-acme-web/abababab-aaaa-bbbb-cccc-000000000014.jsonl",
        [("/Users/tester/Github/wakefx", f"{g}/acme-web"),
         ("Qoder 检查二维码组件的 useEffect() 泄漏", "Audit the retry logic in the upload queue"),
         ("先检查 effect 的依赖和清理回调", "Check the retry guard and the backoff path first"),
         ("我先定位相关 effect。", "Locating the retry entry points."),
         ("src/QrScanner.tsx:42 useEffect(() => watch())", "src/upload/queue.ts:42 retry(task) — no backoff cap")])
    copy_fixture_tree(
        "qoder/projects/-Users-tester-Github-wakefx/abababab-aaaa-bbbb-cccc-000000000014/subagents/agent-child.jsonl",
        ".qoder/projects/-Users-demo-Github-acme-web/abababab-aaaa-bbbb-cccc-000000000014/subagents/agent-child.jsonl",
        [("/Users/tester/Github/wakefx", f"{g}/acme-web")])

    # pi:首行 session 给权威 cwd
    copy_fixture_tree(
        "pi/agent/sessions/--Users-tester-Github-wakefx--/2026-08-06T10-00-00-000Z_66666666-aaaa-bbbb-cccc-000000000006.jsonl",
        ".pi/agent/sessions/--Users-demo-Github-rusty-search--/2026-08-06T10-00-00-000Z_66666666-aaaa-bbbb-cccc-000000000006.jsonl",
        [("/Users/tester/Github/wakefx", f"{g}/rusty-search"),
         ("Pi 查一下二维码组件的 useEffect() 清理", "Chase the flaky scanner test on CI"),
         ("找到泄漏点,已补清理回调。", "The temp dir raced the watcher — pinned with a ready barrier.")])

    # omp(pi 同构,换 id/根/项目)
    copy_fixture_tree(
        "pi/agent/sessions/--Users-tester-Github-wakefx--/2026-08-06T10-00-00-000Z_66666666-aaaa-bbbb-cccc-000000000006.jsonl",
        ".omp/agent/sessions/--Users-demo-Github-data-pipeline--/2026-08-05T15-00-00-000Z_6f6f6f6f-aaaa-bbbb-cccc-000000000026.jsonl",
        [("66666666-aaaa-bbbb-cccc-000000000006", "6f6f6f6f-aaaa-bbbb-cccc-000000000026"),
         ("/Users/tester/Github/wakefx", f"{g}/data-pipeline"),
         ("2026-08-06T10:00", "2026-08-05T15:00"),
         ("Pi 查一下二维码组件的 useEffect() 清理", "Backfill the missing partitions from June"),
         ("找到泄漏点,已补清理回调。", "Backfill ran clean — 42 partitions, checksums verified.")])

    # grok:目录名 = url 编码 cwd
    enc = f"{g}/blog-engine".replace("/", "%2F")
    for name in ("summary.json", "updates.jsonl"):
        copy_fixture_tree(
            f"grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007/{name}",
            f".grok/sessions/{enc}/77777777-aaaa-bbbb-cccc-000000000007/{name}",
            [("/Users/tester/Github/wakefx", f"{g}/blog-engine"),
             ("Grok QR scan cleanup", "Untangle the sitemap cache"),
             ("Grok 看看二维码扫描,", "Why does the sitemap cache "),
             ("重点 useEffect() 清理", "serve stale entries after deploys?"),
             ("用户要查扫描组件的 effect 泄漏", "Cache key misses the deploy stamp"),
             ("已定位泄漏,", "Keyed the cache on the deploy stamp — "),
             ("补了清理回调。", "stale entries gone.")])

    # kimi:state + wire + 根级 index 侧档(cwd 反查靠它)
    copy_fixture_tree(
        "kimi/sessions/wd_wakefx_abc123/session_88888888-aaaa-bbbb-cccc-000000000008/state.json",
        ".kimi-code/sessions/wd_demo_ab12/session_88888888-aaaa-bbbb-cccc-000000000008/state.json",
        [("Kimi QR fix", "Speed up the thumbnail worker")])
    copy_fixture_tree(
        "kimi/sessions/wd_wakefx_abc123/session_88888888-aaaa-bbbb-cccc-000000000008/agents/main/wire.jsonl",
        ".kimi-code/sessions/wd_demo_ab12/session_88888888-aaaa-bbbb-cccc-000000000008/agents/main/wire.jsonl",
        [("Kimi 修一下二维码组件的 useEffect() 内存泄漏", "Speed up the thumbnail worker — it pegs a core"),
         ("已定位 QrScanner 泄漏并补了清理回调。", "The worker re-decoded the source per size; decode once and scale, CPU 92% → 11%.")])
    with open(os.path.join(HOME, ".kimi-code", "session_index.jsonl"), "w") as f:
        f.write(json.dumps({
            "sessionId": "session_88888888-aaaa-bbbb-cccc-000000000008",
            "sessionDir": os.path.join(HOME, ".kimi-code", "sessions", "wd_demo_ab12"),
            "workDir": f"{g}/oss-metrics"}) + "\n")

    # dsh:目录名是转义随意值,cwd 从首行读;明文 .jsonl 与 .zstd 同被接受
    copy_fixture_tree(
        "dsh/session.jsonl",
        ".dsh/sessions/--Users-demo-Github-acme-web--/dsh-demo-0001/session.jsonl",
        [("/Users/tester/Github/wakefx", f"{g}/acme-web"),
         ("dsh-e2e4-0001", "dsh-demo-0001"),
         ("二维码扫描闪退,帮我看看 useEffect() 的依赖", "Why does the export splitter drop the last chunk?"),
         ("crash on unmount 通常是依赖数组遗漏", "Last-chunk drops usually mean a floor where it needs a ceil"),
         ("我先查一下扫码组件", "Reading the splitter first"),
         ("useEffect(() => scan(device), [])", "chunks = len // size  # drops the tail")])

    # antigravity:唯一明文是 summaries 库(正文加密,元数据卡片)
    ag_dir = os.path.join(HOME, ".gemini", "antigravity-cli")
    os.makedirs(ag_dir, exist_ok=True)
    c = sqlite3.connect(os.path.join(ag_dir, "conversation_summaries.db"))
    c.executescript("""
        CREATE TABLE conversation_summaries (
            conversation_id TEXT PRIMARY KEY, title TEXT, preview TEXT,
            step_count INTEGER, last_modified_time TEXT, workspace_uris TEXT,
            parent_conversation_id TEXT DEFAULT '', nesting_depth INTEGER DEFAULT 0);
    """)
    ag_ts = (NOW - timedelta(days=3)).strftime("%Y-%m-%d %H:%M:%S+00:00")
    c.execute(
        "INSERT INTO conversation_summaries VALUES (?,?,?,?,?,?, '', 0)",
        ("ag-demo-1", None,
         "Sketch a migration plan for the media store — object storage with a local cache tier",
         14, ag_ts, json.dumps([f"file://{g}/data-pipeline"])))
    c.commit(); c.close()


# ---------------------------------------------------------------- 批量历史
# Insights 页(0.3.0)需要成规模的活动史:热力图/时段分布/streak/榜单都从
# 这里来。random.seed 固定,同一台机器每次生成完全一致(截图可复现)。

BULK_TOPICS = [
    ("Fix the race in the job queue drain", "The drain loop dropped jobs enqueued mid-flush; swapped to a swap-and-drain pattern."),
    ("Why is the settings pane re-rendering on every keystroke?", "The whole store was passed as a prop — memoized the slice, renders went from 40/s to 2/s."),
    ("Add retry with backoff to the sync client", "Exponential backoff with jitter, capped at 30s; gave up-state now surfaces in the UI."),
    ("Write property tests for the date parser", "Ten properties incl. roundtrip and timezone edges — found one real bug at the DST boundary."),
    ("Speed up the cold build", "Split the mega-module and enabled incremental linking; cold build 4m10s → 1m20s."),
    ("Migrate the config from YAML to TOML", "Ported with a compat shim that warns on old paths for one release."),
    ("Why does the CLI hang on Ctrl-C during upload?", "The signal handler waited on the same channel the uploader held; detached the aborter."),
    ("Refactor the theme tokens into a single palette file", "All 23 hardcoded colors now come from tokens; both modes pass contrast checks."),
    ("Add pagination to the audit log endpoint", "Cursor-based with a stable tiebreaker; last-page off-by-one covered by a test."),
    ("Debug the memory spike during import", "The CSV reader buffered whole rows of blobs — switched to streaming, peak RSS 1.9G → 240M."),
    ("Explain this lifetime error in the cache layer", "You're returning a ref tied to the guard; return an owned Arc clone instead."),
    ("Set up integration tests against a temp database", "Each test gets an isolated tmp DB with migrations applied; suite runs in 3s."),
    ("Make the file watcher survive editor atomic saves", "Watch the parent dir and re-arm on rename — vim/VSCode saves no longer drop events."),
    ("Add a --json flag to the export command", "Structured output with a stable schema; docs and a golden test included."),
    ("优化列表滚动的掉帧", "行高缓存 + 惰性测量,10k 行列表滚动稳定 120fps。"),
    ("给部署脚本加回滚", "保留上一版产物与迁移点,rollback 一条命令,演练过两轮。"),
    ("Trace the flaky websocket test", "The test raced the server boot; awaiting the ready signal fixed 30 red runs in a row."),
    ("Harden the input parser against malformed UTF-8", "Lossy decode at the boundary, strict inside; fuzzer ran clean for 10M cases."),
    ("Why is the search index twice the data size?", "Trigram over raw JSON — indexing extracted text only cut it to 0.4x."),
    ("Add keyboard shortcuts to the review pane", "j/k navigate, x toggles, ? shows the map — all remappable."),
    ("Profile the startup path", "Font enumeration blocked first paint; deferred it, first frame at 90ms."),
    ("Clean up the error taxonomy", "Three layers collapsed into one enum with source chains; logs finally greppable."),
    ("把上传队列改成断点续传", "分片 + etag 校验,弱网下实测断网重连不丢块。"),
    ("Add dark mode to the docs site", "Token pass + prefers-color-scheme, with a manual override stored per-device."),
    ("Dedupe the retry storm in the poller", "Coalesced concurrent polls behind a single-flight guard; API calls dropped 8x."),
    ("Ship a health endpoint with build info", "Version, commit, uptime and index freshness — the deploy script now gates on it."),
]

BULK_MULTI = [
    ("Design the migration for splitting the users table",
     [("What about the foreign keys on sessions?", "Dual-write during the window, then flip reads behind a flag; FKs re-pointed last."),
      ("How long is the backfill?", "Batched at 10k rows/s it's ~40 minutes; runs online with progress in the ops channel.")]),
    ("Review my approach to the plugin sandbox",
     [("Can plugins escape via the require cache?", "Yes as written — froze the resolver and allow-listed builtins; added an escape test.")]),
    ("帮我把发布流程自动化",
     [("产物校验怎么做?", "对六个产物各跑一遍冒烟 + 校验和,任何一个失败整体不发。"),
      ("旧版本怎么清理?", "保留最近三个 Release,更早的产物归档到冷存储。")]),
    ("Debug the intermittent 502 behind the proxy",
     [("Is it keepalive related?", "Exactly — upstream idle timeout was shorter than the proxy's; aligned them and the 502s stopped.")]),
]

BULK_FOLLOWUPS = [
    ("Does that hold under concurrent writes?", "Yes — the guard serializes the critical section; added a loom test to pin it."),
    ("Add a test for the edge case?", "Done — regression test covers the boundary and the empty input."),
    ("What's the perf cost?", "Measured: +0.3ms p50, nothing at p99 — the cache absorbs it."),
    ("能不能兼容旧数据?", "可以,读侧做了版本探测,旧格式走迁移分支,新写入一律新格式。"),
]

# 本地时段权重:上午 10-12、下午 14-19、晚 21-23 高,凌晨接近零
HOUR_WEIGHTS = [1, 1, 0, 0, 0, 0, 1, 2, 4, 7, 10, 11, 9, 6, 10, 12, 12, 11, 9, 6, 7, 9, 8, 4]


def bulk_history(g):
    rng = random.Random(42)
    local = datetime.now().astimezone()
    projects = [(f"{g}/acme-web", 28), (f"{g}/rusty-search", 20), (f"{g}/blog-engine", 17),
                (f"{g}/oss-metrics", 15), (f"{g}/data-pipeline", 12), (f"{g}/dotfiles", 8)]
    pnames = [p for p, _ in projects]
    pweights = [w for _, w in projects]
    claude_models = ["claude-fable-5", "claude-fable-5", "claude-opus-4-5", "claude-sonnet-5"]
    codex_models = ["gpt-5.2-codex", "gpt-5.2-codex", "gpt-5.2"]
    count = 0
    for days_ago in range(299, -1, -1):
        day = local - timedelta(days=days_ago)
        base = 0.85 if day.weekday() < 5 else 0.38
        growth = 0.5 if days_ago > 200 else (0.8 if days_ago > 100 else 1.0)
        # 最近 12 天强制活跃:current streak 有东西可看
        if rng.random() >= base * growth and days_ago > 11:
            continue
        n = 9 if days_ago == 37 else rng.choice([1, 1, 1, 2, 2, 2, 3, 3, 4])
        for _ in range(n):
            hour = rng.choices(range(24), weights=HOUR_WEIGHTS)[0]
            target = day.replace(hour=hour, minute=rng.randrange(60), second=0, microsecond=0)
            if target >= local:  # 今天的未来时段回拨到最近几小时
                target = local - timedelta(minutes=rng.randrange(30, 240))
            age_min = (local - target).total_seconds() / 60
            proj = rng.choices(pnames, weights=pweights)[0]
            if rng.random() < 0.55:
                if rng.random() < 0.18:
                    title, pairs = rng.choice(BULK_MULTI)
                    turns = []
                    for u, a in pairs:
                        turns += [("user", u), ("assistant", a)]
                else:
                    title, reply = rng.choice(BULK_TOPICS)
                    turns = [("assistant", reply)]
                    if rng.random() < 0.5:
                        fu, fa = rng.choice(BULK_FOLLOWUPS)
                        turns += [("user", fu), ("assistant", fa)]
                claude_session(proj, title, turns, age_min, model=rng.choice(claude_models))
            else:
                title, reply = rng.choice(BULK_TOPICS)
                extra = [rng.choice(BULK_FOLLOWUPS)] if rng.random() < 0.35 else None
                codex_session(proj, title, reply, age_min,
                              model=rng.choice(codex_models), extra=extra)
            count += 1
    return count


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


    # hermes:全明文 SQLite(cli + telegram 两条,含工具调用与 reasoning)
    hermes_dir = os.path.join(HOME, ".hermes")
    os.makedirs(hermes_dir, exist_ok=True)
    c = sqlite3.connect(os.path.join(hermes_dir, "state.db"))
    c.executescript("""
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT,
            started_at REAL NOT NULL, ended_at REAL, message_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0, title TEXT);
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
            content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
            timestamp REAL NOT NULL, reasoning TEXT);
    """)
    h1 = (NOW - timedelta(hours=9)).timestamp()
    h2 = (NOW - timedelta(days=2, hours=3)).timestamp()
    c.execute("INSERT INTO sessions VALUES ('hermes-demo-1','cli','gpt-5.2',NULL,?,?,4,1800,320,600,0,90,'Rotate the API keys across services')", (h1, h1 + 240))
    c.execute("INSERT INTO sessions VALUES ('hermes-demo-2','telegram','claude-fable-5',NULL,?,?,2,400,120,0,0,0,NULL)", (h2, h2 + 60))
    c.executemany("INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, reasoning) VALUES (?,?,?,?,?,?,?,?)", [
        ("hermes-demo-1", "user", "Rotate the API keys for the staging services and update the secrets store", None, None, None, h1 + 5, None),
        ("hermes-demo-1", "assistant", None, None, json.dumps([{"id": "call_h1", "type": "function", "function": {"name": "terminal", "arguments": json.dumps({"command": "ls secrets/staging"})}}]), None, h1 + 8, "Need the current key inventory before touching anything."),
        ("hermes-demo-1", "tool", "api.env  db.env  queue.env", "call_h1", None, "terminal", h1 + 9, None),
        ("hermes-demo-1", "assistant", "Three services found. Rotated all three keys, updated the secrets store, and verified each service picked up the new value on restart.", None, None, None, h1 + 200, None),
        ("hermes-demo-1", "user", "Thanks, log it in the runbook", None, None, None, h1 + 230, None),
        ("hermes-demo-1", "assistant", "Runbook entry added under Rotations with today's date and the three service names.", None, None, None, h1 + 240, None),
        ("hermes-demo-2", "user", "Summarize what changed in the data-pipeline repo this week", None, None, None, h2 + 5, None),
        ("hermes-demo-2", "assistant", "Two changes: the June partition backfill landed, and the retry storm in the poller was fixed with a single-flight guard.", None, None, None, h2 + 60, None),
    ])
    c.commit(); c.close()

    # openclaw:旧版 sessions/*.jsonl 转录 + sessions.json 索引(fixture 搬运换文案)
    copy_fixture_tree(
        "openclaw/agents/main/sessions/cccccccc-aaaa-bbbb-cccc-000000000017.jsonl",
        ".openclaw/agents/main/sessions/cccccccc-aaaa-bbbb-cccc-000000000017.jsonl",
        [("/Users/tester/Github/wakefx", f"{g}/blog-engine"),
         ("OpenClaw QR cleanup", "Draft the weekly changelog post"),
         ("OpenClaw 看看二维码组件的 useEffect() 清理", "Draft this week's changelog post from the merged PRs"),
         ("找到泄漏点,已补清理回调。", "Drafted from 14 merged PRs, grouped by area, with links — ready for review."),
         ("之前的对话:排查二维码组件 useEffect 泄漏并修复", "Earlier: drafted the changelog post from merged PRs"),
         ("谢谢,合并了", "Looks good, publish it")])
    copy_fixture_tree(
        "openclaw/agents/main/sessions/sessions.json",
        ".openclaw/agents/main/sessions/sessions.json",
        [])

    build_sqlite_agents()
    build_long_tail_agents(g)
    bulk = bulk_history(g)
    print(f"demo home ready: {HOME} ({bulk} bulk sessions + curated set)")
    print(f"run:  HOME={HOME} {REPO}/dist/Wake.app/Contents/MacOS/Wake")


if __name__ == "__main__":
    main()
