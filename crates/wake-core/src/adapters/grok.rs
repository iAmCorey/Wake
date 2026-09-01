use super::grok_group::{self, GroupCtx};
use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Grok Build(xAI 的 coding CLI):`~/.grok/sessions/<url编码cwd>/<uuid>/` 一目录
/// 一会话。主文件 `updates.jsonl` 是 ACP 风格完整流水({timestamp(unix秒),
/// method:"session/update",params:{update:{sessionUpdate:…}}}),chunk 流式落盘
/// 需按角色段合并;`chat_history.jsonl` 是被压缩过的上下文快照,不能当全量史。
/// `summary.json` 边车给 cwd/标题/git 信息。同级 `session_search.sqlite` 是
/// Grok 自己的索引,`prompt_history.jsonl` 在 cwd 目录级——都不是会话文件。
pub struct GrokAdapter {
    root: PathBuf,
    grok_home: PathBuf,
    group: Mutex<Option<Arc<GroupCtx>>>,
}

impl GrokAdapter {
    pub fn new() -> Self {
        let grok_home = super::home_dir().unwrap_or_default().join(".grok");
        Self {
            root: grok_home.join("sessions"),
            grok_home,
            group: Mutex::new(None),
        }
    }

    fn from_custom_dir(dir: PathBuf) -> Self {
        let (root, grok_home) = if dir.join("sessions").is_dir() {
            (dir.join("sessions"), dir)
        } else if dir.file_name().is_some_and(|name| name == "sessions") {
            let home = dir.parent().unwrap_or(&dir).to_path_buf();
            (dir, home)
        } else {
            // 保留旧行为：自定义目录也可以直接就是 sessions root。
            // 此时没有结构证据允许越过用户选择的边界去父目录读 sidecar。
            (dir.clone(), dir)
        };
        Self {
            root,
            grok_home,
            group: Mutex::new(None),
        }
    }

    fn group_ctx(&self) -> Arc<GroupCtx> {
        let mut group = self.group.lock().unwrap();
        if group.is_none() {
            *group = Some(grok_group::load_group_ctx(&self.root, &self.grok_home));
        }
        Arc::clone(group.as_ref().unwrap())
    }
}

/// summary.json 边车
struct Summary {
    cwd: String,
    title: String,
    created_ms: i64,
    updated_ms: i64,
    git_branch: Option<String>,
    model: Option<String>,
    git_remotes: Vec<String>,
}

fn read_summary(updates_path: &Path) -> Summary {
    let mut s = Summary {
        cwd: String::new(),
        title: String::new(),
        created_ms: 0,
        updated_ms: 0,
        git_branch: None,
        model: None,
        git_remotes: Vec::new(),
    };
    let path = updates_path.with_file_name("summary.json");
    if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(c) = v
                .get("info")
                .and_then(|i| i.get("cwd"))
                .and_then(|x| x.as_str())
            {
                s.cwd = c.to_string();
            }
            // generated_title 与 session_summary 通常同值,前者更语义化
            for k in ["generated_title", "session_summary"] {
                if let Some(t) = v.get(k).and_then(|x| x.as_str()) {
                    if !t.trim().is_empty() {
                        s.title = t.to_string();
                        break;
                    }
                }
            }
            if let Some(t) = v.get("created_at").and_then(|x| x.as_str()) {
                s.created_ms = iso_ms(t);
            }
            if let Some(t) = v.get("updated_at").and_then(|x| x.as_str()) {
                s.updated_ms = iso_ms(t);
            }
            s.git_branch = v
                .get("head_branch")
                .and_then(|x| x.as_str())
                .filter(|b| !b.is_empty())
                .map(String::from);
            s.model = v
                .get("current_model_id")
                .and_then(|x| x.as_str())
                .filter(|m| !m.is_empty())
                .map(String::from);
            if let Some(remotes) = v.get("git_remotes").and_then(Value::as_array) {
                s.git_remotes = remotes
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|remote| !remote.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
    }
    s
}

/// 流水重放的段状态:一个用户回合断一次,assistant 期间 thought/message/tool
/// 交错全并入同一条助手消息
struct Replay {
    messages: Vec<TranscriptMessage>,
    tool_index: HashMap<String, (usize, usize)>,
    /// 当前累积段;None = 段外
    cur_role: Option<Role>,
    cur_text: String,
    cur_thinking: String,
    cur_tools: Vec<ToolCallView>,
    cur_ts: i64,
}

