use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Kimi Code(Moonshot AI):`~/.kimi-code/sessions/wd_<名>_<hash>/session_<uuid>/`
/// 一目录一会话,主文件 `agents/main/wire.jsonl`(事件溯源)。turn.prompt / turn.steer
/// 是用户输入;桌面端 0.4x 的助手回复在 agent.message.appended(包装层 `{message, meta}`);
/// CLI 的助手回复按步写在 context.append_loop_event 里,v1 未展开;config / turn.* 等
/// 已知跳过。`state.json` 边车给标题/时间("New Session" 是占位),cwd 靠根级
/// `session_index.jsonl` 的 sessionId→workDir 映射(目录名 hash 不可反推)。
/// `agents/<非main>/` 是子代理,不进列表。
pub struct KimiAdapter {
    root: PathBuf,
    index_path: PathBuf,
    /// sessionId → workDir,按 index mtime 缓存(全量刷新时逐会话调用)
    cwd_cache: MtimeCache<HashMap<String, String>>,
}

impl KimiAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default().join(".kimi-code");
        Self {
            root: home.join("sessions"),
            index_path: home.join("session_index.jsonl"),
            cwd_cache: MtimeCache::new(),
        }
    }

    fn cwd_map(&self) -> HashMap<String, String> {
        let mtime = fs::metadata(&self.index_path)
            .map(|m| mtime_ms(&m))
            .unwrap_or(0);
        self.cwd_cache
            .get_or_try_build(mtime, || {
                let mut out = HashMap::new();
                if let Ok(raw) = fs::read_to_string(&self.index_path) {
                    for line in raw.lines() {
                        let Ok(v) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if let (Some(id), Some(wd)) = (
                            v.get("sessionId").and_then(|x| x.as_str()),
                            v.get("workDir").and_then(|x| x.as_str()),
                        ) {
                            out.insert(id.to_string(), wd.to_string());
                        }
                    }
                }
                Some(out)
            })
            .unwrap_or_default()
    }

    fn cwd_for(&self, native_id: &str) -> String {
        self.cwd_map().get(native_id).cloned().unwrap_or_default()
    }
}

/// 主代理转录相对会话目录的位置。列表、file_ref 与会话目录反推共用这一处,按平台分隔符拼
/// (写死 `/` 的字符串比较在 Windows 上一条都对不上,PR #61)
const MAIN_WIRE: [&str; 3] = ["agents", "main", "wire.jsonl"];

fn main_wire_rel() -> PathBuf {
    MAIN_WIRE.iter().collect()
}

/// `…/session_<uuid>/agents/main/wire.jsonl` → 会话目录;子代理(`agents/<非main>/`)与别的
/// 形状给 None。read_state / native_id / session_paths 共用,漂移会 trash 错目录
fn session_dir_of(wire_path: &Path) -> Option<&Path> {
    if !wire_path.ends_with(main_wire_rel()) {
        return None;
    }
    let dir = wire_path.ancestors().nth(MAIN_WIRE.len())?;
    dir.file_name()?
        .to_string_lossy()
        .starts_with("session_")
        .then_some(dir)
}

/// state.json 边车(会话目录 session_<uuid>/ 下,wire.jsonl 上三级)
struct KimiState {
    title: String,
    created_ms: i64,
    updated_ms: i64,
}

fn read_state(wire_path: &Path) -> KimiState {
    let mut s = KimiState {
        title: String::new(),
        created_ms: 0,
        updated_ms: 0,
    };
    let Some(session_dir) = session_dir_of(wire_path) else {
        return s;
    };
    if let Ok(raw) = fs::read_to_string(session_dir.join("state.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
                // "New Session" 是 Kimi 的占位标题,不当真实标题
                if t != "New Session" {
                    s.title = t.to_string();
                }
            }
            if let Some(t) = v.get("createdAt").and_then(|x| x.as_str()) {
                s.created_ms = iso_ms(t);
            }
            if let Some(t) = v.get("updatedAt").and_then(|x| x.as_str()) {
                s.updated_ms = iso_ms(t);
            }
        }
    }
    s
}

