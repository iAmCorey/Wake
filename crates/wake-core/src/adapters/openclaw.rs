use super::parse_utils::*;
use super::pi_format::{PiRender, PiRenderOptions};
use super::sqlite_ro::{open_sqlite_ro, strip_virtual_path, virtual_path};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// OpenClaw(原 Clawdbot/Moltbot):状态目录 `~/.openclaw`(`OPENCLAW_STATE_DIR`
/// 可改道,老装机名 `~/.clawdbot`),会话按 agent 分桶在 `agents/<agentId>/` 下,
/// **两代存储并存、同扫**:
/// - 现版(2026.8+):`agent/openclaw-agent.sqlite`,session_windows(每个
///   session_id 一行,reset/rollover 后旧窗口保留)+ session_nodes(按 session_key
///   的活跃指针与 label)+ transcript_events(session_id, seq, event_json)——
///   event_json 就是 pi 系 SessionManager 的一行(首行 `{type:"session",cwd}`
///   header,其余带 id/parentId 的树形条目),`session_transcript_active_events`
///   按 active_position 给出当前可见分支,缺表/缺行时退回"最后一条为叶"回溯。
/// - 旧版:`sessions/<sessionId>.jsonl` 同格式明文,`sessions.json` 是按
///   session_key 的索引(label/model/totalTokens/spawnedBy)。
///
/// 同一会话被 `doctor --fix` 迁进库后旧 jsonl 仍在,同 native_id 双副本由
/// scanner 按 mtime 裁决(不变量 8⑦),这里不做二选一。
/// 消息形状即 pi-ai,渲染核心与 pi.rs 共用(pi_format);OpenClaw 独有的是
/// thinking 块、`runtimeContextCarrier` 归 Meta、compaction/branch_summary/
/// custom_message/session_info 四种条目。spawned_by 非空 = 子代理,不列。
pub struct OpenclawAdapter {
    /// `<state dir>/agents`
    root: PathBuf,
    /// 每库一份窗口行缓存(键 = 库路径),按库(含 -wal)mtime 失效
    rows_cache: Mutex<HashMap<PathBuf, Arc<MtimeCache<Vec<ClawRow>>>>>,
    /// 旧版 sessions.json 索引,每个 sessions 目录一份,按文件 mtime 失效
    index_cache: Mutex<HashMap<PathBuf, Arc<MtimeCache<LegacyIndex>>>>,
}

/// sessions.json 解析结果:sessionId → 条目
type LegacyIndex = Arc<HashMap<String, Value>>;

const DB_NAME: &str = "openclaw-agent.sqlite";
/// 转录事件 JSON 的已知非内容类型:静默跳过,表外计 unknown(格式漂移金丝雀)
const KNOWN_SKIP: &[&str] = &[
    "session",
    "thinking_level_change",
    "label",
    "custom",
    "reset",
];