impl Replay {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            tool_index: HashMap::new(),
            cur_role: None,
            cur_text: String::new(),
            cur_thinking: String::new(),
            cur_tools: Vec::new(),
            cur_ts: 0,
        }
    }

    fn flush(&mut self) {
        let Some(role) = self.cur_role.take() else {
            return;
        };
        let text = std::mem::take(&mut self.cur_text);
        let thinking = std::mem::take(&mut self.cur_thinking);
        let tools = std::mem::take(&mut self.cur_tools);
        if text.trim().is_empty() && thinking.trim().is_empty() && tools.is_empty() {
            return;
        }
        let mut m = text_msg(role, &text, self.cur_ts);
        if !thinking.trim().is_empty() {
            m.thinking = Some(clip(thinking.trim(), MAX_TOOL_IO).0);
        }
        let base = self.messages.len();
        for (ix, tc) in tools.into_iter().enumerate() {
            self.tool_index.insert(tc.id.clone(), (base, ix));
            m.tool_calls.push(tc);
        }
        self.messages.push(m);
    }

    /// 进入(或延续)一个角色段
    fn ensure(&mut self, role: Role, ts: i64) {
        if self.cur_role != Some(role) {
            self.flush();
            self.cur_role = Some(role);
            self.cur_ts = ts;
        }
    }
}

/// user/agent chunk 的 content 文本({content:{type:text,text}}),借用零分配
fn chunk_text(update: &Value) -> &str {
    update
        .pointer("/content/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn tool_content_text(content: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in content.as_array().into_iter().flatten() {
        let inner = item.get("content").unwrap_or(item);
        let t = inner
            .get("text")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        if !t.trim().is_empty() {
            parts.push(t.trim().to_string());
        }
    }
    parts.join("\n")
}

/// tool_call_update 回填:完成态带 content(文本输出,覆盖式取最终态),
/// status failed 置错;初始 tool_call 缺 rawInput 时由 update 补
fn apply_tool_update(tc: &mut ToolCallView, update: &Value) {
    if let Some(content) = update.get("content") {
        let t = tool_content_text(content);
        if !t.is_empty() {
            tc.output = Some(clip(&t, MAX_TOOL_IO).0);
        }
    }
    if update.get("status").and_then(|v| v.as_str()) == Some("failed") {
        tc.is_error = true;
    }
    if tc.input.is_none() {
        if let Some(raw) = update.get("rawInput").filter(|r| !r.is_null()) {
            tc.input_preview = make_preview(raw);
            if let Ok(json) = serde_json::to_string_pretty(raw) {
                tc.input = Some(clip(&json, MAX_TOOL_IO).0);
            }
        }
    }
}

fn parse_grok_updates(path: &Path) -> Result<(Vec<TranscriptMessage>, u32)> {
    let file = fs::File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut rp = Replay::new();
    let mut unknown = 0u32;

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
        let ts = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        let Some(update) = row.get("params").and_then(|p| p.get("update")) else {
            unknown += 1;
            continue;
        };
        match update.get("sessionUpdate").and_then(|v| v.as_str()) {
            Some("user_message_chunk") => {
                rp.ensure(Role::User, ts);
                rp.cur_text.push_str(chunk_text(update));
            }
            Some("agent_message_chunk") => {
                rp.ensure(Role::Assistant, ts);
                rp.cur_text.push_str(chunk_text(update));
            }
            Some("agent_thought_chunk") => {
                rp.ensure(Role::Assistant, ts);
                rp.cur_thinking.push_str(chunk_text(update));
            }
            Some("tool_call") => {
                rp.ensure(Role::Assistant, ts);
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let name = update
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
                rp.cur_tools
                    .push(tool_call_view(id.to_string(), name, &input, None, false));
            }
            Some("tool_call_update") => {
                let Some(id) = update.get("toolCallId").and_then(|v| v.as_str()) else {
                    continue;
                };
                // 目标 tool 可能还在当前未 flush 的段里,也可能已入 messages
                if let Some(tc) = rp.cur_tools.iter_mut().find(|tc| tc.id == id) {
                    apply_tool_update(tc, update);
                } else if let Some(&(mi, ti)) = rp.tool_index.get(id) {
                    apply_tool_update(&mut rp.messages[mi].tool_calls[ti], update);
                }
            }
            // 已知的非内容更新:任务后台化/压缩等
            Some(
                "task_backgrounded"
                | "task_completed"
                | "auto_compact_started"
                | "auto_compact_completed"
                | "compaction_checkpoint"
                | "plan"
                | "current_mode_update",
            ) => {}
            _ => {
                unknown += 1;
            }
        }
    }
    rp.flush();
    let mut messages = rp.messages;
    assign_seq(&mut messages);
    Ok((messages, unknown))
}

