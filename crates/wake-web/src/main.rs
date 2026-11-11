//! Wake 的只读网页版。
//!
//! 数据根(`WAKE_DATA`)下**每个子目录是一台机器**,目录名就是机器名;子目录里的布局与
//! 那台机器 home 下一致(`.claude/projects`、`.codex/sessions`…)——也就是桌面版归档
//! (`wake_core::vault`)的布局,把各台电脑的归档原样同步上来即可。每个子目录按一台
//! "远程主机"挂进 roster(`adapters::remote`),会话 key 带机器名,互不相撞。
//!
//! 环境变量:
//! - `WAKE_DATA`     数据根(默认 `/data`),建议只读挂载
//! - `WAKE_INDEX`    索引库目录(默认 `/index`),可随时删掉重建
//! - `WAKE_PASSWORD` 访问密码(可选;不设就不用登录——只适合放在局域网或 VPN 后面)
//! - `WAKE_LISTEN`   监听地址(默认 `0.0.0.0:8080`)
//! - `WAKE_SCAN_SECS` 增量扫描间隔秒数(默认 30)

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};
use wake_core::adapters::remote::create_remote_adapters;
use wake_core::adapters::{adapter_for, create_adapters, AgentAdapter};
use wake_core::db::{IndexLock, Ownership, Store};
use wake_core::models::*;
use wake_core::scanner::{run_scan, NullEvents};

const INDEX_HTML: &str = include_str!("../web/index.html");
/// 桌面版的图标与各家品牌图:网页版直接用同一份,两边不会各画各的
static ASSETS: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../wake/assets");
const COOKIE: &str = "wake_session";
/// 列表一次最多取这么多根会话再按机器筛(SessionFilter 没有 host 维度;个人量级足够)
const LIST_CAP: i64 = 20_000;

type Adapters = Arc<Vec<Box<dyn AgentAdapter>>>;

struct App {
    store: Arc<Store>,
    adapters: RwLock<Adapters>,
    data: PathBuf,
    /// None = 不设防(显式允许时)
    password: Option<String>,
    /// 登录后发的 cookie 值;进程重启即失效
    token: String,
    index_path: PathBuf,
    scan_secs: u64,
    /// SKILL.md 的解析缓存(Skills 页每次进页现扫,没变的文件不重读)
    skills: wake_core::skills::SkillCache,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 数据根下的机器目录 → (机器名, 路径)。名字要过 `valid_host_name`(进 session key 的
/// 中段,不能带 ':' 与空格);不合规的目录跳过并提示改名
fn machines(data: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(data)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with('@') || name.starts_with('#') {
                return None; // NAS 自己的隐藏 / 回收站 / 缩略图目录
            }
            if !wake_core::remote::valid_host_name(&name) {
                eprintln!(
                    "[wake-web] skipping {name:?}: machine folder names may only use letters, digits, '.', '_', '-'"
                );
                return None;
            }
            Some((name, e.path()))
        })
        .collect();
    out.sort();
    out
}

fn build_adapters(data: &Path) -> Vec<Box<dyn AgentAdapter>> {
    let templates = create_adapters();
    machines(data)
        .iter()
        .flat_map(|(name, dir)| create_remote_adapters(&templates, name, dir))
        .collect()
}

/// 128 位的进程级随机串:`RandomState` 的种子来自系统随机源,不为它引入 rand
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

