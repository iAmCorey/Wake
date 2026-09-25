use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// DeepSeek Harness(dsh):`~/.dsh/sessions/--<cwd转义>--/<id转义>/<日志>`,一目录一会话。
/// 日志文件名带**格式代数**:v0 是 `session.jsonl[.zstd]`,之后每代 `session.vN.jsonl[.zstd]`
/// (issue #45:0.1.6 起写 v3,master 已在写 v4)。dsh 迁移只新增下一代文件、不删旧代,
/// 读的永远是编号最大的那一代——`session_log` 单点裁决。默认落盘是 zstd **多帧连接**
/// (首帧 header 行,之后每次 append 一帧),标准流式解码到 EOF 即可;`compression: none`
/// 配置则是纯文本 .jsonl,两种后缀都认。首行 {type:"session",version,id,createdAt,cwd,…}
/// 给权威 id/cwd/created,不反推目录名;origin=="subagent" 或 delegationDepth>0 是子代理
/// 会话,不进列表。事件行 {type,seq,time,data}:assistant/message 是一次模型调用的合成体
/// (v0/v1 的 assistant/chunk 与其打包行 *-chunks 直接跳过,v2 起流嵌在消息里),tool/result
/// 按 toolCallId 回填(v0–v3 包在一个 tool-result 块里,v4 直接挂在 tool 角色消息上),
/// system/message(v3 起系统提示在对话里)与 developer/message(v4)归 Meta,session/title
/// 事件 last-wins 给标题,model 在 assistant 消息的 source 里。
pub struct DshAdapter {
    root: PathBuf,
}

impl DshAdapter {
    pub fn new() -> Self {
        Self {
            root: super::home_dir()
                .unwrap_or_default()
                .join(".dsh")
                .join("sessions"),
        }
    }
}

/// 打开会话日志为行读取器,按后缀透明解压(.zstd = 多帧连接,Decoder 默认解到
/// EOF)。cap 是缓冲尺寸:只读首行的 header 路径用小值,免得每次列举都预读兆级
fn open_log(path: &Path, cap: usize) -> Result<Box<dyn BufRead>> {
    let file = fs::File::open(path)?;
    if path.extension().is_some_and(|e| e == "zstd") {
        let dec = zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(cap, file))?;
        Ok(Box::new(BufReader::with_capacity(cap, dec)))
    } else {
        Ok(Box::new(BufReader::with_capacity(cap, file)))
    }
}

struct DshHeader {
    id: String,
    cwd: String,
    created_at: i64,
    subagent: bool,
}

fn parse_header(v: &serde_json::Value) -> Option<DshHeader> {
    if v.get("type").and_then(|t| t.as_str()) != Some("session") {
        return None;
    }
    Some(DshHeader {
        id: v.get("id")?.as_str()?.to_string(),
        cwd: v
            .get("cwd")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string(),
        created_at: v.get("createdAt").and_then(|t| t.as_i64()).unwrap_or(0),
        subagent: v.get("origin").and_then(|o| o.as_str()) == Some("subagent")
            || v.get("delegationDepth")
                .and_then(|d| d.as_i64())
                .unwrap_or(0)
                > 0,
    })
}

/// 只读首行拿 header(zstd 惰性解码,不会解整个文件;非会话日志返回 None)
fn read_header(path: &Path) -> Option<DshHeader> {
    let mut reader = open_log(path, 8 << 10).ok()?;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    parse_header(&serde_json::from_str(line.trim_end()).ok()?)
}

/// dsh 的事件词汇,照抄上游 `KNOWN_SESSION_EVENT_TYPES`(dsh-session 包的 known-event-types.ts,
/// 生成文件;当前是 v4)。上面有 match 分支的内容事件也在里面——先到先得、这里不会命中;
/// 上游换代时整段替换即可,不用挑
const UPSTREAM_EVENT_TYPES: &[&str] = &[
    "agent-preset/selected",
    "agent/inbox/spliced",
    "approval/asked",
    "approval/decided",
    "approval/policy",
    "assistant/attempt",
    "assistant/message",
    "command/done",
    "command/run",
    "compaction/end",
    "compaction/prune",
    "compaction/start",
    "compaction/summary",
    "deliverables/presented",
    "developer/message",
    "feedback/message-delete",
    "feedback/message-put",
    "feedback/record",
    "goal/change",
    "hook/invoked",
    "hook/result",
    "image/offload",
    "llm/retry",
    "llm/retry-started",
    "model/selection",
    "permission/preset",
    "plan/mode",
    "request/context",
    "request/header",
    "sandbox/mode",
    "schedule/change",
    "session-log-deepseek/delivery-accepted",
    "session/end-seed",
    "session/title",
    "session/title-llm-request",
    "step/end",
    "step/start",
    "subagent/catalog",
    "subagent/descriptor",
    "subagent/model-selection-policy",
    "system/message",
    "team/member",
    "team/message/delivered",
    "team/message/queued",
    "team/task",
    "todo/write",
    "tool-workflow/agent-end",
    "tool-workflow/agent-start",
    "tool-workflow/run-end",
    "tool-workflow/run-start",
    "tool/call",
    "tool/ptc-dispatch",
    "tool/ptc-dispatch-start",
    "tool/result",
    "turn/end",
    "turn/start",
    "user/message",
    "web/deepseek-search-llm-request",
    "workspace/changes",
];