impl OpenclawAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default();
        // OPENCLAW_STATE_DIR 只是候选:里面真有 agents/ 才采信;默认 ~/.openclaw,
        // 只装过老版本的机器(无 .openclaw、有 .clawdbot)回落旧名——与上游
        // resolveStateDir 的判序一致
        let state = super::env_dir("OPENCLAW_STATE_DIR")
            .filter(|d| d.join("agents").is_dir())
            .unwrap_or_else(|| {
                let new = home.join(".openclaw");
                let legacy = home.join(".clawdbot");
                if !new.exists() && legacy.exists() {
                    legacy
                } else {
                    new
                }
            });
        Self::with_root(state.join("agents"))
    }

    fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            rows_cache: Mutex::new(HashMap::new()),
            index_cache: Mutex::new(HashMap::new()),
        }
    }

    /// `agents/<agentId>/` 目录(构造时不要求存在,枚举时缺根降级为空)
    fn agent_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.path())
            .collect();
        dirs.sort();
        dirs
    }

    fn rows(&self, db: &Path) -> Option<Vec<ClawRow>> {
        let cache = Arc::clone(
            self.rows_cache
                .lock()
                .unwrap()
                .entry(db.to_path_buf())
                .or_insert_with(|| Arc::new(MtimeCache::new())),
        );
        let stamp = super::sqlite_ro::db_cache_stamp(db);
        cache.get_or_try_build(stamp, || {
            let ro = open_sqlite_ro(db, "openclaw")?;
            query_rows(&ro.conn)
        })
    }

    /// 旧版 `sessions.json`:按 sessionId 的索引(原文件按 session_key 键)
    fn legacy_index(&self, sessions_dir: &Path) -> LegacyIndex {
        let file = sessions_dir.join("sessions.json");
        let cache = Arc::clone(
            self.index_cache
                .lock()
                .unwrap()
                .entry(file.clone())
                .or_insert_with(|| Arc::new(MtimeCache::new())),
        );
        let stamp = fs::metadata(&file).map(|m| mtime_ms(&m)).unwrap_or(0);
        cache
            .get_or_try_build(stamp, || {
                let text = fs::read_to_string(&file).ok()?;
                let Value::Object(map) = serde_json::from_str::<Value>(&text).ok()? else {
                    return None;
                };
                Some(Arc::new(
                    map.into_values()
                        .filter_map(|v| Some((v.get("sessionId")?.as_str()?.to_string(), v)))
                        .collect(),
                ))
            })
            .unwrap_or_default()
    }

    fn legacy_hint(&self, path: &Path, native_id: &str) -> RowHint {
        let Some(dir) = path.parent() else {
            return RowHint::default();
        };
        let index = self.legacy_index(dir);
        // topic 转录 `<sessionId>-topic-<id>.jsonl` 共用父会话的索引项
        let base = native_id.split("-topic-").next().unwrap_or(native_id);
        let Some(v) = index.get(native_id).or_else(|| index.get(base)) else {
            return RowHint::default();
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        RowHint {
            label: s("label").or_else(|| s("displayName")),
            model: s("model"),
            tokens: v.get("totalTokens").and_then(Value::as_i64),
            channel: None,
            created_ms: 0,
            updated_ms: norm_ms(v.get("updatedAt").and_then(Value::as_i64).unwrap_or(0)),
        }
    }

    fn build_meta(r: &SessionFileRef, p: &ClawParse, hint: &RowHint) -> SessionMeta {
        let title = p
            .name
            .clone()
            .or_else(|| hint.label.clone())
            .map(|t| clean_title_candidate(&t))
            .filter(|t| !t.is_empty())
            .or_else(|| title_from_messages(&p.messages))
            .unwrap_or_else(|| UNTITLED.to_string());
        SessionMeta {
            key: format!("openclaw:{}", r.native_id),
            host: String::new(),
            id: r.native_id.clone(),
            agent: AgentId::Openclaw,
            title,
            project_path: p.cwd.clone(),
            project_name: project_name_of(&p.cwd),
            file_path: r.file_path.clone(),
            created_at: [p.created_at, hint.created_ms, r.mtime_ms]
                .into_iter()
                .find(|t| *t > 0)
                .unwrap_or(0),
            updated_at: [p.last_ts, hint.updated_ms, r.mtime_ms]
                .into_iter()
                .find(|t| *t > 0)
                .unwrap_or(0),
            message_count: p
                .messages
                .iter()
                .filter(|m| m.kind == MessageKind::Text)
                .count() as i64,
            size_bytes: r.size,
            git_branch: None,
            model: p
                .model
                .clone()
                .or_else(|| hint.model.clone())
                .filter(|m| !m.is_empty()),
            tokens_used: p.tokens_used.or(hint.tokens).filter(|t| *t > 0),
            archived: false,
            // 频道即启动面(telegram/discord/webchat…);cli/空不打徽章
            source: hint.channel.clone().filter(|c| !c.is_empty() && c != "cli"),
            favorite: false,
            pinned: false,
        }
    }

    fn parse(&self, r: &SessionFileRef, decode_images: bool) -> Result<(SessionMeta, ClawParse)> {
        let path = Path::new(&r.file_path);
        if path.is_file() {
            // 旧版 jsonl:整文件即条目流,叶 = 最后一条
            let (entries, bad) = read_jsonl_entries(path)?;
            let mut p = render(linearize(entries, None), decode_images);
            p.unknown_lines += bad;
            let hint = self.legacy_hint(path, &r.native_id);
            let meta = Self::build_meta(r, &p, &hint);
            return Ok((meta, p));
        }
        let db = Path::new(strip_virtual_path(&r.file_path));
        let row = self
            .rows(db)
            .and_then(|rows| rows.into_iter().find(|row| row.session_id == r.native_id))
            .ok_or_else(|| anyhow!("openclaw session {} not in store", r.native_id))?;
        let ro = open_sqlite_ro(db, "openclaw")
            .ok_or_else(|| anyhow!("cannot open openclaw store {}", db.display()))?;
        let mut stmt = ro.conn.prepare(
            "SELECT seq, event_json FROM transcript_events WHERE session_id = ?1 ORDER BY seq",
        )?;
        let mut entries: Vec<(i64, Value)> = Vec::new();
        let mut unknown = 0u32;
        for row in stmt.query_map([&r.native_id], |x| {
            Ok((x.get::<_, i64>(0)?, x.get::<_, String>(1)?))
        })? {
            match row
                .ok()
                .and_then(|(seq, json)| serde_json::from_str::<Value>(&json).ok().map(|v| (seq, v)))
            {
                Some(entry) => entries.push(entry),
                None => unknown += 1,
            }
        }
        // 当前可见分支(按 active_position);表缺失/为空时退回树回溯
        let active: Vec<i64> = ro
            .conn
            .prepare(
                "SELECT event_seq FROM session_transcript_active_events
                 WHERE session_id = ?1 ORDER BY active_position",
            )
            .and_then(|mut s| {
                s.query_map([&r.native_id], |x| x.get::<_, i64>(0))
                    .map(|it| it.flatten().collect::<Vec<_>>())
            })
            .unwrap_or_default();
        let active_ix = (!active.is_empty()).then(|| {
            let by_seq: HashMap<i64, usize> = entries
                .iter()
                .enumerate()
                .map(|(i, (seq, _))| (*seq, i))
                .collect();
            active
                .iter()
                .filter_map(|s| by_seq.get(s).copied())
                .collect()
        });
        let values: Vec<Value> = entries.into_iter().map(|(_, v)| v).collect();
        let mut p = render(linearize(values, active_ix), decode_images);
        p.unknown_lines += unknown;
        let meta = Self::build_meta(r, &p, &row.hint);
        Ok((meta, p))
    }
}