/// 不因内容提前返回的比较
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (_, Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b'+', _) => out.push(b' '),
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(url: &str) -> HashMap<String, String> {
    url.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

type Reply = Response<std::io::Cursor<Vec<u8>>>;

fn reply(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Reply {
    Response::from_data(body.into())
        .with_status_code(status)
        .with_header(header("Content-Type", content_type))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
}

fn json_reply(status: u16, value: Value) -> Reply {
    reply(status, "application/json; charset=utf-8", value.to_string())
}

fn error(status: u16, message: &str) -> Reply {
    json_reply(status, json!({ "error": message }))
}

fn authed(app: &App, req: &Request) -> bool {
    if app.password.is_none() {
        return true;
    }
    req.headers()
        .iter()
        .filter(|h| h.field.equiv("Cookie"))
        .flat_map(|h| h.value.as_str().split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && same(v, &app.token))
}

fn session_json(meta: &SessionMeta) -> Value {
    json!({
        "key": meta.key,
        "id": meta.id,
        "host": meta.host,
        "agent": meta.agent.as_str(),
        "agent_name": meta.agent.display_name(),
        "title": meta.title,
        "project_path": meta.project_path,
        "project_name": meta.project_name,
        "created_at": meta.created_at,
        "updated_at": meta.updated_at,
        "message_count": meta.message_count,
        "size_bytes": meta.size_bytes,
        "git_branch": meta.git_branch,
        "model": meta.model,
        "tokens_used": meta.tokens_used,
        "tokens": wake_core::services::exporter::fmt_tokens(meta.tokens_used),
        "favorite": meta.favorite,
        "pinned": meta.pinned,
        "icon": meta.agent.brand_icon(false),
        "icon_dark": meta.agent.brand_icon(true),
    })
}

fn message_json(m: &TranscriptMessage) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    json!({
        "seq": m.seq,
        "role": m.role.as_str(),
        "kind": m.kind,
        "text": m.text,
        "truncated": m.truncated,
        "thinking": m.thinking,
        "timestamp": m.timestamp,
        "model": m.model,
        "tool_calls": m.tool_calls.iter().map(|t| json!({
            "name": t.name,
            "input_preview": t.input_preview,
            "input": t.input,
            "output": t.output,
            "is_error": t.is_error,
        })).collect::<Vec<_>>(),
        "images": m.images.iter().map(|i| json!({
            "media_type": i.media_type,
            "data": b64.encode(&i.bytes),
            "text_offset": i.text_offset,
        })).collect::<Vec<_>>(),
    })
}

fn agents_param(params: &HashMap<String, String>) -> Vec<AgentId> {
    params
        .get("agent")
        .and_then(|a| AgentId::from_str(a))
        .into_iter()
        .collect()
}

fn api_overview(app: &App) -> Result<Reply> {
    // 按列表同一口径数(只算顶层会话):`Store::host_counts` 连子代理也算,
    // 侧栏的数会和列表对不上
    let (roots, _) = app.store.list_sessions(&SessionFilter {
        roots_only: true,
        limit: LIST_CAP,
        ..Default::default()
    })?;
    let mut hosts: HashMap<&str, i64> = HashMap::new();
    for s in &roots {
        *hosts.entry(s.host.as_str()).or_default() += 1;
    }
    // 目录在、还没有会话的机器也列出来(刚建目录、还没同步完)
    let mut machines: Vec<Value> = machines(&app.data)
        .into_iter()
        .map(
            |(name, _)| json!({ "name": name, "sessions": hosts.get(name.as_str()).copied().unwrap_or(0) }),
        )
        .collect();
    machines.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut agents: Vec<Value> = app
        .store
        .agent_counts()?
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(id, n)| {
            let agent = AgentId::from_str(&id);
            let name = agent.map_or(id.clone(), |a| a.display_name().to_string());
            json!({
                "id": id, "name": name, "sessions": n,
                "icon": agent.map(|a| a.brand_icon(false)),
                "icon_dark": agent.map(|a| a.brand_icon(true)),
            })
        })
        .collect();
    agents.sort_by(|a, b| b["sessions"].as_i64().cmp(&a["sessions"].as_i64()));
    let projects: Vec<Value> = app
        .store
        .list_projects(false)?
        .into_iter()
        .map(|p| json!({ "path": p.path, "name": p.name, "sessions": p.session_count }))
        .collect();
    Ok(json_reply(
        200,
        json!({
            "machines": machines,
            "agents": agents,
            "projects": projects,
            "total": roots.len(),
            "starred": app.store.starred_count()?,
        }),
    ))
}