/// 上游清单已经删掉、只在老格式里出现的名字:v0/v1 的 assistant/chunk 与三种 chunk 打包
/// 存储行,v3 前的 tool/code-dispatch*。没被 dsh 重新打开过的老会话不会迁移,照样得认——
/// 只增不减
const LEGACY_EVENT_TYPES: &[&str] = &[
    "assistant/chunk",
    "text-chunks",
    "reasoning-chunks",
    "tool-call-chunks",
    "tool/code-dispatch",
    "tool/code-dispatch-start",
];

/// 已知最新的格式代数。更新的一代照样选中、按形状读,但 header 那一行记一笔 unknown:
/// 字段在已知事件里挪了位置时(v4 的 tool/result 就是),一个事件名都不会漏,只有这一笔
/// 能让 Clean up 知道这个会话读不全
const NEWEST_FORMAT: u64 = 4;

/// 会话日志文件名 → (格式代数, 是否 zstd)。v0 是 `session.jsonl[.zstd]`,之后每代
/// `session.vN.jsonl[.zstd]`(N 从 1 起、不带前导零——`session.v0` / `session.v03` 不是 dsh
/// 会写的名字,认了就会两个名字指同一代);别的文件(session.lock 等)给 None
fn log_generation(name: &str) -> Option<(u32, bool)> {
    let (stem, zstd) = match name.strip_suffix(".zstd") {
        Some(stem) => (stem, true),
        None => (name, false),
    };
    let version = stem.strip_suffix(".jsonl")?.strip_prefix("session")?;
    if version.is_empty() {
        return Some((0, zstd));
    }
    let digits = version.strip_prefix(".v")?;
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, zstd))
}

/// 会话目录 → 会话引用。日志取 dsh 自己会读的那一份:编号最大的一代(迁移只新增、不删
/// 旧代,旧代的 mtime 可能更新);同一代压缩与明文并存(换过 compression 配置)时 mtime
/// 新者胜、平局 .zstd 胜。list 与 watcher(经 file_ref)两条入口共用这一处裁决——只做在
/// list 会让 watcher 把旧代的事件当主文件解析。首行 header 给权威 id(目录名是转义过的
/// id);子代理会话在此过滤
fn session_log(dir: &Path) -> Option<SessionFileRef> {
    let ((_, mtime, _), path, size) = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let (generation, zstd) = log_generation(&entry.file_name().to_string_lossy())?;
            let path = entry.path();
            // fs::metadata 跟随符号链接(DirEntry::metadata 不跟)
            let meta = fs::metadata(&path)
                .ok()
                .filter(|m| m.is_file() && m.len() > 0)?;
            Some(((generation, mtime_ms(&meta), zstd), path, meta.len()))
        })
        .max_by_key(|(rank, ..)| *rank)?;
    let header = read_header(&path).filter(|h| !h.subagent)?;
    Some(SessionFileRef {
        agent: AgentId::Dsh,
        native_id: header.id,
        file_path: path.to_string_lossy().to_string(),
        mtime_ms: mtime,
        size: size as i64,
    })
}

struct DshParse {
    header: Option<DshHeader>,
    title: Option<String>,
    last_ts: i64,
    messages: Vec<TranscriptMessage>,
    model: Option<String>,
    tokens_used: Option<i64>,
    unknown_lines: u32,
}