/// 库内一条会话窗口(session_windows ⋈ session_nodes)
#[derive(Clone)]
struct ClawRow {
    session_id: String,
    spawned_by: Option<String>,
    event_count: i64,
    content_len: i64,
    hint: RowHint,
}

/// 库行 / 旧版索引给解析结果补缺的元数据
#[derive(Clone, Default)]
struct RowHint {
    label: Option<String>,
    model: Option<String>,
    tokens: Option<i64>,
    channel: Option<String>,
    created_ms: i64,
    updated_ms: i64,
}

/// 上游全部 ms 整数;防御性兼容秒,阈值与 parse_utils::to_epoch_ms 同一条线
fn norm_ms(v: i64) -> i64 {
    to_epoch_ms(&Value::from(v))
}

fn query_rows(conn: &rusqlite::Connection) -> Option<Vec<ClawRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT w.session_id, w.spawned_by, w.created_at, w.updated_at,
                    w.transcript_updated_at, w.started_at, w.model, w.channel, w.display_name,
                    n.label, n.display_name, n.entry_json,
                    COUNT(e.seq), COALESCE(SUM(LENGTH(e.event_json)), 0),
                    COALESCE(MAX(e.created_at), 0)
             FROM session_windows w
             LEFT JOIN session_nodes n ON n.session_key = w.session_key
             LEFT JOIN transcript_events e ON e.session_id = w.session_id
             GROUP BY w.session_id",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            let session_id: String = r.get(0)?;
            let created = norm_ms(r.get::<_, Option<i64>>(2)?.unwrap_or(0));
            let started = norm_ms(r.get::<_, Option<i64>>(5)?.unwrap_or(0));
            let updated = norm_ms(r.get::<_, Option<i64>>(3)?.unwrap_or(0));
            let transcript_updated = norm_ms(r.get::<_, Option<i64>>(4)?.unwrap_or(0));
            let last_event = norm_ms(r.get::<_, Option<i64>>(14)?.unwrap_or(0));
            let entry: Value = r
                .get::<_, Option<String>>(11)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            let entry_str = |k: &str| entry.get(k).and_then(Value::as_str).map(String::from);
            let label = r
                .get::<_, Option<String>>(9)?
                .or(r.get::<_, Option<String>>(10)?)
                .or(r.get::<_, Option<String>>(8)?)
                .or_else(|| entry_str("label"))
                .or_else(|| entry_str("displayName"))
                .filter(|s| !s.trim().is_empty());
            let model = r
                .get::<_, Option<String>>(6)?
                .or_else(|| entry_str("model"));
            // 只有当前活跃窗口的 entry_json 归它;老窗口的 node 指向新窗口,
            // 计数不能张冠李戴
            let is_current = entry.get("sessionId").and_then(Value::as_str) == Some(&session_id);
            let tokens = is_current
                .then(|| entry.get("totalTokens").and_then(Value::as_i64))
                .flatten();
            Ok(ClawRow {
                spawned_by: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                event_count: r.get(12)?,
                content_len: r.get(13)?,
                hint: RowHint {
                    label,
                    model,
                    tokens,
                    channel: r.get(7)?,
                    created_ms: [started, created].into_iter().find(|t| *t > 0).unwrap_or(0),
                    updated_ms: [transcript_updated, last_event, updated]
                        .into_iter()
                        .max()
                        .unwrap_or(0),
                },
                session_id,
            })
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok();
    rows
}