fn api_sessions(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let offset: usize = params
        .get("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let filter = SessionFilter {
        agents: agents_param(params),
        roots_only: true,
        title_query: params.get("title").filter(|t| !t.is_empty()).cloned(),
        favorite_only: params.get("starred").is_some_and(|v| v == "1"),
        project_paths: params
            .get("project")
            .filter(|p| !p.is_empty())
            .cloned()
            .into_iter()
            .collect(),
        sort: match params.get("sort").map(String::as_str) {
            Some("created") => SortKey::Created,
            Some("messages") => SortKey::Messages,
            _ => SortKey::Updated,
        },
        limit: LIST_CAP,
        ..Default::default()
    };
    let (all, _) = app.store.list_sessions(&filter)?;
    let child_counts = app.store.child_counts(&filter)?;
    let host = params.get("host").filter(|h| !h.is_empty());
    let mut matched: Vec<&SessionMeta> = all
        .iter()
        .filter(|s| host.is_none_or(|h| &s.host == h))
        .collect();
    // 清理页要的"按占用从大到小":SortKey 没有这一档,取回来再排
    // 清理页的"N 天前":只留更新时间早于这个时刻的
    if let Some(before) = params.get("before").and_then(|v| v.parse::<i64>().ok()) {
        matched.retain(|s| s.updated_at < before);
    }
    if params.get("sort").is_some_and(|v| v == "size") {
        matched.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    }
    let page: Vec<Value> = matched
        .iter()
        .skip(offset)
        .take(limit.min(500))
        .map(|s| {
            let mut v = session_json(s);
            v["children"] = json!(child_counts.get(&s.key).copied().unwrap_or(0));
            v
        })
        .collect();
    Ok(json_reply(
        200,
        json!({ "total": matched.len(), "sessions": page }),
    ))
}

fn api_search(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let q = params.get("q").map(String::as_str).unwrap_or("").trim();
    if q.is_empty() {
        return Ok(json_reply(200, json!({ "hits": [], "truncated": false })));
    }
    let (hits, truncated) = app.store.search(q, &agents_param(params), None, 200)?;
    let host = params.get("host").filter(|h| !h.is_empty());
    let hits: Vec<Value> = hits
        .iter()
        .filter(|h| host.is_none_or(|want| &h.session.host == want))
        .map(|h| {
            json!({
                "session": session_json(&h.session),
                "seq": h.seq,
                "role": h.role,
                // 命中高亮的私用区标记换成桌面版 ⌘K 面板同款的「」
                "snippet": h.snippet.replace(HL_OPEN, "「").replace(HL_CLOSE, "」"),
                "timestamp": h.timestamp,
            })
        })
        .collect();
    Ok(json_reply(
        200,
        json!({ "hits": hits, "truncated": truncated }),
    ))
}

/// key → (meta, 拥有它的实例)。roster 每轮扫描可能换代,拿一份 Arc 快照用
fn locate(app: &App, params: &HashMap<String, String>) -> Result<Option<(SessionMeta, Adapters)>> {
    let Some(key) = params.get("key") else {
        return Ok(None);
    };
    let Some(meta) = app.store.get_session(key)? else {
        return Ok(None);
    };
    Ok(Some((meta, app.adapters.read().unwrap().clone())))
}

fn api_session(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some((meta, adapters)) = locate(app, params)? else {
        return Ok(error(404, "session not found"));
    };
    let adapter = adapter_for(&adapters, meta.agent, &meta.file_path)
        .context("no adapter owns this session file")?;
    let transcript = adapter.parse_transcript(&SessionFileRef::from_meta(&meta))?;
    Ok(json_reply(
        200,
        json!({
            "session": session_json(&meta),
            "messages": transcript.mainline.iter().map(message_json).collect::<Vec<_>>(),
            "subagents": transcript.sidechains.iter().map(|s| json!({
                "id": s.id,
                "label": s.label(),
            })).collect::<Vec<_>>(),
        }),
    ))
}

fn api_export(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some((meta, adapters)) = locate(app, params)? else {
        return Ok(error(404, "session not found"));
    };
    let adapter = adapter_for(&adapters, meta.agent, &meta.file_path)
        .context("no adapter owns this session file")?;
    let markdown = wake_core::services::exporter::render_markdown(adapter, &meta)?;
    let name = wake_core::services::exporter::default_file_name(&meta, "md");
    // 文件名进响应头:只留 ASCII 的安全字符,其余换成 '_'
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(
        reply(200, "text/markdown; charset=utf-8", markdown).with_header(header(
            "Content-Disposition",
            &format!("attachment; filename=\"{safe}\""),
        )),
    )
}