fn parse_dsh_log(path: &Path, decode_images: bool) -> Result<DshParse> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let reader = open_log(path, 1 << 20)?;

    let mut p = DshParse {
        header: None,
        title: None,
        last_ts: 0,
        messages: Vec::new(),
        model: None,
        tokens_used: None,
        unknown_lines: 0,
    };
    // toolCallId → (消息下标, tool_calls 下标),tool/result 事件回填用
    let mut tool_index: HashMap<String, (usize, usize)> = HashMap::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // 末帧可能是半写的(写端每次 append 一帧,扫描与 dsh 天然并发):
            // zstd decoder 对断尾**反复**返回 UnexpectedEof 而非 EOF,`continue`
            // 就是死循环——扫描线程打满 CPU、ScanFinale 永不 Drop、刷新弹窗按
            // 不变量 6 永久锁死。截断到此为止,已解析的部分照常呈现
            Err(_) => {
                p.unknown_lines += 1;
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            p.unknown_lines += 1;
            continue;
        };
        let ty = row.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        // 信封顶层的 surfaceOp 是联合类型(dsh-session types.d.ts:
        // `'append' | {op:'replace',start,end}`)——**replace 是对象不是字符串**,
        // 按字符串比永远不命中。compaction 与工具结果裁剪都用它把 start..end 的
        // surface 节点换成缩短版,那是喂模型的上下文;Wake 要还原用户当时看到的
        // 原文,所以整条跳过(留下被它遮蔽的原始节点)。缺失或 append 一律照旧
        if row.pointer("/surfaceOp/op").and_then(|v| v.as_str()) == Some("replace") {
            continue;
        }
        let ts = row.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
        p.last_ts = p.last_ts.max(ts);
        let data = row.get("data").unwrap_or(&serde_json::Value::Null);
        match ty {
            "session" => {
                if row
                    .get("version")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|v| v > NEWEST_FORMAT)
                {
                    p.unknown_lines += 1;
                }
                if let Some(h) = parse_header(&row) {
                    p.header = Some(h);
                }
            }
            // v3 起系统提示是对话里的一条 system/message;v4 的 developer/message 记会话中途
            // 的变更(工具增删是 tool-addition / tool-removal 块,没有文字)。都是给模型的
            // 上下文、不是对话,归 Meta。图片不解码、只留占位文字:解码了转录那边就只剩图片
            // 没有文字,这条被跳过,之后每条消息的 seq 都和索引错开一位
            "system/message" | "developer/message" => {
                let parsed = content_parts(
                    data.pointer("/message/content")
                        .unwrap_or(&serde_json::Value::Null),
                    false,
                );
                if !parsed.text.is_empty() {
                    p.messages.push(meta_msg(&parsed.text, ts));
                }
            }
            "user/message" => {
                let parsed = content_parts(
                    data.get("content").unwrap_or(&serde_json::Value::Null),
                    decode_images,
                );
                if parsed.text.is_empty() && parsed.images.is_empty() {
                    continue;
                }
                let kind = data.pointer("/source/kind").and_then(|k| k.as_str());
                let mut m = text_msg(Role::User, &parsed.text, ts);
                m.images = parsed.images;
                // dsh 的 source.kind 权威区分真人输入与注入上下文(实测一族:
                // "agent-instructions"/"plugin"/"skill-catalog",后续还会长)。
                // 白名单 "user":非真人一律归 Meta,缺失时当真人(宁可多显示)
                if kind.is_some_and(|k| k != "user") {
                    m.kind = MessageKind::Meta;
                }
                p.messages.push(m);
            }
            "assistant/message" => {
                let msg = data.get("message").unwrap_or(&serde_json::Value::Null);
                let content = msg.get("content").unwrap_or(&serde_json::Value::Null);
                // 单遍分桶:text 进正文、reasoning 进 thinking、tool-call 进工具
                // (blocks_text 类型盲,会把 reasoning 的 text 混进正文,不适用)
                let mut parsed_message = ParsedContent::default();
                let mut thinking_parts: Vec<&str> = Vec::new();
                let mut tools: Vec<ToolCallView> = Vec::new();
                for b in content.as_array().into_iter().flatten() {
                    let block_text = || {
                        b.get("text")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                    };
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(text) = block_text() {
                                parsed_message.push_text(text);
                            }
                        }
                        Some("reasoning") => thinking_parts.extend(block_text()),
                        Some("tool-call") => {
                            let id = b.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                            // arguments 是模型原样输出的 JSON 字符串,解析失败保留原文
                            let raw = b
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            let input = serde_json::from_str::<serde_json::Value>(raw)
                                .unwrap_or(serde_json::Value::String(raw.to_string()));
                            tools.push(tool_call_view(id.to_string(), name, &input, None, false));
                        }
                        _ if is_image_part(b) => {
                            let parsed = content_parts(b, decode_images);
                            parsed_message.append(parsed);
                        }
                        _ => {}
                    }
                }
                // 元数据先收再判空:content 为空的 assistant/message 是 dsh 专门
                // 用来挂 usage 的载体(dsh-session surface.d.ts:"exists only to
                // host usage",如撞上 max-tokens 的调用),提前 continue 会把这次
                // 调用的 model 与 token 一起丢掉——它落在会话末尾时最明显
                let model = msg
                    .pointer("/source/model")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                if model.is_some() {
                    p.model = model.clone();
                }
                if let Some(u) = data.get("usage") {
                    // TokenUsage 是"one model call"的账、三项 input 互斥(billed =
                    // 三项之和),所以**按调用累加**,不能 last-wins:一个 turn 多个
                    // step,last-wins 只会剩最后一次调用的量
                    let sum = [
                        "inputTokens",
                        "outputTokens",
                        "cacheReadTokens",
                        "cacheWriteTokens",
                    ]
                    .iter()
                    .filter_map(|k| u.get(*k).and_then(|v| v.as_i64()))
                    .sum::<i64>();
                    if sum > 0 {
                        p.tokens_used = Some(p.tokens_used.unwrap_or(0) + sum);
                    }
                }
                if parsed_message.text.is_empty()
                    && tools.is_empty()
                    && thinking_parts.is_empty()
                    && parsed_message.images.is_empty()
                {
                    continue;
                }
                // 一个 turn 多个 step,每 step 一条 assistant/message;连续 assistant
                // 行(中间只隔 tool/result)合并成一条,详情页每回合一条助手消息
                // (合并 + 整体 clip 机制与 pi.rs 同构,改动需两侧同步)
                if !matches!(p.messages.last(), Some(m) if m.role == Role::Assistant) {
                    p.messages.push(text_msg(Role::Assistant, "", ts));
                }
                let base = p.messages.len() - 1;
                let last = &mut p.messages[base];
                if !parsed_message.text.is_empty() || !parsed_message.images.is_empty() {
                    if last.text.len() < MAX_MSG_TEXT {
                        append_content_to_message(last, parsed_message, "\n\n");
                    } else {
                        append_images_to_message_end(last, parsed_message.images);
                    }
                    if last.text.len() > MAX_MSG_TEXT {
                        let (t, _) = clip(&last.text, MAX_MSG_TEXT);
                        last.text = t;
                        last.truncated = true;
                    }
                }
                // thinking 与全库其他家同压 MAX_TOOL_IO;已到上限就不再拼接
                if !thinking_parts.is_empty()
                    && last.thinking.as_deref().map_or(0, str::len) < MAX_TOOL_IO
                {
                    let joined = thinking_parts.join("\n\n");
                    match &mut last.thinking {
                        Some(t) => {
                            t.push_str("\n\n");
                            t.push_str(&joined);
                            if t.len() > MAX_TOOL_IO {
                                *t = clip(t, MAX_TOOL_IO).0;
                            }
                        }
                        slot => *slot = Some(clip(&joined, MAX_TOOL_IO).0),
                    }
                }
                if model.is_some() {
                    last.model = model;
                }
                for tc in tools {
                    tool_index.insert(tc.id.clone(), (base, last.tool_calls.len()));
                    last.tool_calls.push(tc);
                }
            }
            "tool/result" => {
                // v4:tool 角色消息自己带 toolCallId / content / isError;v0–v3:
                // data.message.content = [tool-result 块],三样包在块里
                let msg = data.get("message").unwrap_or(&serde_json::Value::Null);
                let block = if msg.get("toolCallId").is_some() {
                    msg
                } else {
                    msg.pointer("/content/0")
                        .unwrap_or(&serde_json::Value::Null)
                };
                let Some(call_id) = block.get("toolCallId").and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Some(&(mi, ti)) = tool_index.get(call_id) {
                    let parsed = content_parts(
                        block.get("content").unwrap_or(&serde_json::Value::Null),
                        decode_images,
                    );
                    let message = &mut p.messages[mi];
                    let tc = &mut message.tool_calls[ti];
                    if !parsed.text.is_empty() {
                        tc.output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                    }
                    if block.get("isError").and_then(|v| v.as_bool()) == Some(true)
                        || data.get("error").is_some_and(|e| !e.is_null())
                    {
                        tc.is_error = true;
                    }
                    append_images_to_message_end(message, parsed.images);
                }
            }
            "session/title" => {
                if let Some(t) = data.get("title").and_then(|v| v.as_str()) {
                    let t = t.trim();
                    if !t.is_empty() {
                        p.title = Some(clean_title_candidate(t));
                    }
                }
            }
            "request/context" => {
                // 路由元数据(仅变更时记录);assistant/message 的 source.model 会覆盖
                if p.model.is_none() {
                    if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                        p.model = Some(m.to_string());
                    }
                }
            }
            // 信封自带 ignorable 标记 = 写端声明的纯信息性记录,读者可安全跳过
            _ if row.get("ignorable").and_then(|v| v.as_bool()) == Some(true) => {}
            // 其余已知词汇(上游清单 + 老格式名字)跳过,两张表外才计 unknown——保住 schema
            // 漂移金丝雀
            t if UPSTREAM_EVENT_TYPES.contains(&t) || LEGACY_EVENT_TYPES.contains(&t) => {}
            _ => {
                p.unknown_lines += 1;
            }
        }
    }
    assign_seq(&mut p.messages);
    Ok(p)
}

