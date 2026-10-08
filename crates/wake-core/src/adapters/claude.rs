use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// Claude JSONL 已知的非消息行类型,静默跳过(不计 unknown)
const KNOWN_SKIP_TYPES: &[&str] = &[
    "queue-operation",
    "mode",
    "last-prompt",
    "permission-mode",
    "file-history-snapshot",
    "file-history-delta",
    "pr-link",
    "frame-link",
    "attachment",
    "summary",
    "atis-latch",
    // Bridge connection bookkeeping, not transcript content.
    "bridge-session",
];

pub struct ClaudeAdapter {
    root: PathBuf,
    /// `~/.claude`(全局 CLAUDE.md 的所在);直接选中 projects 目录的自定义根 / 远程
    /// 镜像没有 home——不越界摸父目录(不变量 8⑨)
    home: Option<PathBuf>,
    /// 默认根(本机自己的 ~/.claude):项目目录名才能在本机文件系统上反推——目录名
    /// 指的是写下会话的那台机器上的路径,自定义根(别处拷来的、远程镜像)反推不得
    local: bool,
    /// auto-memory 文档,按目录指纹缓存(每轮扫描都会列,指纹没变不重读)
    memories: super::MemoryCache,
    /// 目录名 → 反推出的项目路径,进程内记住(含反推失败):反推要从 `/` 逐层
    /// read_dir + stat,答案又几乎不变,每轮扫描重走一遍是白费;目录后来才出现的
    /// 极少数情况重启即可
    decoded: std::sync::Mutex<HashMap<String, Option<String>>>,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default().join(".claude");
        Self {
            root: home.join("projects"),
            home: Some(home),
            local: true,
            memories: super::MemoryCache::new(),
            decoded: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// auto-memory 树 `projects/<dir>/memory/*.md` 展开成读取单元:每个项目目录的记忆
    /// 挂到该目录最新的会话上(见 newest_session_key);目录里没有会话(Claude 按
    /// cleanupPeriodDays 清过)就从目录名在本机磁盘上反推项目路径(decode_project_dir,
    /// 只对默认根),反推不出才落 Unknown project。projects/ 不在 = 没有记忆;列不出来
    ///(权限、I/O、fd 耗尽)是"不知道",报 Err 让 scanner 冻结这一来源——折成空会把
    /// 库里 Claude 的记忆整组删光(2026-09-21 review)
    fn tree_units(&self, source: &MemorySource) -> Result<Vec<super::MemoryUnit>> {
        super::project_tree_units(source, |project| {
            let session_key = newest_session_key(&project.path()).unwrap_or_default();
            let project_path = if session_key.is_empty() && self.local {
                self.project_path_of_dir(&project.file_name().to_string_lossy())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (session_key, project_path)
        })
    }

    /// `decode_project_dir` 的记忆版
    fn project_path_of_dir(&self, name: &str) -> Option<String> {
        let mut cache = self.decoded.lock().unwrap();
        cache
            .entry(name.to_string())
            .or_insert_with(|| decode_project_dir(name))
            .clone()
    }
}

/// projects/<目录名> → 项目路径:Claude 把 cwd 里所有非字母数字字符都替成 `-`
/// (`/Users/x/.claude/worktrees/a-b` → `-Users-x--claude-worktrees-a-b`),反推只能
/// 靠磁盘:从 `/` 起逐层列目录,取编码后与剩余串前缀相符的子目录(同一层多个候选
/// 先试最长的),整串吃完即命中。只在没有会话可当锚点的目录上用(每轮几个目录、
/// 每层一次 read_dir);目录已不在磁盘上就认不出,落 Unknown project
fn decode_project_dir(name: &str) -> Option<String> {
    fn encode(raw: &str) -> String {
        raw.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }
    fn walk(dir: PathBuf, rest: &str) -> Option<PathBuf> {
        if rest.is_empty() {
            return Some(dir);
        }
        let mut candidates: Vec<(String, PathBuf)> = fs::read_dir(&dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let encoded = encode(&e.file_name().to_string_lossy());
                let matches = !encoded.is_empty()
                    && rest.starts_with(&encoded)
                    && matches!(rest.as_bytes().get(encoded.len()), None | Some(b'-'));
                matches.then(|| (encoded, e.path()))
            })
            .collect();
        candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        for (encoded, path) in candidates {
            let next = &rest[encoded.len()..];
            let next = next.strip_prefix('-').unwrap_or(next);
            if let Some(hit) = walk(path, next) {
                return Some(hit);
            }
        }
        None
    }
    // 只认 POSIX 绝对路径(首字符 `/` → `-`);Windows 的 `C--Users-…` 形态不做
    let rest = name.strip_prefix('-')?;
    if rest.is_empty() {
        return None;
    }
    walk(PathBuf::from("/"), rest).map(|p| p.to_string_lossy().to_string())
}

struct ParseResult {
    messages: Vec<TranscriptMessage>,
    title: String,
    cwd: String,
    git_branch: Option<String>,
    model: Option<String>,
    tokens_used: i64,
    created_at: i64,
    updated_at: i64,
    unknown_lines: u32,
}

#[derive(Default)]
struct PendingAssistant {
    msg_id: Option<String>,
    content: ParsedContent,
    thinking: Vec<String>,
    tool_calls: Vec<ToolCallView>,
    timestamp: Option<i64>,
    model: Option<String>,
}

fn flush_assistant(
    pending: &mut Option<PendingAssistant>,
    messages: &mut Vec<TranscriptMessage>,
    tool_index: &mut HashMap<String, (usize, usize)>,
) {
    let Some(p) = pending.take() else { return };
    let ParsedContent { text, images } = p.content;
    if text.is_empty() && p.thinking.is_empty() && p.tool_calls.is_empty() && images.is_empty() {
        return;
    }
    let (clipped, truncated) = clip(&text, MAX_MSG_TEXT);
    let thinking = if p.thinking.is_empty() {
        None
    } else {
        Some(clip(&p.thinking.join("\n\n"), MAX_TOOL_IO).0)
    };
    // tool_result 出现在后续 user 行,此刻登记 tool_use id → 消息内的真实位置
    let msg_idx = messages.len();
    for (ti, tc) in p.tool_calls.iter().enumerate() {
        if !tc.id.is_empty() {
            tool_index.insert(tc.id.clone(), (msg_idx, ti));
        }
    }
    messages.push(TranscriptMessage {
        seq: 0,
        role: Role::Assistant,
        kind: MessageKind::Text,
        text: clipped,
        truncated,
        tool_calls: p.tool_calls,
        thinking,
        timestamp: p.timestamp,
        model: p.model,
        images,
    });
}

fn parse_claude_jsonl(
    path: &Path,
    include_sidechain: bool,
    decode_images: bool,
) -> Result<ParseResult> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut messages: Vec<TranscriptMessage> = Vec::new();
    // tool_use id → (消息占位:回填时按 id 在已 flush 消息与 pending 中查)
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut custom_title = String::new();
    let mut fallback_title = String::new();
    let mut cwd = String::new();
    let mut git_branch: Option<String> = None;
    let mut model: Option<String> = None;
    let mut tokens_used: i64 = 0;
    let mut created_at: i64 = 0;
    let mut updated_at: i64 = 0;
    let mut unknown_lines: u32 = 0;
    let mut pending: Option<PendingAssistant> = None;

    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            unknown_lines += 1;
            continue;
        };
        let typ = row.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if typ == "custom-title" {
            if let Some(t) = row.get("customTitle").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    custom_title = t.trim().to_string();
                }
            }
            continue;
        }
        if typ != "user" && typ != "assistant" && typ != "system" {
            if typ.is_empty() || !KNOWN_SKIP_TYPES.contains(&typ) {
                unknown_lines += 1;
            }
            continue;
        }

        // ---- 消息行公共信封 ----
        if cwd.is_empty() {
            if let Some(c) = row.get("cwd").and_then(|v| v.as_str()) {
                cwd = c.to_string();
            }
        }
        if let Some(b) = row.get("gitBranch").and_then(|v| v.as_str()) {
            if !b.is_empty() {
                git_branch = Some(b.to_string());
            }
        }
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        if ts > 0 {
            if created_at == 0 {
                created_at = ts;
            }
            if ts > updated_at {
                updated_at = ts;
            }
        }
        if row.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) && !include_sidechain {
            continue;
        }

        let message = row.get("message");

        if typ == "system" {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            let subtype = row.get("subtype").and_then(|v| v.as_str());
            if subtype == Some("compact_boundary") {
                messages.push(mk_msg(
                    Role::System,
                    MessageKind::CompactSummary,
                    "── Context compacted ──",
                    ts,
                ));
            } else if let Some(content) = row.get("content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    let (text, truncated) = clip(content, MAX_TOOL_IO);
                    messages.push(TranscriptMessage {
                        seq: 0,
                        role: Role::System,
                        kind: MessageKind::Meta,
                        text,
                        truncated,
                        tool_calls: Vec::new(),
                        thinking: None,
                        timestamp: ts_opt(ts),
                        model: None,
                        images: Vec::new(),
                    });
                }
            }
            continue;
        }

        if typ == "user" {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            let Some(message) = message else { continue };
            let content = message.get("content");
            let mut parsed_message = ParsedContent::default();
            let mut had_tool_result = false;

            match content {
                Some(Value::String(s)) => parsed_message.push_text(s),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                    parsed_message.push_text(t);
                                }
                            }
                            Some("tool_result") => {
                                had_tool_result = true;
                                let id =
                                    b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                                if let Some(&(mi, ti)) = tool_index.get(id) {
                                    let parsed = tool_result_parts(
                                        b.get("content").unwrap_or(&Value::Null),
                                        decode_images,
                                    );
                                    let message = &mut messages[mi];
                                    let target = &mut message.tool_calls[ti];
                                    target.output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                                    if b.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                                        target.is_error = true;
                                    }
                                    append_images_to_message_end(message, parsed.images);
                                }
                            }
                            _ if is_image_part(b) => {
                                let parsed = content_parts(b, decode_images);
                                parsed_message.append(parsed);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            let ParsedContent { text, images } = parsed_message;
            if text.is_empty() && images.is_empty() {
                let _ = had_tool_result;
                continue;
            }

            let is_meta = row.get("isMeta").and_then(|v| v.as_bool()) == Some(true);
            let is_compact = row.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true);
            let kind = if is_compact {
                MessageKind::CompactSummary
            } else if is_meta || is_injected_user_content(&text) {
                MessageKind::Meta
            } else {
                MessageKind::Text
            };
            if kind == MessageKind::Text && fallback_title.is_empty() {
                fallback_title = clean_title_candidate(&text);
            }
            let (clipped, truncated) = clip(&text, MAX_MSG_TEXT);
            messages.push(TranscriptMessage {
                seq: 0,
                role: Role::User,
                kind,
                text: clipped,
                truncated,
                tool_calls: Vec::new(),
                thinking: None,
                timestamp: ts_opt(ts),
                model: None,
                images,
            });
            continue;
        }

        // ---- assistant ----
        let Some(message) = message else { continue };
        let msg_id = message.get("id").and_then(|v| v.as_str()).map(String::from);
        let need_new = match (&pending, &msg_id) {
            (None, _) => true,
            (Some(p), Some(mid)) => p.msg_id.as_deref().is_some_and(|pid| pid != mid),
            (Some(_), None) => false,
        };
        if need_new {
            flush_assistant(&mut pending, &mut messages, &mut tool_index);
            pending = Some(PendingAssistant {
                msg_id: msg_id.clone(),
                timestamp: ts_opt(ts),
                ..Default::default()
            });
        }
        let p = pending.as_mut().unwrap();
        if p.msg_id.is_none() {
            p.msg_id = msg_id;
        }
        // "<synthetic>" = 系统合成消息(中断提示等)的占位 model,不是真实模型
        if let Some(m) = message.get("model").and_then(|v| v.as_str()) {
            if !m.is_empty() && m != "<synthetic>" {
                p.model = Some(m.to_string());
                model = Some(m.to_string());
            }
        }
        if let Some(usage) = message.get("usage") {
            tokens_used += usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + usage
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                + usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
        }
        match message.get("content") {
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    p.content.push_text(t);
                                }
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    p.thinking.push(t.to_string());
                                }
                            }
                        }
                        Some("tool_use") => {
                            let id = b
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                            p.tool_calls
                                .push(tool_call_view(id, name, &input, None, false));
                        }
                        _ if is_image_part(b) => {
                            let parsed = content_parts(b, decode_images);
                            p.content.append(parsed);
                        }
                        _ => {}
                    }
                }
            }
            Some(Value::String(s)) if !s.trim().is_empty() => p.content.push_text(s),
            _ => {}
        }
    }
    flush_assistant(&mut pending, &mut messages, &mut tool_index);
    assign_seq(&mut messages);

    Ok(ParseResult {
        title: if !custom_title.is_empty() {
            custom_title
        } else if !fallback_title.is_empty() {
            fallback_title
        } else {
            UNTITLED.to_string()
        },
        messages,
        cwd,
        git_branch,
        model,
        tokens_used,
        created_at,
        updated_at,
        unknown_lines,
    })
}