/// 收藏 / 取消收藏。写的是索引库里的 user_data 表(与桌面版同一张),不碰会话文件
fn api_star(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some(key) = params.get("key") else {
        return Ok(error(400, "missing key"));
    };
    if app.store.get_session(key)?.is_none() {
        return Ok(error(404, "session not found"));
    }
    let on = params.get("on").is_some_and(|v| v == "1");
    app.store.set_user_data(key, Some(on), None)?;
    Ok(json_reply(200, json!({ "favorite": on })))
}

fn api_children(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some(key) = params.get("key") else {
        return Ok(error(400, "missing key"));
    };
    let filter = SessionFilter {
        limit: LIST_CAP,
        ..Default::default()
    };
    let children = app.store.list_children(key, &filter)?;
    Ok(json_reply(
        200,
        json!({ "sessions": children.iter().map(session_json).collect::<Vec<_>>() }),
    ))
}

fn api_pin(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some(key) = params.get("key") else {
        return Ok(error(400, "missing key"));
    };
    if app.store.get_session(key)?.is_none() {
        return Ok(error(404, "session not found"));
    }
    let on = params.get("on").is_some_and(|v| v == "1");
    app.store.set_user_data(key, None, Some(on))?;
    Ok(json_reply(200, json!({ "pinned": on })))
}

/// 网页版的"删除"是隐藏:数据目录只读挂载,会话文件一个都不动,只在索引里记墓碑
/// (连同随它走的子代理,`cleanup::delete_plan` 同桌面版一条规矩)。设置页可以取消隐藏
fn api_hide(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some((meta, adapters)) = locate(app, params)? else {
        return Ok(error(404, "session not found"));
    };
    let plan = wake_core::cleanup::delete_plan(&app.store, &adapters, &meta)?;
    let keys: Vec<String> = plan.sessions.iter().map(|s| s.key.clone()).collect();
    app.store.remove_trashed_sessions(&keys, &HashMap::new())?;
    Ok(json_reply(200, json!({ "hidden": keys.len() })))
}

fn api_hidden(app: &App) -> Result<Reply> {
    let adapters = app.adapters.read().unwrap().clone();
    let groups = wake_core::cleanup::hidden_sessions(&app.store, &adapters)?;
    let rows: Vec<Value> = groups
        .iter()
        .map(|g| {
            json!({
                "hidden_at": g.hidden_at,
                "nested": g.nested,
                "session": session_json(&g.head),
            })
        })
        .collect();
    Ok(json_reply(200, json!({ "groups": rows })))
}

fn api_unhide(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let at: i64 = params.get("at").and_then(|v| v.parse().ok()).unwrap_or(0);
    let adapters = app.adapters.read().unwrap().clone();
    let group = wake_core::cleanup::hidden_sessions(&app.store, &adapters)?
        .into_iter()
        .find(|g| g.hidden_at == at);
    match group {
        Some(group) => {
            wake_core::cleanup::unhide(&app.store, &adapters, group)?;
            Ok(json_reply(200, json!({ "ok": true })))
        }
        None => Ok(error(404, "not hidden")),
    }
}

/// 恢复命令(给用户复制):在原机器上跑的,与从别的机器 ssh 过去跑的
fn api_resume(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let Some((meta, _)) = locate(app, params)? else {
        return Ok(error(404, "session not found"));
    };
    Ok(json_reply(
        200,
        json!({
            "local": wake_core::services::terminal::local_resume_command(&meta),
            "ssh": wake_core::services::terminal::ssh_resume_command(&meta),
        }),
    ))
}

fn agent_fields(id: &str) -> (String, Option<&'static str>, Option<&'static str>) {
    match AgentId::from_str(id) {
        Some(a) => (
            a.display_name().to_string(),
            Some(a.brand_icon(false)),
            Some(a.brand_icon(true)),
        ),
        None => (id.to_string(), None, None),
    }
}