/// 旧版转录文件名过滤:归档/回滚快照/压缩检查点都不是活会话
fn is_legacy_transcript(name: &str) -> bool {
    name.ends_with(".jsonl")
        && !name.starts_with('.')
        && !name.contains(".deleted.")
        && !name.contains(".reset.")
        && !name.contains(".checkpoint.")
}

/// (条目, 坏行数)
fn read_jsonl_entries(path: &Path) -> Result<(Vec<Value>, u32)> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut out = Vec::new();
    let mut bad = 0u32;
    for line in reader.lines() {
        let Ok(line) = line else {
            bad += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => out.push(v),
            Err(_) => bad += 1,
        }
    }
    Ok((out, bad))
}

/// 树形条目 → 当前分支的线性序列(header 在首位)。`active` 给出(下标)显式
/// 分支时照单;否则以最后一条带 id 的条目为叶沿 parentId 回溯到根(pi
/// SessionManager 的默认叶语义,旧版 jsonl 只有这一种)。
fn linearize(mut entries: Vec<Value>, active: Option<Vec<usize>>) -> Vec<Value> {
    let header_ix = entries
        .iter()
        .position(|v| v.get("type").and_then(Value::as_str) == Some("session"));
    let chain: Vec<usize> = match active {
        Some(ix) => ix,
        None => {
            let by_id: HashMap<&str, usize> = entries
                .iter()
                .enumerate()
                .filter_map(|(i, v)| v.get("id").and_then(Value::as_str).map(|id| (id, i)))
                .collect();
            let mut chain = Vec::new();
            let mut seen = HashSet::new();
            let mut cursor = entries
                .iter()
                .rposition(|v| v.get("id").and_then(Value::as_str).is_some());
            while let Some(i) = cursor {
                if !seen.insert(i) {
                    break;
                }
                chain.push(i);
                cursor = entries[i]
                    .get("parentId")
                    .and_then(Value::as_str)
                    .and_then(|p| by_id.get(p).copied());
            }
            chain.reverse();
            chain
        }
    };
    let mut out = Vec::with_capacity(chain.len() + 1);
    if let Some(h) = header_ix {
        out.push(std::mem::take(&mut entries[h]));
    }
    for i in chain {
        if Some(i) != header_ix {
            out.push(std::mem::take(&mut entries[i]));
        }
    }
    out
}