fn ts_opt(ts: i64) -> Option<i64> {
    if ts > 0 {
        Some(ts)
    } else {
        None
    }
}

fn mk_msg(role: Role, kind: MessageKind, text: &str, ts: i64) -> TranscriptMessage {
    TranscriptMessage {
        seq: 0,
        role,
        kind,
        text: text.to_string(),
        truncated: false,
        tool_calls: Vec::new(),
        thinking: None,
        timestamp: ts_opt(ts),
        model: None,
        images: Vec::new(),
    }
}

fn build_meta(r: &SessionFileRef, p: &ParseResult) -> SessionMeta {
    let project_name = project_name_of(&p.cwd);
    SessionMeta {
        key: format!("claude-code:{}", r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::ClaudeCode,
        title: p.title.clone(),
        project_path: p.cwd.clone(),
        project_name,
        file_path: r.file_path.clone(),
        created_at: if p.created_at > 0 {
            p.created_at
        } else {
            r.mtime_ms
        },
        updated_at: if p.updated_at > 0 {
            p.updated_at
        } else {
            r.mtime_ms
        },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: p.git_branch.clone(),
        model: p.model.clone(),
        tokens_used: if p.tokens_used > 0 {
            Some(p.tokens_used)
        } else {
            None
        },
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

fn subagents_dir(r: &SessionFileRef) -> PathBuf {
    Path::new(&r.file_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join(&r.native_id)
        .join("subagents")
}

fn list_sidechains(r: &SessionFileRef) -> Vec<SidechainInfo> {
    let dir = subagents_dir(r);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jsonl") {
            continue;
        }
        let id = name.trim_end_matches(".jsonl").to_string();
        let mut info = SidechainInfo {
            id: id.clone(),
            agent_type: None,
            description: None,
            tool_use_id: None,
        };
        if let Ok(meta_raw) = fs::read_to_string(dir.join(format!("{id}.meta.json"))) {
            if let Ok(meta) = serde_json::from_str::<Value>(&meta_raw) {
                info.agent_type = meta
                    .get("agentType")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                info.description = meta
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                info.tool_use_id = meta
                    .get("toolUseId")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
        }
        out.push(info);
    }
    out
}

/// projects/<有损编码目录>/ 里最新的一份会话(按 mtime,平局取 id 字典序靠后的)
/// 的 key:目录名反推不了项目路径,而目录里的会话都是同一个 cwd 起的——记忆挂到
/// 这条会话上,项目路径读库时按它从 sessions 表解析(索引已经算过,不再翻文件)。
/// 候选与会话枚举同一个判据(`default_file_ref`:.jsonl、非隐藏、非零字节),否则
/// 锚点会指向一条进不了库的会话、整组记忆落 Unknown project。没有会话给 None
fn newest_session_key(dir: &Path) -> Option<String> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| default_file_ref(AgentId::ClaudeCode, &e.path()))
        .max_by(|a, b| {
            a.mtime_ms
                .cmp(&b.mtime_ms)
                .then_with(|| a.native_id.cmp(&b.native_id))
        })
        .map(|r| session_key(AgentId::ClaudeCode, "", &r.native_id))
}

impl AgentAdapter for ClaudeAdapter {
    fn agent(&self) -> AgentId {
        AgentId::ClaudeCode
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(projects) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        for project in projects.flatten() {
            let Ok(entries) = fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".jsonl") {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() || meta.len() == 0 {
                    continue;
                }
                refs.push(SessionFileRef {
                    agent: AgentId::ClaudeCode,
                    native_id: name.trim_end_matches(".jsonl").to_string(),
                    file_path: path.to_string_lossy().to_string(),
                    mtime_ms: mtime_ms(&meta),
                    size: meta.len() as i64,
                });
            }
        }
        Ok(refs)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_claude_jsonl(Path::new(&r.file_path), false, false)?;
        let meta = build_meta(r, &parsed);
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_claude_jsonl(Path::new(&r.file_path), false, true)?;
        let sidechains = list_sidechains(r);
        let mut mainline = parsed.messages.clone();
        // 把 sidechain 挂到主线对应 Task tool call
        let by_tool_use: HashMap<&str, &SidechainInfo> = sidechains
            .iter()
            .filter_map(|s| s.tool_use_id.as_deref().map(|t| (t, s)))
            .collect();
        for m in &mut mainline {
            for tc in &mut m.tool_calls {
                if let Some(sc) = by_tool_use.get(tc.id.as_str()) {
                    tc.sidechain_ref = Some(sc.id.clone());
                }
            }
        }
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline,
            sidechains,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn load_sidechain(
        &self,
        r: &SessionFileRef,
        sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        let file = subagents_dir(r).join(format!("{sidechain_id}.jsonl"));
        if !file.is_file() {
            return Ok(Vec::new());
        }
        let parsed = parse_claude_jsonl(&file, true, true)?;
        Ok(parsed.messages)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // subagents 转录与 memory 边车不是主会话。按所在目录的名字判:按路径分段,Windows 的
        // 反斜杠路径一样认(原先拿带 `/` 的字符串去比,watcher 报来的路径一条都对不上);只看
        // 直接所在的目录,数据根上层恰好有叫 memory / subagents 的目录也不误伤
        let parent = path.parent()?.file_name()?;
        if parent == "subagents" || parent == "memory" {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        let mut v = vec![meta.file_path.clone()];
        // <projects>/<dir>/<id>/ 边车目录(subagents 等)随会话一并删
        if let Some(dir) = Path::new(&meta.file_path).parent() {
            let sidecar = dir.join(&meta.id);
            if sidecar.is_dir() {
                v.push(sidecar.to_string_lossy().to_string());
            }
        }
        v
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn memory_sources(&self) -> Vec<MemorySource> {
        // auto-memory 树(agent 记的、按项目)+ 全局 CLAUDE.md(用户写的指令,对每个
        // 项目都成立);项目根下的 CLAUDE.md 由 project_instruction_sources 给
        let mut v = vec![MemorySource {
            agent: AgentId::ClaudeCode,
            kind: MemorySourceKind::ProjectTree,
            path: self.root.clone(),
        }];
        if let Some(home) = &self.home {
            v.push(MemorySource {
                agent: AgentId::ClaudeCode,
                kind: MemorySourceKind::File,
                path: home.join("CLAUDE.md"),
            });
        }
        v
    }

    fn list_memories(
        &self,
        sources: &[MemorySource],
        projects: &[PathBuf],
    ) -> Result<Vec<MemoryDoc>> {
        super::memory_docs_with_tree(
            &self.memories,
            AgentId::ClaudeCode,
            sources,
            projects,
            |tree| self.tree_units(tree),
        )
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 `~/.claude` 形态(含 projects/)或直接选中 projects 目录都认;只有前者
        // 有 home(全局 CLAUDE.md 的所在),裸 projects 目录不摸父目录
        let (root, home) = if dir.join("projects").is_dir() {
            (dir.join("projects"), Some(dir))
        } else {
            (dir, None)
        };
        Box::new(Self {
            root,
            home,
            local: false,
            memories: super::MemoryCache::new(),
            decoded: std::sync::Mutex::new(HashMap::new()),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目录名反推项目路径:`.`、`_`、空格都被编成 `-`,同一层多个候选按最长优先,
    /// 磁盘上不存在的认不出。只在 unix 跑:Windows 的临时目录编码出来是 `C--Users-…`
    /// 形态,函数对它直接给 None(文档里写明不做),测试里 `starts_with('-')` 必红
    /// (2026-09-22 Windows CI)
    #[cfg(unix)]
    #[test]
    fn project_dir_name_decodes_against_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp
            .path()
            .join(".claude")
            .join("work trees")
            .join("my-app_v2");
        fs::create_dir_all(&project).unwrap();
        // 干扰项:同一层还有一个前缀相同但更短的目录
        fs::create_dir_all(tmp.path().join(".claude").join("work")).unwrap();
        let encoded: String = project
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        assert!(encoded.starts_with('-'), "{encoded}");
        assert_eq!(
            decode_project_dir(&encoded).as_deref(),
            Some(project.to_string_lossy().as_ref())
        );
        assert_eq!(decode_project_dir(&format!("{encoded}-nope")), None);
        assert_eq!(decode_project_dir("Users-x"), None, "不是绝对路径形态");
    }
}