fn api_insights(app: &App) -> Result<Reply> {
    // "今天"按容器的本地时区(compose 里的 TZ);没设就是 UTC,热力图的日界会差几小时
    let d = app.store.insights(chrono::Local::now().date_naive())?;
    let tally = |t: &UsageTally, agent: bool| {
        let (label, icon, icon_dark) = if agent {
            agent_fields(&t.name)
        } else {
            (t.name.clone(), None, None)
        };
        json!({
            "name": t.name, "label": label, "icon": icon, "icon_dark": icon_dark,
            "sessions": t.sessions, "prompts": t.prompts, "tokens": t.tokens,
        })
    };
    let days = |v: &[(chrono::NaiveDate, i64)]| -> Vec<Value> {
        v.iter()
            .map(|(date, n)| json!([date.to_string(), n]))
            .collect()
    };
    Ok(json_reply(
        200,
        json!({
            "as_of": d.as_of.to_string(),
            "sessions": d.sessions,
            "prompts": d.prompts,
            "tokens": d.tokens,
            "project_count": d.project_count,
            "first_ts": d.first_ts,
            "current_streak": d.current_streak,
            "longest_streak": d.longest_streak,
            "daily": days(&d.daily),
            "daily_sessions": days(&d.daily_sessions),
            "hourly": d.hourly,
            "weekday": d.weekday,
            "monthly": d.monthly,
            "agents": d.agents.iter().map(|t| tally(t, true)).collect::<Vec<_>>(),
            "projects": d.projects.iter().map(|t| tally(t, false)).collect::<Vec<_>>(),
            "models": d.models.iter().map(|t| tally(t, false)).collect::<Vec<_>>(),
            "trend": d.trend_agents.iter().map(|t| json!({
                "name": t.name, "label": agent_fields(&t.name).0, "weekly": t.weekly,
            })).collect::<Vec<_>>(),
        }),
    ))
}

/// 机器目录里的绝对路径 → 那台机器上的写法(`~/.codex/memories/MEMORY.md`):机器目录
/// 就是那台机器 home 的镜像。不在任何机器目录下的原样给
fn home_path(app: &App, host: &str, path: &str) -> String {
    let root = app.data.join(host);
    match Path::new(path).strip_prefix(&root) {
        Ok(rest) if !host.is_empty() => format!("~/{}", rest.to_string_lossy()),
        _ => path.to_string(),
    }
}

fn memory_json(app: &App, doc: &MemoryDoc) -> Value {
    json!({
        "key": doc.key,
        "agent": doc.agent.as_str(),
        "agent_name": doc.agent.display_name(),
        "icon": doc.agent.brand_icon(false),
        "icon_dark": doc.agent.brand_icon(true),
        "host": doc.host,
        "scope": doc.scope.as_str(),
        "project_name": doc.project_name,
        "title": doc.title,
        "path": home_path(app, &doc.host, &doc.path),
        "updated_at": doc.updated_at,
        "size_bytes": doc.size_bytes,
    })
}

fn api_memories(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let filter = MemoryFilter {
        agents: agents_param(params),
        project_paths: params
            .get("project")
            .filter(|p| !p.is_empty())
            .cloned()
            .into_iter()
            .collect(),
        // 侧栏的"用户记忆"行只列用户级;项目行只算项目自己的(桌面版同口径)
        user: match (
            params.get("user").map(String::as_str),
            params.get("project"),
        ) {
            (Some("only"), _) => UserMemories::Only,
            (_, Some(p)) if !p.is_empty() => UserMemories::Excluded,
            _ => UserMemories::Alongside,
        },
        ..Default::default()
    };
    let q = params.get("q").map(String::as_str).unwrap_or("").trim();
    let docs: Vec<Value> = if q.is_empty() {
        app.store
            .list_memories(&filter)?
            .iter()
            .map(|d| memory_json(app, d))
            .collect()
    } else {
        app.store
            .search_memories(q, &filter)?
            .iter()
            .map(|h| {
                let mut v = memory_json(app, &h.doc);
                v["snippet"] = json!(h.snippet.replace(HL_OPEN, "「").replace(HL_CLOSE, "」"));
                v
            })
            .collect()
    };
    Ok(json_reply(200, json!({ "docs": docs })))
}