fn build_meta(
    r: &SessionFileRef,
    side: &Summary,
    messages: &[TranscriptMessage],
    grok_home: &Path,
    group: &GroupCtx,
) -> SessionMeta {
    let title = Some(clean_title_candidate(&side.title))
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    let msg_ts_max = messages
        .iter()
        .filter_map(|m| m.timestamp)
        .max()
        .unwrap_or(0);
    let (project_path, project_name) =
        grok_group::canonical_project(&side.cwd, &side.git_remotes, grok_home, &r.native_id, group);
    SessionMeta {
        key: format!("grok:{}", r.native_id),
        id: r.native_id.clone(),
        agent: AgentId::Grok,
        title,
        project_path,
        project_name,
        file_path: r.file_path.clone(),
        created_at: if side.created_ms > 0 {
            side.created_ms
        } else {
            r.mtime_ms
        },
        updated_at: match side.updated_ms.max(msg_ts_max) {
            t if t > 0 => t,
            _ => r.mtime_ms,
        },
        message_count: messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: side.git_branch.clone(),
        model: side.model.clone(),
        tokens_used: None,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for GrokAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Grok
    }

    fn begin_scan(&self) {
        *self.group.lock().unwrap() = Some(grok_group::load_group_ctx(&self.root, &self.grok_home));
    }

    fn manages_parent_links(&self) -> bool {
        true
    }

    fn parent_links(&self) -> Vec<(String, String)> {
        grok_group::parent_links(&self.group_ctx())
    }

    fn is_parent_link_event(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root).ok().is_some_and(|relative| {
            relative
                .components()
                .any(|component| component.as_os_str() == "subagents")
        })
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(cwds) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        // session_search.sqlite 等根级文件对 read_dir 自然失败跳过;
        // 会话主文件的判定(存在、非空、native_id)统一走 file_ref
        for cwd_dir in cwds.flatten() {
            let Ok(sessions) = fs::read_dir(cwd_dir.path()) else {
                continue;
            };
            for sess in sessions.flatten() {
                if let Some(r) = self.file_ref(&sess.path().join("updates.jsonl")) {
                    refs.push(r);
                }
            }
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 只认会话目录里的 updates.jsonl;prompt_history/chat_history/events 都不是主文件
        if path.file_name()?.to_string_lossy() != "updates.jsonl" {
            return None;
        }
        let session_dir = path.parent()?;
        let mut r = default_file_ref(self.agent(), path)?;
        r.native_id = session_dir.file_name()?.to_string_lossy().to_string();
        Some(r)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 会话是 <uuid>/ 整个目录(summary/events/plan 等边车),整目录进废纸篓
        Path::new(&meta.file_path)
            .parent()
            .map(|d| vec![d.to_string_lossy().to_string()])
            .unwrap_or_else(|| vec![meta.file_path.clone()])
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (messages, unknown) = parse_grok_updates(Path::new(&r.file_path))?;
        let side = read_summary(Path::new(&r.file_path));
        let meta = build_meta(r, &side, &messages, &self.grok_home, &self.group_ctx());
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: unknown,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (messages, unknown) = parse_grok_updates(Path::new(&r.file_path))?;
        let side = read_summary(Path::new(&r.file_path));
        Ok(ParsedTranscript {
            meta: build_meta(r, &side, &messages, &self.grok_home, &self.group_ctx()),
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: unknown,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        Box::new(Self::from_custom_dir(dir))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