/// 思考块:content 里的 `{"type":"think","think":"..."}`,多段拼成一段(截断在合并处统一做)
fn think_texts(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("think"))
        .filter_map(|p| p.get("think").and_then(Value::as_str).map(str::trim))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parse_kimi_wire(path: &Path, decode_images: bool) -> Result<(Vec<TranscriptMessage>, u32)> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut unknown = 0u32;
    // 这一轮的助手回复还在接着并:新的一轮(turn.* 里除了 turn.step.*)关上它
    let mut reply_open = false;

    for line in reader.lines() {
        let Ok(line) = line else {
            unknown += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            unknown += 1;
            continue;
        };
        // 事件时间:Kimi 给每条记录打 time(毫秒;metadata 行没有),没有就不标
        let ts = row.get("time").map(to_epoch_ms).unwrap_or(0);
        match row.get("type").and_then(|v| v.as_str()) {
            Some("turn.prompt") | Some("turn.steer") => {
                reply_open = false;
                let parsed = content_parts(row.get("input").unwrap_or(&Value::Null), decode_images);
                if !parsed.text.is_empty() || !parsed.images.is_empty() {
                    let mut message = text_msg(Role::User, &parsed.text, ts);
                    message.images = parsed.images;
                    messages.push(message);
                }
            }
            Some("context.append_message") => {
                let Some(msg) = row.get("message") else {
                    unknown += 1;
                    continue;
                };
                let role = match msg.get("role").and_then(|v| v.as_str()) {
                    Some("assistant") => Role::Assistant,
                    // user 输入已由 turn.prompt 覆盖;tool/system 等上下文行跳过
                    _ => continue,
                };
                let parsed =
                    content_parts(msg.get("content").unwrap_or(&Value::Null), decode_images);
                if !parsed.text.is_empty() || !parsed.images.is_empty() {
                    let mut message = text_msg(role, &parsed.text, ts);
                    message.images = parsed.images;
                    messages.push(message);
                }
            }
            // 桌面端(0.4x):助手回复在 agent.message.appended,包装层 `{message, meta}`
            Some("agent.message.appended") => {
                let Some(msg) = row
                    .get("message")
                    .and_then(|wrapper| wrapper.get("message"))
                else {
                    unknown += 1;
                    continue;
                };
                match msg.get("role").and_then(Value::as_str) {
                    Some("assistant") => {}
                    // user 与 turn.prompt / turn.steer 重复;tool 的明细在 loop_event 里,v1 不展开;
                    // notify 是界面通知。别的角色计未知行:格式一漂移回复就会悄悄消失,清理页
                    // 靠未知行判断这个会话读全了没有
                    Some("user" | "tool" | "notify") => continue,
                    _ => {
                        unknown += 1;
                        continue;
                    }
                }
                let content = msg.get("content").unwrap_or(&Value::Null);
                let parsed = content_parts(content, decode_images);
                let thinking = think_texts(content);
                if parsed.text.is_empty() && parsed.images.is_empty() && thinking.is_empty() {
                    continue;
                }
                // 一轮里每次模型调用一条事件(多半只有 think),并成一条回复;新的一轮另起一条——
                // 输入渲染不出来的那一轮(只带音视频附件)没有用户消息隔开,不能并进上一轮
                if !reply_open {
                    messages.push(text_msg(Role::Assistant, "", ts));
                    reply_open = true;
                }
                merge_into_last_assistant(&mut messages, ts, parsed, &thinking);
            }
            // 已知的配置/生命周期/工具事件行(工具明细在 loop_event 里,v1 不展开)
            Some("metadata")
            | Some("config.update")
            | Some("tools.set_active_tools")
            | Some("context.append_loop_event") => {}
            Some(t) if t.starts_with("turn.") => reply_open &= t.starts_with("turn.step."),
            _ => {
                unknown += 1;
            }
        }
    }
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

fn build_meta(
    r: &SessionFileRef,
    state: &KimiState,
    cwd: &str,
    messages: &[TranscriptMessage],
) -> SessionMeta {
    let title = Some(clean_title_candidate(&state.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    // Kimi 在 Windows 上把 workDir 记成 `C:/…`,别家记 `C:\…`:换成同一种写法才归到同一个项目
    let project = canonical_project_path(cwd);
    SessionMeta {
        key: format!("kimi:{}", r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Kimi,
        title,
        project_name: project_name_of(&project),
        project_path: project,
        file_path: r.file_path.clone(),
        created_at: if state.created_ms > 0 {
            state.created_ms
        } else {
            r.mtime_ms
        },
        updated_at: if state.updated_ms > 0 {
            state.updated_ms
        } else {
            r.mtime_ms
        },
        message_count: messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: None,
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

/// 会话目录名即 native_id(与 session_index.jsonl 的 sessionId 同形,
/// resume 直接可用)
fn native_id_of(wire_path: &Path) -> Option<String> {
    Some(
        session_dir_of(wire_path)?
            .file_name()?
            .to_string_lossy()
            .to_string(),
    )
}

impl AgentAdapter for KimiAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Kimi
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(wds) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        // 主文件判定(session_ 前缀、存在、非空、native_id)统一走 file_ref
        let rel = main_wire_rel();
        for wd in wds.flatten() {
            let Ok(sessions) = fs::read_dir(wd.path()) else {
                continue;
            };
            for sess in sessions.flatten() {
                let wire = sess.path().join(&rel);
                if let Some(r) = self.file_ref(&wire) {
                    refs.push(r);
                }
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认主代理的 wire.jsonl(形状判定在 session_dir_of;agents/<其他>/ 是子代理)
        let native = native_id_of(path)?;
        let mut r = default_file_ref(self.agent(), path)?;
        r.native_id = native;
        Some(r)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 会话是 session_<uuid>/ 整个目录(state.json + agents/),整目录进废纸篓
        session_dir_of(Path::new(&meta.file_path))
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (messages, unknown) = parse_kimi_wire(Path::new(&r.file_path), false)?;
        let state = read_state(Path::new(&r.file_path));
        let meta = build_meta(r, &state, &self.cwd_for(&r.native_id), &messages);
        Ok(ParsedSession::derive(meta, &messages, unknown))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (messages, unknown) = parse_kimi_wire(Path::new(&r.file_path), true)?;
        let state = read_state(Path::new(&r.file_path));
        Ok(ParsedTranscript {
            meta: build_meta(r, &state, &self.cwd_for(&r.native_id), &messages),
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: unknown,
        })
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // `~/.kimi-code` 形态(含 sessions/)则 index 在其顶层;直接选中
        // sessions 则上一层找。index 相对 dir 派生,落回默认家会拿错 cwd 映射
        let (root, index_path) = if dir.join("sessions").is_dir() {
            (dir.join("sessions"), dir.join("session_index.jsonl"))
        } else {
            let idx = dir
                .parent()
                .map(|p| p.join("session_index.jsonl"))
                .unwrap_or_else(|| dir.join("session_index.jsonl"));
            (dir, idx)
        };
        Box::new(Self {
            root,
            index_path,
            cwd_cache: MtimeCache::new(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