/// 记忆页侧栏的计数:总数、用户级、按 agent、按项目
fn api_memory_counts(app: &App) -> Result<Reply> {
    let c = app.store.memory_counts()?;
    Ok(json_reply(
        200,
        json!({
            "total": c.total,
            "user": c.user,
            "agents": c.agents.iter().map(|(a, n)| json!({
                "id": a.as_str(), "name": a.display_name(), "count": n,
                "icon": a.brand_icon(false), "icon_dark": a.brand_icon(true),
            })).collect::<Vec<_>>(),
            "projects": c.projects.iter().filter(|p| !p.path.is_empty()).map(|p| json!({
                "path": p.path, "name": p.name, "count": p.count,
            })).collect::<Vec<_>>(),
        }),
    ))
}

fn api_memory(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let key = params.get("key").map(String::as_str).unwrap_or("");
    // 路径不从请求里取:按 key 在库里找到那一份,再读它自己记的路径
    let docs = app.store.list_memories(&MemoryFilter::default())?;
    match docs.iter().find(|d| d.key == key) {
        Some(doc) => {
            let mut v = memory_json(app, doc);
            v["body"] = json!(wake_core::adapters::memory_body(doc));
            Ok(json_reply(200, v))
        }
        None => Ok(error(404, "memory not found")),
    }
}

/// 一台机器目录里的 skill:按那台机器的 home 布局扫(`~/.claude/skills`、`~/.codex/skills`、
/// `~/.agents/skills`——桌面版归档会把这三处一起镜像上来)
fn scan_skills(app: &App, dir: &Path) -> wake_core::skills::SkillScan {
    let roster: Vec<(AgentId, Vec<PathBuf>)> = [
        (AgentId::ClaudeCode, dir.join(".claude")),
        (AgentId::Codex, dir.join(".codex")),
    ]
    .into_iter()
    .filter(|(_, home)| home.is_dir())
    .map(|(agent, home)| (agent, vec![home]))
    .collect();
    wake_core::skills::scan_in(
        &roster,
        &wake_core::skills::SkillHomes::at(dir),
        &[],
        &app.skills,
    )
}

/// 归属方连同图标:共享目录没有品牌图(页面用 layers 图标),各家用自己的
fn owner_json(owner: &wake_core::skills::SkillOwner) -> Value {
    match owner {
        wake_core::skills::SkillOwner::Shared => json!({ "id": "shared", "name": "共享" }),
        wake_core::skills::SkillOwner::Agent(a) => json!({
            "id": a.as_str(), "name": a.display_name(),
            "icon": a.brand_icon(false), "icon_dark": a.brand_icon(true),
        }),
    }
}

fn scope_name(scope: &wake_core::skills::SkillScope) -> &'static str {
    match scope {
        wake_core::skills::SkillScope::User => "用户级",
        wake_core::skills::SkillScope::Plugin { .. } => "插件",
        wake_core::skills::SkillScope::Project { .. } => "项目级",
    }
}

fn api_skills(app: &App) -> Result<Reply> {
    let mut out: Vec<Value> = Vec::new();
    for (machine, dir) in machines(&app.data) {
        for skill in scan_skills(app, &dir).skills {
            out.push(json!({
                "machine": machine,
                "name": skill.name,
                "description": skill.description(),
                "updated_at": skill.updated_at(),
                "copies": skill.copies,
                "differs": skill.differs,
                "plugin": skill.plugin,
                "owners": skill.owners.iter().map(owner_json).collect::<Vec<_>>(),
                "path": home_path(app, &machine, &skill.installs[skill.newest].skill_md().to_string_lossy()),
                "installs": skill.installs.iter().map(|i| json!({
                    "owner": owner_json(&i.owner),
                    "scope": scope_name(&i.scope),
                    "path": home_path(app, &machine, &i.path.to_string_lossy()),
                    "linked": i.linked,
                })).collect::<Vec<_>>(),
            }));
        }
    }
    Ok(json_reply(200, json!({ "skills": out })))
}