struct ClawParse {
    cwd: String,
    created_at: i64,
    last_ts: i64,
    messages: Vec<TranscriptMessage>,
    model: Option<String>,
    tokens_used: Option<i64>,
    /// session_info 的 name(用户/自动命名),标题首选
    name: Option<String>,
    unknown_lines: u32,
}

fn entry_ts(v: &Value, msg: Option<&Value>) -> i64 {
    v.get("timestamp")
        .and_then(Value::as_str)
        .map(iso_ms)
        .filter(|t| *t > 0)
        .or_else(|| {
            msg.and_then(|m| m.get("timestamp"))
                .map(to_epoch_ms)
                .filter(|t| *t > 0)
        })
        .unwrap_or(0)
}

fn render(chain: Vec<Value>, decode_images: bool) -> ClawParse {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let mut p = ClawParse {
        cwd: String::new(),
        created_at: 0,
        last_ts: 0,
        messages: Vec::new(),
        model: None,
        tokens_used: None,
        name: None,
        unknown_lines: 0,
    };
    let mut render = PiRender::new(
        PiRenderOptions {
            thinking_blocks: true,
            meta_carrier: true,
            raw_tool_results: true,
        },
        decode_images,
    );
    // 非 message 条目(compaction 摘要等)要按位置插在消息流里,故由 render
    // 接管 messages,这里只借它推进
    for v in &chain {
        let Some(typ) = v.get("type").and_then(Value::as_str) else {
            p.unknown_lines += 1;
            continue;
        };
        match typ {
            "session" => {
                if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                    p.cwd = c.to_string();
                }
                if let Some(t) = v.get("timestamp").and_then(Value::as_str) {
                    p.created_at = iso_ms(t);
                }
            }
            "message" => {
                let Some(msg) = v.get("message") else {
                    p.unknown_lines += 1;
                    continue;
                };
                let ts = entry_ts(v, Some(msg));
                p.last_ts = p.last_ts.max(ts);
                if !render.push(msg, ts) {
                    p.unknown_lines += 1;
                }
            }
            "model_change" => {
                if let Some(m) = v.get("modelId").and_then(Value::as_str) {
                    render.model = Some(m.to_string());
                }
            }
            "compaction" | "branch_summary" => {
                let ts = entry_ts(v, None);
                p.last_ts = p.last_ts.max(ts);
                if let Some(s) = v.get("summary").and_then(Value::as_str) {
                    if !s.trim().is_empty() {
                        let mut m = text_msg(Role::System, s, ts);
                        m.kind = MessageKind::CompactSummary;
                        render.messages.push(m);
                    }
                }
            }
            "custom_message" => {
                // 扩展注入进上下文的消息:display=true 才是给人看的,折叠为 Meta
                if v.get("display").and_then(Value::as_bool) == Some(true) {
                    let parsed = content_parts(v.get("content").unwrap_or(&Value::Null), false);
                    if !parsed.text.is_empty() {
                        let mut m = text_msg(Role::System, &parsed.text, entry_ts(v, None));
                        m.kind = MessageKind::Meta;
                        render.messages.push(m);
                    }
                }
            }
            "session_info" => {
                if let Some(n) = v.get("name").and_then(Value::as_str) {
                    let n = n.trim();
                    p.name = (!n.is_empty()).then(|| n.to_string());
                }
            }
            other if KNOWN_SKIP.contains(&other) => {}
            _ => p.unknown_lines += 1,
        }
    }
    p.messages = render.messages;
    p.model = render.model;
    p.tokens_used = render.tokens_used;
    assign_seq(&mut p.messages);
    p
}