fn build_meta(r: &SessionFileRef, p: &DshParse) -> SessionMeta {
    let (native, cwd, created) = match &p.header {
        Some(h) => (h.id.clone(), h.cwd.clone(), h.created_at),
        None => (r.native_id.clone(), String::new(), 0),
    };
    let title = p
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| title_from_messages(&p.messages))
        .unwrap_or_else(|| UNTITLED.to_string());
    SessionMeta {
        key: format!("dsh:{native}"),
        host: String::new(),
        id: native,
        agent: AgentId::Dsh,
        title,
        project_path: cwd.clone(),
        project_name: project_name_of(&cwd),
        file_path: r.file_path.clone(),
        created_at: if created > 0 { created } else { r.mtime_ms },
        updated_at: if p.last_ts > 0 { p.last_ts } else { r.mtime_ms },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: None,
        model: p.model.clone(),
        tokens_used: p.tokens_used,
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for DshAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Dsh
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        // 固定两层:<project-dir>/<session-dir>/<日志>,不 walkdir 全递归(会话目录还会
        // 放别的 artifacts);根目录读不了就本家降级为空,不把整轮扫描炸掉
        let Ok(projects) = fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        for project in projects.flatten() {
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for session in fs::read_dir(project.path()).into_iter().flatten().flatten() {
                if !session.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                refs.extend(session_log(&session.path()));
            }
        }
        Ok(refs)
    }

    /// 同一会话的几份日志,新一代恒胜(位次小者胜:v0 = 255、v4 = 251),与
    /// `session_log` 的选代同一把尺子。list 只交出新一代还不够:库里已经记着旧代时
    /// (0.8.2 及以前的 Wake 只认 v0 的名字,或者同步、拷贝来的新代 mtime 反而更旧),
    /// 写库的副本裁决先比位次、再比 mtime,不给位次新代就会被挡回去
    fn dedup_rank(&self, path: &str) -> u8 {
        let generation = Path::new(path)
            .file_name()
            .and_then(|name| log_generation(&name.to_string_lossy()))
            .map_or(0, |(generation, _)| generation);
        u8::MAX - generation.min(u32::from(u8::MAX)) as u8
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 名字先粗筛(session.lock、迁移临时文件等不碰磁盘就不要),再看它是不是这个目录里
        // 被读的那一代
        log_generation(&path.file_name()?.to_string_lossy())?;
        session_log(path.parent()?).filter(|r| Path::new(&r.file_path) == path)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_dsh_log(Path::new(&r.file_path), false)?;
        let meta = build_meta(r, &parsed);
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_dsh_log(Path::new(&r.file_path), true)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        // 一目录一会话:trash 整个会话目录(各代日志、session.lock 与 artifacts 一起)
        match Path::new(&meta.file_path).parent() {
            Some(dir) => vec![dir.to_string_lossy().to_string()],
            None => vec![meta.file_path.clone()],
        }
    }

    fn cleanup_paths(&self, meta: &SessionMeta) -> Option<Vec<String>> {
        Some(self.session_paths(meta))
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        let root = if dir.join("sessions").is_dir() {
            dir.join("sessions")
        } else {
            dir
        };
        Box::new(Self { root })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::log_generation;

    #[test]
    fn log_names_carry_their_format_generation() {
        assert_eq!(log_generation("session.jsonl"), Some((0, false)));
        assert_eq!(log_generation("session.jsonl.zstd"), Some((0, true)));
        assert_eq!(log_generation("session.v3.jsonl.zstd"), Some((3, true)));
        assert_eq!(log_generation("session.v12.jsonl"), Some((12, false)));
        // dsh 不会写的名字:v0 不带版本号、没有前导零;锁文件与别的 artifacts 不是日志
        for name in [
            "session.v0.jsonl",
            "session.v03.jsonl.zstd",
            "session.v.jsonl",
            "session.v3a.jsonl",
            "session.v3.jsonl.tmp",
            "session.lock",
            "notes.jsonl",
        ] {
            assert_eq!(log_generation(name), None, "{name}");
        }
    }
}