fn api_skill(app: &App, params: &HashMap<String, String>) -> Result<Reply> {
    let machine = params.get("machine").map(String::as_str).unwrap_or("");
    let name = params.get("name").map(String::as_str).unwrap_or("");
    // 机器名与 skill 名都只用来在扫描结果里**查找**,读的路径是扫描给的,不拼请求里的串
    let Some((_, dir)) = machines(&app.data).into_iter().find(|(m, _)| m == machine) else {
        return Ok(error(404, "machine not found"));
    };
    let scan = scan_skills(app, &dir);
    let Some(skill) = scan.skills.iter().find(|s| s.name == name) else {
        return Ok(error(404, "skill not found"));
    };
    let body = std::fs::read_to_string(skill.installs[skill.newest].skill_md())?;
    Ok(json_reply(200, json!({ "name": skill.name, "body": body })))
}

fn api_settings(app: &App) -> Result<Reply> {
    let index_bytes: u64 = ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            std::fs::metadata(format!("{}{suffix}", app.index_path.display())).ok()
        })
        .map(|m| m.len())
        .sum();
    Ok(json_reply(
        200,
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "data": app.data.to_string_lossy(),
            "machines": machines(&app.data).iter().map(|(name, dir)| json!({
                "name": name, "path": dir.to_string_lossy(),
            })).collect::<Vec<_>>(),
            "index_path": app.index_path.to_string_lossy(),
            "index_bytes": index_bytes,
            "scan_secs": app.scan_secs,
            "password": app.password.is_some(),
        }),
    ))
}

fn asset(path: &str) -> Reply {
    let rel = path.trim_start_matches("/assets/");
    // 只认内嵌的 icons/ 与 brands/ 两个目录下的文件名,不解析任何 `..`
    let ok = (rel.starts_with("icons/") || rel.starts_with("brands/"))
        && rel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
        && !rel.contains("..");
    match ok.then(|| ASSETS.get_file(rel)).flatten() {
        Some(file) => {
            let kind = if rel.ends_with(".svg") {
                "image/svg+xml"
            } else {
                "image/png"
            };
            Response::from_data(file.contents().to_vec())
                .with_header(header("Content-Type", kind))
                .with_header(header("Cache-Control", "public, max-age=86400"))
                .with_header(header("X-Content-Type-Options", "nosniff"))
        }
        None => error(404, "not found"),
    }
}

fn api_login(app: &App, req: &mut Request) -> Reply {
    let Some(password) = &app.password else {
        return json_reply(200, json!({ "ok": true }));
    };
    let mut body = String::new();
    if req
        .as_reader()
        .take(4096)
        .read_to_string(&mut body)
        .is_err()
    {
        return error(400, "bad request");
    }
    let given = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["password"].as_str().map(str::to_string))
        .unwrap_or_default();
    if !same(&given, password) {
        // 猜密码的每次尝试都要付这半秒
        std::thread::sleep(std::time::Duration::from_millis(500));
        return error(401, "wrong password");
    }
    json_reply(200, json!({ "ok": true })).with_header(header(
        "Set-Cookie",
        &format!(
            "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000",
            app.token
        ),
    ))
}

fn route(app: &App, req: &mut Request) -> Reply {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("");
    let params = query_params(&url);
    match (req.method(), path) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            reply(200, "text/html; charset=utf-8", INDEX_HTML).with_header(header(
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
            ))
        }
        (Method::Get, "/healthz") => reply(200, "text/plain", "ok"),
        (Method::Get, p) if p.starts_with("/assets/") => asset(p),
        (Method::Post, "/api/login") => api_login(app, req),
        (Method::Post, p) if p.starts_with("/api/") => {
            if !authed(app, req) {
                return error(401, "login required");
            }
            let result = match p {
                "/api/star" => api_star(app, &params),
                "/api/pin" => api_pin(app, &params),
                "/api/hide" => api_hide(app, &params),
                "/api/unhide" => api_unhide(app, &params),
                _ => Ok(error(404, "not found")),
            };
            result.unwrap_or_else(|e| {
                eprintln!("[wake-web] {p}: {e:#}");
                error(500, &e.to_string())
            })
        }
        (Method::Get, p) if p.starts_with("/api/") => {
            if !authed(app, req) {
                return error(401, "login required");
            }
            let result = match p {
                "/api/overview" => api_overview(app),
                "/api/sessions" => api_sessions(app, &params),
                "/api/search" => api_search(app, &params),
                "/api/session" => api_session(app, &params),
                "/api/export" => api_export(app, &params),
                "/api/children" => api_children(app, &params),
                "/api/resume" => api_resume(app, &params),
                "/api/hidden" => api_hidden(app),
                "/api/insights" => api_insights(app),
                "/api/memories" => api_memories(app, &params),
                "/api/memory" => api_memory(app, &params),
                "/api/memory_counts" => api_memory_counts(app),
                "/api/skills" => api_skills(app),
                "/api/skill" => api_skill(app, &params),
                "/api/settings" => api_settings(app),
                _ => Ok(error(404, "not found")),
            };
            result.unwrap_or_else(|e| {
                eprintln!("[wake-web] {p}: {e:#}");
                error(500, &e.to_string())
            })
        }
        _ => error(404, "not found"),
    }
}