impl AgentAdapter for OpenclawAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Openclaw
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut out = Vec::new();
        for dir in self.agent_dirs() {
            let db = dir.join("agent").join(DB_NAME);
            if db.is_file() {
                if let Some(rows) = self.rows(&db) {
                    out.extend(
                        rows.into_iter()
                            .filter(|row| row.spawned_by.is_none() && row.event_count > 0)
                            .map(|row| SessionFileRef {
                                agent: AgentId::Openclaw,
                                native_id: row.session_id.clone(),
                                file_path: virtual_path(&db, &row.session_id),
                                mtime_ms: row.hint.updated_ms,
                                size: row.content_len,
                            }),
                    );
                }
            }
            let sessions = dir.join("sessions");
            let Ok(entries) = fs::read_dir(&sessions) else {
                continue;
            };
            let index = self.legacy_index(&sessions);
            let spawned: HashSet<&str> = index
                .iter()
                .filter(|(_, v)| {
                    v.get("spawnedBy")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                })
                .map(|(id, _)| id.as_str())
                .collect();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !is_legacy_transcript(&name) {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() || meta.len() == 0 {
                    continue;
                }
                let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
                let base = stem.split("-topic-").next().unwrap_or(stem);
                if spawned.contains(stem) || spawned.contains(base) {
                    continue;
                }
                out.push(SessionFileRef {
                    agent: AgentId::Openclaw,
                    native_id: stem.to_string(),
                    file_path: e.path().to_string_lossy().to_string(),
                    mtime_ms: mtime_ms(&meta),
                    size: meta.len() as i64,
                });
            }
        }
        Ok(out)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认 agents/<id>/sessions/<name>.jsonl 这一层;归档/检查点同 list 过滤
        let name = path.file_name()?.to_string_lossy().to_string();
        if !is_legacy_transcript(&name) {
            return None;
        }
        let sessions = path.parent()?;
        if sessions.file_name()?.to_str()? != "sessions" {
            return None;
        }
        if sessions.parent()?.parent()? != self.root {
            return None;
        }
        default_file_ref(AgentId::Openclaw, path)
    }

    // quick_meta 不覆写:库行给不出标题的可靠来源(名字在 session_info 事件里),
    // 走全解析

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, p) = self.parse(r, false)?;
        let units = units_from_messages(&p.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: p.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, p) = self.parse(r, true)?;
        Ok(ParsedTranscript {
            meta,
            mainline: p.messages,
            sidechains: Vec::new(),
            unknown_line_count: p.unknown_lines,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中状态目录(~/.openclaw)或 agents 目录本身都认;目录尚不存在时按
        // 名字判:叫 agents 即是根,否则视作状态目录(远程挂载点 `.openclaw/agents`
        // 同步前就落在正确形态)
        let is_agents = dir.file_name().and_then(|n| n.to_str()) == Some("agents")
            || (!dir.join("agents").is_dir() && looks_like_agents(&dir));
        let root = if is_agents { dir } else { dir.join("agents") };
        Box::new(Self::with_root(root))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// 目录里已有 `<x>/sessions` 或 `<x>/agent/openclaw-agent.sqlite` 形态的子目录
/// → 它本身就是 agents 根(用户把 agents 目录改名拷出来的情形)
fn looks_like_agents(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|it| {
            it.flatten().any(|e| {
                let p = e.path();
                p.join("sessions").is_dir() || p.join("agent").join(DB_NAME).is_file()
            })
        })
        .unwrap_or(false)
}