/// 后台:每隔一阵按数据根此刻的机器目录重组 roster、跑一轮增量扫描。NAS 上的同步
/// 目录不一定给得出可靠的文件事件(网络盘、容器卷),所以不挂 watcher,只轮询
fn scan_loop(app: Arc<App>, every: std::time::Duration) {
    loop {
        let adapters: Adapters = Arc::new(build_adapters(&app.data));
        *app.adapters.write().unwrap() = adapters.clone();
        let started = std::time::Instant::now();
        if let Err(e) = run_scan(&adapters, &app.store, &NullEvents, false) {
            eprintln!("[wake-web] scan failed: {e:#}");
        }
        let took = started.elapsed();
        if took.as_secs() >= 5 {
            eprintln!("[wake-web] scan took {:.1}s", took.as_secs_f64());
        }
        std::thread::sleep(every);
    }
}

fn main() -> Result<()> {
    let data = PathBuf::from(env_or("WAKE_DATA", "/data"));
    let index_dir = PathBuf::from(env_or("WAKE_INDEX", "/index"));
    let listen = env_or("WAKE_LISTEN", "0.0.0.0:8080");
    let scan_secs: u64 = env_or("WAKE_SCAN_SECS", "30").parse().unwrap_or(30).max(5);
    let password = std::env::var("WAKE_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    anyhow::ensure!(
        data.is_dir(),
        "WAKE_DATA {} is not a directory",
        data.display()
    );
    std::fs::create_dir_all(&index_dir)
        .with_context(|| format!("creating WAKE_INDEX {}", index_dir.display()))?;
    let db_path = index_dir.join("wake.db");
    let _lock = match IndexLock::try_acquire(&db_path, "web")? {
        Ownership::Ours(lock) => lock,
        Ownership::Held(holder) => {
            anyhow::bail!("index at {} is held by {holder}", db_path.display())
        }
    };
    let (store, rebuilt) = wake_core::db::open_or_rebuild(&db_path)?;
    if let Some(why) = rebuilt {
        eprintln!("[wake-web] index rebuilt: {why}");
    }

    let app = Arc::new(App {
        store: Arc::new(store),
        adapters: RwLock::new(Arc::new(Vec::new())),
        data,
        password,
        token: random_token(),
        index_path: db_path.clone(),
        scan_secs,
        skills: wake_core::skills::SkillCache::new(),
    });
    eprintln!(
        "[wake-web] data {} ({} machines), index {}, listening on http://{listen}{}",
        app.data.display(),
        machines(&app.data).len(),
        db_path.display(),
        if app.password.is_some() {
            ""
        } else {
            " (no password)"
        }
    );
    {
        let app = app.clone();
        std::thread::spawn(move || scan_loop(app, std::time::Duration::from_secs(scan_secs)));
    }

    let server =
        Arc::new(Server::http(&listen).map_err(|e| anyhow::anyhow!("listen {listen}: {e}"))?);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (server, app) = (server.clone(), app.clone());
            std::thread::spawn(move || {
                for mut req in server.incoming_requests() {
                    let response = route(&app, &mut req);
                    let _ = req.respond(response);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}
