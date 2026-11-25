use crate::models::*;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE as BASE64_URL_SAFE};
use base64::Engine as _;
use serde_json::Value;
use std::cell::Cell;

const MAX_TRANSCRIPT_IMAGE_BYTES: usize = 64 * 1024 * 1024;

thread_local! {
    static IMAGE_DECODE_REMAINING: Cell<Option<usize>> = const { Cell::new(None) };
}

/// 一次会话解析的图片压缩字节总预算。嵌套调用复用外层预算，离开解析作用域后恢复。
pub struct ImageDecodeBudgetGuard {
    previous: Option<usize>,
    changed: bool,
}

pub fn transcript_image_decode_budget(decode_images: bool) -> ImageDecodeBudgetGuard {
    image_decode_budget(decode_images.then_some(MAX_TRANSCRIPT_IMAGE_BYTES))
}

fn image_decode_budget(limit: Option<usize>) -> ImageDecodeBudgetGuard {
    IMAGE_DECODE_REMAINING.with(|remaining| {
        let previous = remaining.get();
        let changed = previous.is_none() && limit.is_some();
        if changed {
            remaining.set(limit);
        }
        ImageDecodeBudgetGuard { previous, changed }
    })
}

impl Drop for ImageDecodeBudgetGuard {
    fn drop(&mut self) {
        if self.changed {
            IMAGE_DECODE_REMAINING.with(|remaining| remaining.set(self.previous));
        }
    }
}

/// 预算还放不放得下 `bytes`(只看不扣):读文件之前先问,放不下的连读都不读
fn image_decode_budget_allows(bytes: usize) -> bool {
    IMAGE_DECODE_REMAINING.with(|remaining| remaining.get().is_none_or(|left| bytes <= left))
}

fn consume_image_decode_budget(bytes: usize) -> bool {
    IMAGE_DECODE_REMAINING.with(|remaining| match remaining.get() {
        Some(left) if bytes <= left => {
            remaining.set(Some(left - bytes));
            true
        }
        Some(_) => false,
        None => true,
    })
}

/// JSONL 逐行解成 JSON:空行跳过,读不出(坏 UTF-8)或不是 JSON 的行交成 `Ok(None)`,调用方计
/// 未知行。读错误交一次 `Err` 就结束,调用方 `?` 出去让这次解析失败:`BufRead::lines` 遇到读
/// 错误会一直交回同一个错误(EIO、断开的网络盘、Windows 上被别的进程锁住的文件),`continue`
/// 接着读就是死循环——扫描线程卡在 SCAN_GATE 里,刷新弹窗永远关不掉;当成读完了,半截转录就
/// 带着整个文件的 mtime / size 入库,文件不再变就永远停在半截。解析失败则库里的旧行原样留着、
/// 下一轮再试
pub fn jsonl_values<R: std::io::BufRead>(
    reader: R,
) -> impl Iterator<Item = std::io::Result<Option<Value>>> {
    jsonl_rows(reader)
}

/// `jsonl_values` 的窄版:每行只反序列化成 `T`(用不到的字段直接跳过,不建整棵 Value 树),
/// 读错误规则相同;读不出、不是 JSON、对不上形状的行交 `Ok(None)`
pub fn jsonl_rows<T: serde::de::DeserializeOwned, R: std::io::BufRead>(
    reader: R,
) -> impl Iterator<Item = std::io::Result<Option<T>>> {
    jsonl_lines(reader).filter_map(|line| match line {
        Ok(Some(line)) if line.trim().is_empty() => None,
        Ok(line) => Some(Ok(line.and_then(|line| serde_json::from_str(&line).ok()))),
        Err(e) => Some(Err(e)),
    })
}

/// `jsonl_rows` 的预筛版:行里没有 `needle` 的不交给 serde、直接跳过(不给 `Ok(None)`)——在大文件里
/// 只找少数几行时省掉逐行解析(豆包工作在转录里找 `create_agent` 的结果)。读错误规则相同
pub fn jsonl_rows_containing<'a, T, R>(
    reader: R,
    needle: &'a str,
) -> impl Iterator<Item = std::io::Result<Option<T>>> + 'a
where
    T: serde::de::DeserializeOwned + 'a,
    R: std::io::BufRead + 'a,
{
    jsonl_lines(reader).filter_map(move |line| match line {
        Ok(Some(line)) if !line.contains(needle) => None,
        Ok(line) => Some(Ok(line.and_then(|line| serde_json::from_str(&line).ok()))),
        Err(e) => Some(Err(e)),
    })
}

/// 读不读得出:不存在(含"中间某段不是目录")是确定没有,别的是这一刻读不出
pub fn is_absent(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// `BufRead::lines` 加上上面那条读错误规则:坏 UTF-8 的行字节已经读过,交成 `Ok(None)` 接着读
fn jsonl_lines<R: std::io::BufRead>(reader: R) -> JsonlLines<R> {
    JsonlLines {
        lines: reader.lines(),
        done: false,
    }
}

struct JsonlLines<R> {
    lines: std::io::Lines<R>,
    done: bool,
}

impl<R: std::io::BufRead> Iterator for JsonlLines<R> {
    type Item = std::io::Result<Option<String>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.lines.next()? {
            Ok(line) => Some(Ok(Some(line))),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Some(Ok(None)),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// 文件 mtime → epoch ms(各家 adapter 与 watcher 共用)
pub fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(system_time_ms).unwrap_or(0)
}

/// 文件创建时间 → epoch ms;平台或文件系统不记的给 None
pub fn created_ms(meta: &std::fs::Metadata) -> Option<i64> {
    meta.created().ok().and_then(system_time_ms)
}

/// 文件的 (mtime 毫秒, 字节数),跟随符号链接;不是普通文件、读不到元数据给 None
pub fn stat_stamp(path: &std::path::Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    meta.is_file().then(|| (mtime_ms(&meta), meta.len() as i64))
}

fn system_time_ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// 路径末段,`/` 与 `\` 都算分隔符:别的机器写下的路径(Codex 的 state DB 可能是 Windows 机器写的、
/// Grok Bot 的附件在云端电脑上),不能交给本机的 `Path`
pub fn path_tail(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// 并进 `SessionFileRef.size` 的边车指纹:展示要用的那几个字段的稳定哈希(FNV-1a,段与段之间隔一个
/// 0 字节),取高 32 位——FNV 的低位只由各字节的低位决定,原先按 1024 取模,"Oscar" 与 "Ada 4" 就
/// 撞在一起,改了名也认不出(2026-10-11 Codex review)。size 因此不再是字节数,没有谁拿会话的
/// size 当字节数用(界面、MCP、导出、Clean Up 都现 stat 文件)。按长度求和的话同长的改名认不
/// 出来(Antigravity 与 Kilo 旧版任务还在求和,换过来会让它们的会话各重解析一次)
pub fn sidecar_fingerprint(parts: &[&str]) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    (hash >> 32) as i64
}

/// cwd → 项目显示名(路径末段;空/异常 = "Unknown project")
pub fn project_name_of(cwd: &str) -> String {
    if cwd.is_empty() {
        return "Unknown project".to_string();
    }
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown project".to_string())
}

/// 项目路径的规范形。Windows 形状的路径(`c:\…`、`C:/…`、`//server/…`)盘符改大写,
/// 在 Windows 上斜杠换成反斜杠:VS Code 的 fsPath 写小写盘符,OpenCode 系把目录存成
/// 正斜杠(它自己读出来时再换回),原样用就与别家记下的 `C:\…` 分成两个项目。
/// POSIX 路径(含远程镜像来的)原样
pub fn canonical_project_path(path: &str) -> String {
    let drive = matches!(path.as_bytes(), [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic());
    if !drive && !path.starts_with("//") {
        return path.to_string();
    }
    let mut out = if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    };
    if drive {
        out[..1].make_ascii_uppercase();
    }
    out
}

/// user 消息的 kind:注入内容归 Meta 折叠
pub fn user_kind(text: &str) -> MessageKind {
    if is_injected_user_content(text) {
        MessageKind::Meta
    } else {
        MessageKind::Text
    }
}

/// 纯文本消息构造(clip + user 注入判定),多家 adapter 共用
pub fn text_msg(role: Role, text: &str, ts: i64) -> TranscriptMessage {
    let (clipped, truncated) = clip(text.trim(), MAX_MSG_TEXT);
    let kind = if role == Role::User {
        user_kind(text)
    } else {
        MessageKind::Text
    };
    TranscriptMessage {
        seq: 0,
        role,
        kind,
        text: clipped,
        truncated,
        tool_calls: Vec::new(),
        thinking: None,
        timestamp: if ts > 0 { Some(ts) } else { None },
        model: None,
        images: Vec::new(),
    }
}

/// 给模型看、不是对话的上下文(系统提示、状态说明等):System 角色的 Meta 消息——
/// 不进 FTS、不算 message_count、不参与标题推导
pub fn meta_msg(text: &str, ts: i64) -> TranscriptMessage {
    let mut m = text_msg(Role::System, text, ts);
    m.kind = MessageKind::Meta;
    m
}

/// seq 回填——FTS seq 与详情页序号一致(跨文件不变量 1)的统一执行点,
/// 必须在消息序列定型后、入库/返回前调用
pub fn assign_seq(messages: &mut [TranscriptMessage]) {
    for (i, m) in messages.iter_mut().enumerate() {
        m.seq = i as i64;
    }
}

/// 标题推导:首条真实用户消息经清洗,各家共用的回退链
pub fn title_from_messages(messages: &[TranscriptMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User && m.kind == MessageKind::Text)
        .map(|m| clean_title_candidate(&m.text))
        .find(|title| !title.is_empty())
}

/// 会话标题:存着的名字为准,没有就退回首条提问;`placeholder` 时反过来——存着的只是占位
/// (ZCode 的 default、MiMo Code 的 fallback),有提问就用提问。都没有给 UNTITLED
pub fn session_title(stored: &str, placeholder: bool, messages: &[TranscriptMessage]) -> String {
    let stored = Some(clean_title_candidate(stored)).filter(|t| !t.is_empty());
    let asked = || title_from_messages(messages);
    if placeholder {
        asked().or(stored)
    } else {
        stored.or_else(asked)
    }
    .unwrap_or_else(|| UNTITLED.to_string())
}

/// 折掉的一段留下的那条标记:"── N messages <来由> ──",System 角色的 Meta
pub fn collapsed_marker(count: usize, origin: &str, ts: i64) -> TranscriptMessage {
    meta_msg(
        &format!(
            "── {count} message{} {origin} ──",
            crate::text::plural(count as i64)
        ),
        ts,
    )
}

/// 会话开头从父会话原样复制来的那段(Codex `spawn_agent` 的 fork_turns、Craft Agents
/// 的分支)折成一条 Meta 标记。原样入库就是:子会话偷走父会话的标题、父会话的每一句
/// 在 FTS 里出现两次、Insights 的 prompt 数翻倍。Meta 不进 FTS、不算 message_count、
/// 不参与标题推导,GUI / 导出 / MCP 也都跳过它——留一条而不是整段删掉,是为了"这里
/// 原本有东西"不至于无声消失;内容本体就在父会话里。`parent` 是标记里父会话的叫法
/// (Codex 说 thread)
pub fn collapse_inherited(messages: &mut Vec<TranscriptMessage>, cut: usize, parent: &str) {
    if cut == 0 {
        return;
    }
    let inherited = &messages[..cut];
    let turns = inherited
        .iter()
        .filter(|m| m.kind == MessageKind::Text)
        .count();
    let ts = inherited
        .iter()
        .find_map(|m| m.timestamp)
        .unwrap_or_default();
    let marker = collapsed_marker(turns, &format!("inherited from the parent {parent}"), ts);
    messages.splice(..cut, std::iter::once(marker));
}

// ---- Claude 式会话目录里的子代理(Claude Code、CodeBuddy / WorkBuddy、Qoder 同构) ----

/// 会话的同名目录(`<projects>/<slug>/<id>/`,子代理转录等边车在里面)
pub fn session_dir(r: &SessionFileRef) -> std::path::PathBuf {
    session_dir_of(std::path::Path::new(&r.file_path), &r.native_id)
}

/// 会话文件的同名目录。Qoder IDE / Qoder CN 的任务转录在 `<slug>/transcript/<id>.jsonl`,它的同名
/// 目录(工具结果、子代理)仍在 `<slug>/<id>/`
pub fn session_dir_of(file: &std::path::Path, id: &str) -> std::path::PathBuf {
    let mut parent = file.parent().unwrap_or(std::path::Path::new("."));
    if parent.file_name().is_some_and(|name| name == "transcript") {
        parent = parent.parent().unwrap_or(parent);
    }
    parent.join(id)
}

/// 子代理转录所在会话的目录(目录名就是派它的会话的 id):Task / Agent 派的在
/// `<会话>/subagents/agent-<id>.jsonl`,Workflow 跑出来的再深一层 `subagents/workflows/wf_<id>/`;
/// 同一目录里的 journal.jsonl 是 Workflow 的运行日志,不是转录。这些转录作为子会话入库,但不是
/// 能单独恢复的会话
pub fn subagent_session_dir(path: &std::path::Path) -> Option<&std::path::Path> {
    let name = path.file_name()?.to_str()?;
    if !(name.starts_with("agent-") && name.ends_with(".jsonl")) {
        return None;
    }
    let named = |dir: &std::path::Path, name: &str| dir.file_name().is_some_and(|n| n == name);
    let dir = path.parent()?;
    let subagents = if named(dir, "subagents") {
        dir
    } else {
        let run = dir.file_name()?.to_str()?;
        let workflows = dir.parent()?;
        let subagents = workflows.parent()?;
        (run.starts_with("wf_") && named(workflows, "workflows") && named(subagents, "subagents"))
            .then_some(subagents)?
    };
    subagents.parent()
}

pub fn is_subagent_transcript(path: &str) -> bool {
    subagent_session_dir(std::path::Path::new(path)).is_some()
}

/// 子代理转录所在会话的原生 id(`AgentAdapter::parent_in_path`)
pub fn subagent_parent(path: &std::path::Path) -> Option<String> {
    Some(
        subagent_session_dir(path)?
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    )
}

/// 一个会话目录里全部子代理转录(Task / Agent 派的与 Workflow 跑出来的),读不出的目录跳过
pub fn subagent_transcripts(session_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let files = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    let mut transcripts = Vec::new();
    for path in files(&session_dir.join("subagents")) {
        if path.file_name().is_some_and(|name| name == "workflows") {
            transcripts.extend(files(&path).iter().flat_map(|run| files(run)));
        } else {
            transcripts.push(path);
        }
    }
    transcripts.retain(|path| subagent_session_dir(path).is_some());
    transcripts
}

/// 一个会话目录里全部子代理转录的引用
pub fn subagent_refs(agent: AgentId, session_dir: &std::path::Path) -> Vec<SessionFileRef> {
    subagent_transcripts(session_dir)
        .iter()
        .filter_map(|path| subagent_ref(agent, path))
        .collect()
}

/// 监听事件落在子代理转录或它的边车上:换成转录的引用。`session_ok` 判它所在的会话目录是不是这个
/// 实例认的那一层(各家的数据根与目录深度不同)
pub fn subagent_event_ref(
    agent: AgentId,
    path: &std::path::Path,
    session_ok: impl FnOnce(&std::path::Path) -> bool,
) -> Option<SessionFileRef> {
    let transcript = match path.file_name()?.to_str()?.strip_suffix(".meta.json") {
        Some(stem) => path.with_file_name(format!("{stem}.jsonl")),
        None => path.to_path_buf(),
    };
    if !session_ok(subagent_session_dir(&transcript)?) {
        return None;
    }
    subagent_ref(agent, &transcript)
}

/// 子代理转录的边车 `agent-<id>.meta.json`:agentType / 任务说明 / 派它的 tool_use id
fn subagent_sidecar(transcript: &std::path::Path) -> std::path::PathBuf {
    transcript.with_extension("meta.json")
}

/// 子代理转录的引用,边车并进戳(列表与监听同一个)
fn subagent_ref(agent: AgentId, transcript: &std::path::Path) -> Option<SessionFileRef> {
    let mut r = default_file_ref(agent, transcript)?;
    if let Some((mtime, size)) = stat_stamp(&subagent_sidecar(transcript)) {
        r.mtime_ms = r.mtime_ms.max(mtime);
        r.size += size;
    }
    Some(r)
}

/// 一份子代理转录读成 `SidechainInfo`:边车给是谁、干什么、哪次调用派的(没有边车的家这几项为空);
/// 它同时作为子会话入库,`session_id` 就是文件名
pub fn sidechain_info(transcript: &std::path::Path) -> Option<SidechainInfo> {
    let id = transcript.file_stem()?.to_string_lossy().to_string();
    let sidecar = std::fs::read_to_string(subagent_sidecar(transcript))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let field = |key: &str| {
        sidecar
            .as_ref()
            .and_then(|v| text_at(v, key))
            .map(String::from)
    };
    Some(SidechainInfo {
        session_id: Some(id.clone()),
        agent_type: field("agentType"),
        description: field("description"),
        tool_use_id: field("toolUseId"),
        id,
    })
}

/// 会话直属的子代理(`<会话>/subagents/` 那一层;MCP 的 `subagent` 参数与导出按它读),派活的
/// 调用按边车记的 tool_use id 指向子会话。子代理自己下面没有这一层目录,给空
pub fn link_subagents(
    session_dir: &std::path::Path,
    mainline: &mut [TranscriptMessage],
) -> Vec<SidechainInfo> {
    let Ok(entries) = std::fs::read_dir(session_dir.join("subagents")) else {
        return Vec::new();
    };
    let sidechains: Vec<SidechainInfo> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| subagent_session_dir(path).is_some())
        .filter_map(|path| sidechain_info(&path))
        .collect();
    let by_call: std::collections::HashMap<&str, &SidechainInfo> = sidechains
        .iter()
        .filter_map(|s| s.tool_use_id.as_deref().map(|id| (id, s)))
        .collect();
    for call in mainline.iter_mut().flat_map(|m| m.tool_calls.iter_mut()) {
        if let Some(sidechain) = by_call.get(call.id.as_str()) {
            call.child_id = sidechain.session_id.clone();
        }
    }
    sidechains
}

/// Claude 式布局的会话转录:会话里派活的调用指向它派出去的子会话(`link_subagents`)
pub fn transcript_with_subagents(
    r: &SessionFileRef,
    meta: SessionMeta,
    mut mainline: Vec<TranscriptMessage>,
    unknown_line_count: u32,
) -> ParsedTranscript {
    let sidechains = link_subagents(&session_dir(r), &mut mainline);
    ParsedTranscript {
        meta,
        mainline,
        sidechains,
        unknown_line_count,
    }
}

/// 按 id 读会话直属的一份子代理转录(`AgentAdapter::load_sidechain`,MCP 的 `subagent` 参数);
/// 没有这份给空
pub fn load_subagent(
    r: &SessionFileRef,
    id: &str,
    parse: impl FnOnce(&std::path::Path) -> anyhow::Result<Vec<TranscriptMessage>>,
) -> anyhow::Result<Vec<TranscriptMessage>> {
    let file = session_dir(r).join("subagents").join(format!("{id}.jsonl"));
    if !file.is_file() {
        return Ok(Vec::new());
    }
    parse(&file)
}

/// 子代理转录作为子会话:是 agent 派出去的(Insights 不算),标题取边车记的任务说明——没有就照常
/// (多半是派活那句话)。不是子代理转录的不动
pub fn mark_subagent(meta: &mut SessionMeta, transcript: &std::path::Path) {
    if subagent_session_dir(transcript).is_none() {
        return;
    }
    meta.spawned = true;
    if let Some(task) = sidechain_info(transcript)
        .and_then(|info| info.description)
        .map(|task| clean_title_candidate(&task))
        .filter(|task| !task.is_empty())
    {
        meta.title = task;
    }
}

/// 子代理自己的文件:转录与它的边车(删除时一起走)
pub fn subagent_paths(transcript: &str) -> Vec<String> {
    let sidecar = subagent_sidecar(std::path::Path::new(transcript));
    let mut paths = vec![transcript.to_string()];
    if sidecar.is_file() {
        paths.push(sidecar.to_string_lossy().to_string());
    }
    paths
}

/// `AgentAdapter::file_ref` 的默认实现:非空 .jsonl,stem 即 native_id。
/// 覆写方可在此结果上做路径过滤或 native_id 改写。
pub fn default_file_ref(agent: AgentId, path: &std::path::Path) -> Option<SessionFileRef> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if name.starts_with('.') {
        return None;
    }
    let stem = name.strip_suffix(".jsonl")?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    Some(SessionFileRef {
        agent,
        native_id: stem.to_string(),
        file_path: path.to_string_lossy().to_string(),
        mtime_ms: mtime_ms(&meta),
        size: meta.len() as i64,
    })
}

/// Claude 式 projects 树(Qoder / CodeBuddy / WorkBuddy)的会话枚举:根直属的
/// `<session>.jsonl` 与下一层 `<slug>/<session>.jsonl` 都收——自定义 location
/// 允许直接选中某个 slug 目录;会话同名目录里派出去的子代理(`<session>/subagents/`)
/// 作为子会话一并收,别的边车不收(递归的 list_jsonl_refs 会把它们吞进来)。按路径排序
pub fn list_project_tree_refs(root: &std::path::Path, agent: AgentId) -> Vec<SessionFileRef> {
    let mut refs = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return refs;
    };
    let is_dir = |entry: &std::fs::DirEntry| entry.file_type().is_ok_and(|t| t.is_dir());
    for entry in entries.flatten() {
        let path = entry.path();
        if is_dir(&entry) {
            // slug 目录(里面是会话),或者根本身就是 slug 目录时的会话同名目录
            refs.extend(subagent_refs(agent, &path));
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for e in inner.flatten() {
                if is_dir(&e) {
                    refs.extend(subagent_refs(agent, &e.path()));
                } else if let Some(r) = default_file_ref(agent, &e.path()) {
                    refs.push(r);
                }
            }
        } else if let Some(r) = default_file_ref(agent, &path) {
            refs.push(r);
        }
    }
    refs.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    refs
}

/// Claude 式 projects 树里监听事件对应的会话(Qoder / CodeBuddy / WorkBuddy):根下一两层的
/// `<session>.jsonl`,或会话同名目录里的子代理转录(边车的事件换成转录的引用)。更深的
/// 别的边车(state、memory)都不是
pub fn project_tree_file_ref(
    root: &std::path::Path,
    agent: AgentId,
    path: &std::path::Path,
) -> Option<SessionFileRef> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.components().count() <= 2 {
        return default_file_ref(agent, path);
    }
    // 会话同名目录在根下一两层(根是 projects 目录或某个 slug 目录)
    subagent_event_ref(agent, path, |session| {
        session
            .strip_prefix(root)
            .is_ok_and(|session| session.components().count() <= 2)
    })
}

/// 上面那棵树里有没有任何会话文件:env 候选根只在探到真实数据时才采信
/// (存在但空的候选不能遮掉默认根)。见首个就停,不做完整枚举——它跑在
/// adapter 构造时、也就是应用启动路径上
pub fn project_tree_has_session(root: &std::path::Path, agent: AgentId) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::read_dir(&path).is_ok_and(|inner| {
                inner
                    .flatten()
                    .any(|e| default_file_ref(agent, &e.path()).is_some())
            })
        } else {
            default_file_ref(agent, &path).is_some()
        }
    })
}

/// `optional_string` 的借用版:`key` 上修剪过、非空的字符串
pub fn text_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `key` 上是 `true`(缺席、别的类型都算不是)
pub fn true_at(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

/// JSON 里"有就是非空字符串、否则没有"的字段:标题、模型名、cwd 一类
pub fn optional_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// usage 对象 → 本次调用的 token 数:`total_tokens` 优先;否则 Anthropic 形
/// input / output / cache 四项之和;再否则 OpenAI completions 形 prompt +
/// completion(Qoder 与 CodeBuddy 的 rawUsage 共用)
pub fn usage_tokens(usage: &Value) -> i64 {
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if total > 0 {
        return total;
    }
    let message_tokens: i64 = [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|key| usage.get(*key).and_then(Value::as_i64).unwrap_or(0))
    .sum();
    if message_tokens > 0 {
        return message_tokens;
    }
    ["prompt_tokens", "completion_tokens"]
        .iter()
        .map(|key| usage.get(*key).and_then(Value::as_i64).unwrap_or(0))
        .sum()
}

/// 递归枚举 `dir` 下的 `*.jsonl` 普通文件。file_type 来自 readdir、不额外
/// stat;根不存在或不是目录给空迭代(各家 list_session_files 对缺根自行降级,
/// 不变量 8①)。想在文件边界再加判据的家(codex)拿它喂自己的 `file_ref` 漏斗
pub fn jsonl_entries(dir: &std::path::Path) -> impl Iterator<Item = walkdir::DirEntry> {
    walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".jsonl")
        })
}

/// 递归枚举目录下非空 .jsonl 为 SessionFileRef;`native_id` 从文件 stem 提取
/// 会话 id(多数家恒等,codex 需剥 rollout 前缀)
pub fn list_jsonl_refs(
    dir: &std::path::Path,
    agent: AgentId,
    native_id: impl Fn(&str) -> String,
) -> Vec<SessionFileRef> {
    let mut refs = Vec::new();
    for entry in jsonl_entries(dir) {
        let name = entry.file_name().to_string_lossy();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() == 0 {
            continue;
        }
        refs.push(SessionFileRef {
            agent,
            native_id: native_id(stem),
            file_path: entry.path().to_string_lossy().to_string(),
            mtime_ms: mtime_ms(&meta),
            size: meta.len() as i64,
        });
    }
    refs
}

/// content blocks → 文本和图片。支持各家目前出现的 Anthropic、OpenAI、
/// Gemini、Pi/Kiro 与 OpenCode 形状；未知、远程、本地路径或过大的图片退回占位符。
#[derive(Debug, Default)]
pub struct ParsedContent {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl ParsedContent {
    /// 追加一个正文块，并保持已经记录的图片偏移不变。
    pub fn push_text(&mut self, value: &str) {
        self.push_text_with_separator(value, "\n\n");
    }

    pub fn push_text_with_separator(&mut self, value: &str, separator: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str(separator);
        }
        self.text.push_str(value);
    }

    /// 追加另一段已解析内容；图片偏移随正文基址一起平移。
    pub fn append(&mut self, other: Self) {
        self.append_with_separator(other, "\n\n");
    }

    pub fn append_with_separator(&mut self, mut other: Self, separator: &str) {
        let had_text = !self.text.is_empty();
        let has_other_text = !other.text.is_empty();
        let base = self.text.len();
        let separator_len = if had_text && has_other_text {
            separator.len()
        } else {
            0
        };
        for image in &mut other.images {
            image.text_offset += base
                + if image.text_offset > 0 {
                    separator_len
                } else {
                    0
                };
        }
        if has_other_text {
            if separator_len > 0 {
                self.text.push_str(separator);
            }
            self.text.push_str(&other.text);
        }
        self.images.extend(other.images);
    }

    pub fn push_image(&mut self, mut image: ImageAttachment) {
        image.text_offset = self.text.len();
        self.images.push(image);
    }
}

/// 把新内容并入已存在的消息，并同步平移图片的正文偏移。
fn append_content_to_message(message: &mut TranscriptMessage, content: ParsedContent) {
    let mut combined = ParsedContent {
        text: std::mem::take(&mut message.text),
        images: std::mem::take(&mut message.images),
    };
    combined.append_with_separator(content, "\n\n");
    message.text = combined.text;
    message.images = combined.images;
}

/// 一次模型调用的产出并进最后一条助手消息(最后一条不是助手就先开一条空的),返回它的下标
/// (登记工具调用用)。整个 agentic 回合并成一条,单行 `text_msg` 的 clip 靠不住:正文合并后
/// 统一压 MAX_MSG_TEXT(截过之后只收图片),thinking 压 MAX_TOOL_IO、截过就不再拼接——反复
/// 对已带截断标记的串再 clip,标记前会一次次多出换行。截过的串连标记一起必然长过上限,所以
/// 守卫是 `<=`:恰好停在上限上的还要再收一次,不然后面的正文悄悄丢掉、连标记都没有。
/// 最后一条还没有时间时用这一次的
pub fn merge_into_last_assistant(
    messages: &mut Vec<TranscriptMessage>,
    ts: i64,
    content: ParsedContent,
    thinking: &str,
) -> usize {
    if !matches!(messages.last(), Some(m) if m.role == Role::Assistant) {
        messages.push(text_msg(Role::Assistant, "", ts));
    }
    let ix = messages.len() - 1;
    let last = &mut messages[ix];
    if last.timestamp.is_none() && ts > 0 {
        last.timestamp = Some(ts);
    }
    if !content.text.is_empty() || !content.images.is_empty() {
        if last.text.len() <= MAX_MSG_TEXT {
            append_content_to_message(last, content);
            if last.text.len() > MAX_MSG_TEXT {
                last.text = clip(&last.text, MAX_MSG_TEXT).0;
                last.truncated = true;
            }
        } else {
            append_images_to_message_end(last, content.images);
        }
    }
    let thinking = thinking.trim();
    if !thinking.is_empty() && last.thinking.as_deref().map_or(0, str::len) <= MAX_TOOL_IO {
        match &mut last.thinking {
            Some(t) => {
                t.push_str("\n\n");
                t.push_str(thinking);
                if t.len() > MAX_TOOL_IO {
                    *t = clip(t, MAX_TOOL_IO).0;
                }
            }
            slot => *slot = Some(clip(thinking, MAX_TOOL_IO).0),
        }
    }
    ix
}

/// 工具结果不属于消息正文流；其图片统一放到现有正文末尾。
pub fn append_images_to_message_end(
    message: &mut TranscriptMessage,
    mut images: Vec<ImageAttachment>,
) {
    for image in &mut images {
        image.text_offset = message.text.len();
    }
    message.images.extend(images);
}

pub fn content_parts(v: &Value, decode_images: bool) -> ParsedContent {
    let mut parsed = ParsedContent::default();
    collect_content(v, decode_images, 0, &mut parsed);
    parsed
}

fn collect_content(v: &Value, decode_images: bool, depth: usize, parsed: &mut ParsedContent) {
    if depth > 4 {
        return;
    }
    match v {
        Value::String(value) => {
            parsed.push_text(value);
        }
        Value::Array(parts) => {
            for part in parts {
                collect_content(part, decode_images, depth + 1, parsed);
            }
        }
        Value::Object(_) => {
            if is_image_part(v) {
                if decode_images {
                    if let Some(image) = decode_image_part(v, depth) {
                        parsed.push_image(image);
                        return;
                    }
                }
                parsed.push_text(IMAGE_PLACEHOLDER);
                return;
            }

            let kind = part_kind(v);
            if matches!(kind.as_deref(), Some("text" | "input_text" | "output_text")) {
                if let Some(value) = v
                    .get("text")
                    .or_else(|| v.get("data"))
                    .and_then(Value::as_str)
                {
                    parsed.push_text(value);
                }
                return;
            }

            // Gemini 的普通 part 没有 type；Grok 的 content wrapper 则需要
            // 再下钻一层。reasoning/tool 块由各 adapter 自己分桶，不在这里混入正文。
            if kind.is_none() {
                if let Some(value) = v.get("text").and_then(Value::as_str) {
                    parsed.push_text(value);
                    return;
                }
                if let Some(inner) = v.get("content") {
                    collect_content(inner, decode_images, depth + 1, parsed);
                    return;
                }
            }
            if matches!(kind.as_deref(), Some("content" | "part")) {
                if let Some(inner) = v.get("content").or_else(|| v.get("data")) {
                    collect_content(inner, decode_images, depth + 1, parsed);
                }
            }
        }
        _ => {}
    }
}

/// 工具结果与普通消息块的区别是：未知对象在旧解析器里会以 JSON 原文显示。
/// 图片仍走统一解码，无法识别的非图片对象则保留这个兼容行为。
pub fn tool_result_parts(v: &Value, decode_images: bool) -> ParsedContent {
    let values = v
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(v));
    let mut result = ParsedContent::default();
    for value in values {
        let parsed = content_parts(value, decode_images);
        if !parsed.text.is_empty() {
            result.append_with_separator(parsed, "\n");
        } else if parsed.images.is_empty() && !value.is_null() {
            if let Ok(json) = serde_json::to_string(value) {
                result.push_text_with_separator(&json, "\n");
            }
        } else {
            result.append_with_separator(parsed, "\n");
        }
    }
    result
}

fn part_kind(v: &Value) -> Option<String> {
    v.get("type")
        .or_else(|| v.get("kind"))
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase().replace('-', "_"))
}

fn image_media_type(v: &Value) -> Option<String> {
    ["media_type", "mediaType", "mime_type", "mimeType", "mime"]
        .iter()
        .find_map(|key| v.get(*key).and_then(Value::as_str))
        .filter(|value| value.starts_with("image/"))
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .to_ascii_lowercase()
        })
}

pub fn is_image_part(v: &Value) -> bool {
    part_kind(v).is_some_and(|kind| kind.contains("image"))
        || image_media_type(v).is_some()
        || v.get("inlineData").is_some()
        || v.get("inline_data").is_some()
        || v.get("image_url").is_some()
        || v.get("imageUrl").is_some()
        || v.get("source")
            .is_some_and(|source| image_media_type(source).is_some())
}

pub fn has_image_content(v: &Value) -> bool {
    if is_image_part(v) {
        return true;
    }
    match v {
        Value::Array(values) => values.iter().any(has_image_content),
        Value::Object(_) => v
            .get("content")
            .or_else(|| v.get("data"))
            .is_some_and(has_image_content),
        _ => false,
    }
}

fn decode_image_part(v: &Value, depth: usize) -> Option<ImageAttachment> {
    if depth > 5 {
        return None;
    }
    for key in ["source", "inlineData", "inline_data"] {
        if let Some(inner) = v.get(key) {
            if let Some(image) = decode_image_part(inner, depth + 1) {
                return Some(image);
            }
        }
    }

    let media_type = image_media_type(v);
    for key in [
        "image_url",
        "imageUrl",
        "url",
        "uri",
        "fileUri",
        "file_uri",
        "path",
    ] {
        if let Some(source) = v.get(key) {
            if let Some(value) = source.as_str() {
                if let Some(image) = decode_image_source(value, media_type.as_deref()) {
                    return Some(image);
                }
            } else if let Some(image) = decode_image_part(source, depth + 1) {
                return Some(image);
            }
        }
    }

    if let Some(data) = v.get("data") {
        match data {
            Value::String(value) => {
                if value.starts_with("data:") {
                    return decode_data_uri(value);
                }
                if media_type.as_deref() == Some("image/svg+xml") && value.contains("<svg") {
                    return attachment_from_bytes(value.as_bytes().to_vec(), media_type.as_deref());
                }
                return decode_base64_image(value, media_type.as_deref());
            }
            Value::Array(values) if values.len() <= MAX_IMAGE_BYTES as usize => {
                let bytes = values
                    .iter()
                    .map(Value::as_u64)
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .map(u8::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                return attachment_from_bytes(bytes, media_type.as_deref());
            }
            Value::Object(_) => return decode_image_part(data, depth + 1),
            _ => {}
        }
    }
    None
}

fn decode_image_source(value: &str, _media_type: Option<&str>) -> Option<ImageAttachment> {
    if value.starts_with("data:") {
        return decode_data_uri(value);
    }
    // 会话文件是不可信输入，不能允许其中的 path/file:// 指向任意本地文件。
    // 各适配器只解析会话自身携带的 data URI/base64/byte-array 图片。
    None
}

fn decode_base64_image(data: &str, media_type: Option<&str>) -> Option<ImageAttachment> {
    if data.len() > MAX_IMAGE_B64 {
        return None;
    }
    let compact;
    let data = if data.bytes().any(|byte| byte.is_ascii_whitespace()) {
        compact = data
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        compact.as_slice()
    } else {
        data.as_bytes()
    };
    let bytes = BASE64
        .decode(data)
        .or_else(|_| BASE64_URL_SAFE.decode(data))
        .ok()?;
    attachment_from_bytes(bytes, media_type)
}

/// `data:image/png;base64,...` → 图片附件。只接受 image MIME 与 base64，
/// 不下载 http(s) URL。
pub fn decode_data_uri(uri: &str) -> Option<ImageAttachment> {
    let rest = uri.strip_prefix("data:")?;
    let (metadata, data) = rest.split_once(',')?;
    let mut fields = metadata.split(';');
    let media_type = fields.next()?;
    if !media_type.starts_with("image/") || !fields.any(|field| field == "base64") {
        return None;
    }
    decode_base64_image(data, Some(media_type))
}

/// 会话自己目录里的图片文件(Antigravity 的 `.user_uploaded/`)→ 图片附件。只给 adapter 自己
/// 枚举出来的文件、或按内容寻址的 id 在自己数据目录里拼出的路径用(CodeBuddy 的 blobs:
/// 只认十六进制 id 与图片扩展名),**不要拿转录内容里写的路径来调**——那条路一律不解引用
/// 本地路径(见 `image_parser_never_dereferences_local_paths`)。解开符号链接后必须仍在 `session` 目录里:
/// 远程镜像原样保留符号链接,指到外面的会读到本机的文件(中间哪一层目录是链接都一样)。
/// 类型按字节嗅探;解码预算与 base64 那条路同一份(调用方的解析要开着
/// `transcript_image_decode_budget`),按 stat 到的大小先问预算再读
pub fn image_from_session_file(
    session: &std::path::Path,
    path: &std::path::Path,
) -> Option<ImageAttachment> {
    let real = std::fs::canonicalize(path).ok()?;
    if !real.starts_with(std::fs::canonicalize(session).ok()?) {
        return None;
    }
    let len = std::fs::metadata(&real).ok()?.len();
    if len == 0 || len > MAX_IMAGE_BYTES || !image_decode_budget_allows(len as usize) {
        return None;
    }
    attachment_from_bytes(std::fs::read(&real).ok()?, None)
}

/// URI / 目录名里的 percent-encoding(`%2F` → `/`)。逐字节解,不成对的 `%` 原样留着,
/// 后面跟着多字节字符也不会切坏(按 `&str` 下标切会 panic)
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        if bytes[ix] == b'%' && ix + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_digit(bytes[ix + 1]), hex_digit(bytes[ix + 2])) {
                out.push((high << 4) | low);
                ix += 3;
                continue;
            }
        }
        out.push(bytes[ix]);
        ix += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(c: u8) -> Option<u8> {
    Some(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => return None,
    })
}

fn attachment_from_bytes(bytes: Vec<u8>, media_type: Option<&str>) -> Option<ImageAttachment> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let media_type = media_type
        .filter(|value| value.starts_with("image/"))
        .map(str::to_string)
        .or_else(|| sniff_image_media_type(&bytes).map(str::to_string))?;
    if !consume_image_decode_budget(bytes.len()) {
        return None;
    }
    Some(ImageAttachment {
        media_type,
        bytes,
        text_offset: 0,
    })
}

fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if std::str::from_utf8(&bytes[..bytes.len().min(512)])
        .ok()
        .is_some_and(|head| head.contains("<svg"))
    {
        Some("image/svg+xml")
    } else {
        None
    }
}

pub const IMAGE_PLACEHOLDER: &str = "[image]";

/// 拆掉 Codex Desktop 文件清单包装，但保留 `My request` 后的真实提问。
pub fn unwrap_file_preamble(text: &str) -> Option<String> {
    const HEADS: &[&str] = &[
        "# Files mentioned by the user",
        "# Files pasted by the user",
    ];
    const MARKERS: &[&str] = &["## My request for Codex:", "## My request:"];
    let text = text.trim_start();
    if !HEADS.iter().any(|head| text.starts_with(head)) {
        return None;
    }
    Some(
        MARKERS
            .iter()
            .find_map(|marker| text.find(marker).map(|index| (marker, index)))
            .map(|(marker, index)| text[index + marker.len()..].trim().to_string())
            .unwrap_or_default(),
    )
}

/// 剥掉 Codex Desktop 写入正文的 `<image ...>` 标签。真正的图片通常同时
/// 存在于 input_image 内容块；临时文件路径失效时不再污染正文。
pub fn strip_image_tags(text: &str) -> String {
    const INJECTED_PREFIX: &str = "<image name=[Image #";
    if !text.contains(INJECTED_PREFIX) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(INJECTED_PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(open_end) = after.find('>').map(|index| index + 1) else {
            out.push_str(after);
            return out;
        };
        let open_tag = &after[..open_end];
        if !open_tag.contains("] path=") {
            out.push_str(&after[..INJECTED_PREFIX.len()]);
            rest = &after[INJECTED_PREFIX.len()..];
            continue;
        }
        let end = after[open_end..]
            .find("</image>")
            .map(|index| open_end + index + "</image>".len())
            .unwrap_or(open_end);
        rest = &after[end..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 按文件 (mtime, size) 缓存的读取结果,一个路径一格(`MtimeCache` 的按路径版;边车、会话头这类
/// 每轮扫描、每批监听都要再问一遍的小文件)。`stamp` 是调用方刚 stat 到的戳,None = 文件不在,给空的;
/// 戳没变给缓存;读的那一刻文件刚好没了也给空的。**读不出报错,不交回上次读到的**:调用方会拿刚 stat
/// 到的新戳记进引用,旧内容配上新戳入库,文件不再变就再也不会重读(当成"没有"同理,2026-11-23
/// Codex review)——报错让这一次解析失败,下一轮再来
pub struct StampCache<T>(
    std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, ((i64, i64), std::sync::Arc<T>)>,
    >,
);

impl<T> Default for StampCache<T> {
    fn default() -> Self {
        Self(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

impl<T: Default> StampCache<T> {
    pub fn get(
        &self,
        path: &std::path::Path,
        stamp: Option<(i64, i64)>,
        read: impl FnOnce(&std::path::Path) -> std::io::Result<T>,
    ) -> std::io::Result<std::sync::Arc<T>> {
        let Some(stamp) = stamp else {
            return Ok(std::sync::Arc::default());
        };
        if let Some((seen, value)) = self.0.lock().unwrap().get(path) {
            if *seen == stamp {
                return Ok(value.clone());
            }
        }
        match read(path) {
            Ok(value) => {
                let value = std::sync::Arc::new(value);
                self.0
                    .lock()
                    .unwrap()
                    .insert(path.to_path_buf(), (stamp, value.clone()));
                Ok(value)
            }
            Err(e) if is_absent(&e) => Ok(std::sync::Arc::default()),
            Err(e) => Err(e),
        }
    }

    /// 只留 `keep` 认的路径(全量列会话时修剪掉已经不在的)
    pub fn retain(&self, keep: impl Fn(&std::path::Path) -> bool) {
        self.0.lock().unwrap().retain(|path, _| keep(path));
    }
}

/// 按 mtime 失效的单值缓存(边车映射/SQLite 全表在一轮扫描内的重复调用)。
/// build 返回 None 时不缓存失败结果,mtime 不变也会在下次重试。
pub struct MtimeCache<T: Clone>(std::sync::Mutex<Option<(i64, T)>>);

impl<T: Clone> MtimeCache<T> {
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    pub fn get_or_try_build(&self, mtime: i64, build: impl FnOnce() -> Option<T>) -> Option<T> {
        self.get_or_build_result(mtime, || build().ok_or(())).ok()
    }

    /// 同 `get_or_try_build`,但 build 失败时交回**上一次成功的值**(戳不更新,下次照样
    /// 重试)。给"读不出来也得给个答案"的调用方:ZCode 的隐藏名单与行清单这类接口表达
    /// 不了"不知道",而空答案的后果是全部放出 / 全部删掉(2026-09-21 review),旧答案
    /// 至少是上一次的事实。从没成功过才是 None
    pub fn get_or_stale(&self, mtime: i64, build: impl FnOnce() -> Option<T>) -> Option<T> {
        self.get_or_try_build(mtime, build)
            .or_else(|| self.latest())
    }

    /// 上一次成功读到的值,不看戳、不重读。扫描末尾才问的快照(`parent_links`)要的是这一轮
    /// 枚举入库的那一份:库在扫描期间被写过时按戳重读,只是把整个枚举查询再跑一遍,多读到的
    /// 新会话这一轮也没入库
    pub fn latest(&self) -> Option<T> {
        self.0.lock().unwrap().as_ref().map(|(_, v)| v.clone())
    }

    /// build 带错误:Ok 才缓存,Err 原样带回、不记——"失败不缓存"是这个缓存本来的
    /// 契约,只是多把原因交出去(记忆目录列不出、读不出时 scanner 要靠 Err 跳过该组,
    /// 不能拿到一个空的 Ok);`get_or_try_build` 是它的 Option 壳
    pub fn get_or_build_result<E>(
        &self,
        mtime: i64,
        build: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        {
            let cache = self.0.lock().unwrap();
            if let Some((t, v)) = cache.as_ref() {
                if *t == mtime {
                    return Ok(v.clone());
                }
            }
        }
        let v = build()?;
        *self.0.lock().unwrap() = Some((mtime, v.clone()));
        Ok(v)
    }
}

/// tool_use 块 → ToolCallView(preview + pretty-print input 三件套统一)
/// 工具调用的参数:模型常把它写成 JSON 字符串,解得开就用解开的,解不开(命令行、自由文本)
/// 原样留着
pub fn decoded_arguments(args: &Value) -> std::borrow::Cow<'_, Value> {
    match args.as_str().and_then(|s| serde_json::from_str(s).ok()) {
        Some(parsed) => std::borrow::Cow::Owned(parsed),
        None => std::borrow::Cow::Borrowed(args),
    }
}

pub fn tool_call_view(
    id: String,
    name: &str,
    input: &Value,
    output: Option<String>,
    is_error: bool,
) -> ToolCallView {
    let input_json = if input.is_null() {
        String::new()
    } else {
        serde_json::to_string_pretty(input).unwrap_or_default()
    };
    ToolCallView {
        id,
        name: if name.is_empty() {
            "tool".to_string()
        } else {
            name.to_string()
        },
        input_preview: make_preview(input),
        input: if input_json.is_empty() {
            None
        } else {
            Some(clip(&input_json, MAX_TOOL_IO).0)
        },
        output: output.map(|o| clip(&o, MAX_TOOL_IO).0),
        is_error,
        child_id: None,
    }
}

/// 截断到 max 字符(按 char 边界),返回 (text, truncated)
/// `clip` 的按字符版:预算是字符数——记忆正文多是中文(约 3 字节一字),按字节截会把
/// 9000 字的文件在 6600 字处截断、提示却说"20000 of 9000 characters"(2026-09-22 review)
pub fn clip_chars(s: &str, max_chars: usize) -> (String, bool) {
    match s.char_indices().nth(max_chars) {
        None => (s.to_string(), false),
        Some((end, _)) => (format!("{}\n… (truncated)", &s[..end]), true),
    }
}

pub fn clip(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    // 找不超过 max 的最大 char 边界
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n… (truncated)", &s[..end]), true)
}

/// RFC3339 字符串 → epoch ms,解析失败 = 0
pub fn iso_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// SQLite `datetime('now')` 风格("2026-05-15 06:23:37" 或带 T/毫秒/Z)→ epoch ms(按 UTC)
pub fn sqlite_dt_ms(s: &str) -> i64 {
    let ms = iso_ms(s);
    if ms > 0 {
        return ms;
    }
    let t = s.replace(' ', "T");
    let with_z = if t.ends_with('Z') { t } else { format!("{t}Z") };
    iso_ms(&with_z)
}

/// ISO8601 字符串或 unix 秒/毫秒 → epoch ms
pub fn to_epoch_ms(v: &Value) -> i64 {
    match v {
        Value::Number(n) => epoch_ms(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => iso_ms(s),
        _ => 0,
    }
}

/// unix 秒或毫秒(> 1e12 当毫秒)→ epoch ms;非正数给 0。秒/毫秒的唯一裁定点
pub fn epoch_ms(n: f64) -> i64 {
    if n > 1e12 {
        n as i64
    } else if n > 0.0 {
        (n * 1000.0) as i64
    } else {
        0
    }
}

/// 清洗标题候选:剥 slash-command 壳与 system-reminder 等标签,压单行截断。空串=不可用
pub fn clean_title_candidate(raw: &str) -> String {
    clean_user_text(raw, false)
}

/// 用户那句话折成一行(新会话开场交接里的 "last asked"):与标题同一套清洗,只是 slash 命令
/// 写回用户敲的样子 `/name args`——标题只留参数,交接里要看得出是哪条命令。空串 = 不是人说的
/// 话(只剩 `/model` 一类命令的输出等)
pub fn clean_prompt_line(raw: &str) -> String {
    clean_user_text(raw, true)
}

/// 两者共用的清洗;`whole_command` 定 slash 命令写成 `/name args`,还是只留参数(没有参数退
/// 命令名)
fn clean_user_text(raw: &str, whole_command: bool) -> String {
    let raw = codex_ide_request(raw).unwrap_or(raw);
    let raw = skill_panel_request(raw);
    let mut s = strip_tag_block(&raw, "system-reminder");
    s = strip_tag_block(&s, "local-command-caveat");
    s = strip_tag_block(&s, "local-command-stdout");
    // Kimi 派出去的 explore 子代理收到的任务前面垫着仓库现状
    s = strip_tag_block(&s, "git-context");
    // 千问办公贴的图片与长文本(`<user-pasted-image file=… />`、`<user-pasted-text …>`)带着文件路径
    // 一类长属性,下面去残余短标签那一遍剥不掉;只剥标签,贴的字留着
    s = strip_open_tags(&s, "<user-pasted-");

    let args = extract_tag(&s, "command-args");
    let name = extract_tag(&s, "command-name");
    if args.is_some() || name.is_some() {
        let args = args.filter(|a| !a.trim().is_empty());
        s = match (whole_command, name, args) {
            (true, Some(name), args) => format!("{name} {}", args.unwrap_or_default()),
            (_, name, args) => args.or(name).unwrap_or_default(),
        };
    }
    // 去掉残余短标签
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    closed = true;
                    break;
                }
                if c2 == '\n' || tag.len() > 60 {
                    break;
                }
                tag.push(c2);
            }
            if !closed {
                out.push('<');
                out.push_str(&tag);
            } else {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    crate::text::one_line(&out.replace(IMAGE_PLACEHOLDER, " "), MAX_TITLE)
}

/// Codex IDE 扩展把编辑器上下文(活动文件、打开的标签页、选中的代码)包在提问外面,
/// `## My request for Codex:` 之后才是用户的话。只给标题与交接用:正文原样保留,选中的
/// 代码是用户特意附上的
fn codex_ide_request(text: &str) -> Option<&str> {
    let (_, request) = text
        .trim_start()
        .strip_prefix("# Context from my IDE setup:")?
        .split_once("## My request for Codex:")?;
    Some(request)
}

/// 千问办公从技能面板发起的任务:人打的字前面垫着一段固定的"必须用某个 skill 完成"的指令,选了几个
/// 技能就垫几段。标题与交接只留技能名,写成 `技能 · 那句话`;正文原样保留
fn skill_panel_request(text: &str) -> std::borrow::Cow<'_, str> {
    const LEAD: &str = "This request must be fulfilled using the \"";
    const TAIL: &str = "Do NOT claim this capability is unavailable.";
    let mut skills = Vec::new();
    let mut rest = text.trim_start();
    while let Some(after) = rest.strip_prefix(LEAD) {
        let (Some(name_end), Some(tail)) = (after.find('"'), after.find(TAIL)) else {
            break;
        };
        skills.push(&after[..name_end]);
        rest = after[tail + TAIL.len()..].trim_start();
    }
    if skills.is_empty() {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(format!("{} · {rest}", skills.join(" + ")))
    }
}

/// 去掉以 `prefix` 开头、到下一个 `>` 为止的标签(长度不限),标签外的字留着
fn strip_open_tags(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(prefix) {
        out.push_str(&rest[..start]);
        out.push(' ');
        let Some(end) = rest[start..].find('>') else {
            return out;
        };
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

fn strip_tag_block(s: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find(&open) {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push(' ');
                match rest[i..].find(&close) {
                    None => return out,
                    Some(j) => rest = &rest[i + j + close.len()..],
                }
            }
        }
    }
}

pub(crate) fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let i = s.find(&open)?;
    let j = s[i + open.len()..].find(&close)?;
    Some(s[i + open.len()..i + open.len() + j].to_string())
}

/// `<tag>…</tag>` 的内文(去首尾空白);没有或为空 = None
pub(crate) fn tag_inner(text: &str, tag: &str) -> Option<String> {
    extract_tag(text, tag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 开头那几段 `<system-reminder …>…</system-reminder>`(可以带属性)与其后的话,切成两截。
/// 只切开头的,后面夹着的照旧留在话里;没有闭合标签的不切
fn split_leading_reminders(text: &str) -> (&str, &str) {
    const OPEN: &str = "<system-reminder";
    const CLOSE: &str = "</system-reminder>";
    let text = text.trim_start();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix(OPEN) {
        // 标签名到这里为止,后面是 `>` 或属性,不是名字更长的别的标签
        if !after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
            break;
        }
        let Some(end) = after.find(CLOSE) else {
            break;
        };
        rest = after[end + CLOSE.len()..].trim_start();
    }
    (text[..text.len() - rest.len()].trim_end(), rest)
}

/// 人说的话开头垫着的宿主状态与上下文(Claude 桌面端的 "Linked sessions now…"、CodeBuddy IDE 的
/// 编辑器上下文,都是 `<system-reminder>`)。整条以 `<system-` 开头会被 `is_injected_user_content`
/// 判成注入内容,话跟着不见——切下来单独推一条 Meta(内容照留)进 `messages`,交回剩下的再判;
/// 图片的正文偏移随之平移。开头的图片占位先跳过(`after_image_placeholders`):不然图片在前时,扫描
/// (不解码,图片是 `[image]`)不切、详情(解码,图片没有字)切,两边消息条数对不上、seq 错位。
/// 宿主自己合成的消息开头也垫着它(桌面端的 "Background (from the session that started this
/// one)…"),那种整条照旧是注入内容,调用方别对它调(Claude 系看 `from_human`)
pub fn push_leading_context(
    mut content: ParsedContent,
    ts: i64,
    messages: &mut Vec<TranscriptMessage>,
) -> ParsedContent {
    let rest = after_image_placeholders(&content.text);
    let (lead, said) = split_leading_reminders(rest);
    if lead.is_empty() {
        return content;
    }
    let mut context = text_msg(Role::User, lead, ts);
    context.kind = MessageKind::Meta;
    messages.push(context);
    let start = content.text.len() - rest.len();
    let end = content.text.len() - said.len();
    content.text.replace_range(start..end, "");
    for image in &mut content.images {
        image.text_offset = match image.text_offset {
            offset if offset >= end => offset - (end - start),
            offset => offset.min(start),
        };
    }
    content
}

/// 跳过开头的图片占位。扫描不解码时图片是 `[image]`,详情解码时图片没有字:按开头判断的地方
/// (分类、切上下文)两种解析要判成一样
/// 开头的图片占位与 `<system-reminder>` 之后,人说的那段话(`push_leading_context` 同一个切法)
pub fn after_leading_context(text: &str) -> &str {
    split_leading_reminders(after_image_placeholders(text)).1
}

pub fn after_image_placeholders(text: &str) -> &str {
    let mut text = text.trim_start();
    while let Some(rest) = text.strip_prefix(IMAGE_PLACEHOLDER) {
        text = rest.trim_start();
    }
    text
}

/// Claude Code 系转录(Claude、Qoder)的一行是不是人说的:`turnOrigin` / `origin.kind` 记了 human
/// 以外的来源(桌面端合成的 host_synthetic、经 SDK 塞进来的 sdk)就不是;都没记的(老版本)算人说的
pub fn from_human(row: &Value) -> bool {
    ["/turnOrigin", "/origin/kind"].iter().all(|key| {
        row.pointer(key)
            .and_then(Value::as_str)
            .is_none_or(|origin| origin == "human")
    })
}

/// IDE/CLI 注入型"用户"消息:非用户手打,详情里应归 Meta 折叠,不参与标题推导。
pub fn is_injected_user_content(text: &str) -> bool {
    let t = text.trim_start();
    const PREFIXES: &[&str] = &[
        "<recommended_plugins",
        "<environment_context",
        "<user_action",
        "<user_instructions",
        "<permissions",
        "<workspace",
        "<system-",
        "<context ",
        "<session_context",
        "IMPORTANT: Do NOT read",
        "Caveat: The messages below",
        "# Files pasted by the user",
        "# Files mentioned by the user",
        "## Referenced ChatGPT conversation",
        // Codex 把 AGENTS.md 作为 user 消息注入
        "# AGENTS.md instructions",
        // Claude Code 的后台任务(子代理、后台命令)结束时以 user 行注入的通知,不是人打的字;
        // 不认它的话它会当成提问进 Insights、给分支会话起出 "b69tgbuwc toolu_…" 这种标题
        "<task-notification",
        // 用户按停时 Claude Code(与同一套转录格式的 Qoder、千问办公)补写的那一行:
        // `[Request interrupted by user]` / `… for tool use]`,不带 isMeta,也不是人打的字
        "[Request interrupted by user",
        // Codex 的 subagent / 分支线程:父会话的整段 transcript 被打包成
        // 一条 role=user 消息喂进来,里面含父会话的 assistant 输出。不认它
        // 的话,父会话里 AI 说的话会显示成这个会话里用户发的
        "The following is the Codex agent history",
    ];
    if PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    // Codex 的 skill 展开体:skill-name + SKILL.md 路径 + frontmatter(不带 $,
    // 用户手打的 "$skill" 引用是另一条独立消息,不受影响)。路径子串两种
    // 分隔符都认——Windows 上 Codex 写进日志的是反斜杠路径,只匹配 '/'
    // 会让展开体漏过滤、灌进正文与 FTS(2026-08-25 review)
    t.contains("/.codex/plugins/")
        || t.contains(r"\.codex\plugins\")
        || ((t.contains("/plugins/cache/") || t.contains(r"\plugins\cache\"))
            && t.contains("SKILL.md"))
}

/// tool 输入的单行摘要
pub fn make_preview(input: &Value) -> String {
    const MAX: usize = 200;
    let cand = if let Value::Object(obj) = input {
        [
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
        ]
        .iter()
        .find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
        })
        .map(|s| s.to_string())
        .or_else(|| serde_json::to_string(input).ok())
    } else if let Value::String(s) = input {
        Some(s.clone())
    } else {
        serde_json::to_string(input).ok()
    };
    let one = cand
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = one.chars().collect();
    if chars.len() > MAX {
        let mut t: String = chars[..MAX].iter().collect();
        t.push('…');
        t
    } else {
        one
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 戳没变给缓存;读不出报错、不交回旧内容(旧内容配上新戳入库就再也不会重读);文件不在给空的
    #[test]
    fn stamp_cache_rereads_on_change_and_never_serves_stale_content() {
        let cache: StampCache<String> = StampCache::default();
        let path = std::path::Path::new("/x/assignment.md");
        let reads = std::cell::Cell::new(0);
        let read = |text: &'static str| {
            let reads = &reads;
            move |_: &std::path::Path| {
                reads.set(reads.get() + 1);
                Ok(text.to_string())
            }
        };
        assert_eq!(
            *cache.get(path, Some((1, 5)), read("first")).unwrap(),
            "first"
        );
        assert_eq!(
            *cache.get(path, Some((1, 5)), read("unused")).unwrap(),
            "first"
        );
        assert_eq!(reads.get(), 1, "戳没变不重读");
        let denied = |_: &std::path::Path| -> std::io::Result<String> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        };
        assert!(cache.get(path, Some((2, 9)), denied).is_err());
        let gone = |_: &std::path::Path| -> std::io::Result<String> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        assert_eq!(*cache.get(path, Some((3, 9)), gone).unwrap(), "");
        assert_eq!(*cache.get(path, None, read("unused")).unwrap(), "");
    }

    /// Claude Code 后台任务结束的通知是注入内容,不是提问;正文里提到这个词的照算人话
    #[test]
    fn task_notifications_are_injected_content() {
        assert!(is_injected_user_content(
            "<task-notification>\n<task-id>b69tgbuwc</task-id>\n<status>completed</status>"
        ));
        assert!(!is_injected_user_content(
            "为什么 <task-notification> 会被当成提问?"
        ));
    }

    /// 用户按停时补写的那一行不是提问;正文里引用到它的照算人话
    #[test]
    fn interrupt_markers_are_injected_content() {
        assert!(is_injected_user_content("[Request interrupted by user]"));
        assert!(is_injected_user_content(
            "[Request interrupted by user for tool use]"
        ));
        assert!(!is_injected_user_content(
            "为什么会出现 [Request interrupted by user]?"
        ));
    }

    /// 千问办公:技能面板垫的指令在标题与交接里只留技能名,贴的图片与长文本只剥标签(标签带着长路径,
    /// 去残余短标签那一遍剥不掉)
    #[test]
    fn qwen_work_wrappers_are_trimmed_from_titles() {
        let skill = |name: &str| {
            format!("This request must be fulfilled using the \"{name}\" skill. Load it first. Do NOT claim this capability is unavailable.\n\n")
        };
        assert_eq!(
            clean_title_candidate(&format!("{}把这份合同翻译成英文", skill("pdf"))),
            "pdf · 把这份合同翻译成英文"
        );
        assert_eq!(
            clean_prompt_line(&format!("{}{}合并成一份", skill("pdf"), skill("docx"))),
            "pdf + docx · 合并成一份"
        );
        let long = "a".repeat(80);
        assert_eq!(
            clean_title_candidate(&format!(
                "看看这张图 <user-pasted-image file=\"/Users/tester/{long}.png\" /> 哪里不对"
            )),
            "看看这张图 哪里不对"
        );
        assert_eq!(
            clean_title_candidate(&format!(
                "<user-pasted-text id=\"{long}\">粘贴的长文本</user-pasted-text> 总结一下"
            )),
            "粘贴的长文本 总结一下"
        );
    }

    /// 任务转录在 `transcript/` 里,同名目录在上一层;别的会话文件旁边就是
    #[test]
    fn task_transcripts_keep_their_folder_one_level_up() {
        let p = std::path::Path::new;
        assert_eq!(
            session_dir_of(p("/q/projects/-Users-x/transcript/task-1.jsonl"), "task-1"),
            p("/q/projects/-Users-x/task-1")
        );
        assert_eq!(
            session_dir_of(p("/q/projects/-Users-x/abc.jsonl"), "abc"),
            p("/q/projects/-Users-x/abc")
        );
    }

    #[test]
    fn sidecar_fingerprints_tell_renames_apart() {
        // 按 1024 取模时这两个名字撞在同一个值上(2026-10-11 Codex review)
        assert_ne!(
            sidecar_fingerprint(&["Oscar", ""]),
            sidecar_fingerprint(&["Ada 4", ""])
        );
        // 段界算数:字段之间挪一个字不是同一份
        assert_ne!(
            sidecar_fingerprint(&["ab", "c"]),
            sidecar_fingerprint(&["a", "bc"])
        );
    }

    #[test]
    fn jsonl_lines_stop_on_a_persistent_read_error() {
        // 一直读不出来(EIO、断开的网络盘):交一次 Err 就结束,不打转
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk gone"))
            }
        }
        let mut lines = jsonl_lines(std::io::BufReader::new(Broken));
        assert!(matches!(lines.next(), Some(Err(_))));
        assert!(lines.next().is_none());
        // 坏 UTF-8 的行跳过,接着读
        let data: &[u8] = b"{\"a\":1}\n\xff\xfe\n{\"b\":2}\n";
        let lines: Vec<_> = jsonl_lines(data).map(Result::unwrap).collect();
        assert_eq!(
            lines,
            vec![
                Some("{\"a\":1}".to_string()),
                None,
                Some("{\"b\":2}".to_string())
            ]
        );
        // 解成 JSON:空行跳过,坏行与不是 JSON 的行都交 None
        let data: &[u8] = b"{\"a\":1}\n\n  \nnot json\n\xff\n{\"b\":2}";
        let values: Vec<_> = jsonl_values(data).map(Result::unwrap).collect();
        assert_eq!(
            values,
            vec![
                Some(serde_json::json!({"a": 1})),
                None,
                None,
                Some(serde_json::json!({"b": 2}))
            ]
        );
    }

    #[test]
    fn merging_into_the_last_assistant_caps_text_and_thinking() {
        let content = |text: &str| ParsedContent {
            text: text.to_string(),
            images: Vec::new(),
        };
        let mut messages = vec![text_msg(Role::User, "hi", 1)];
        // 最后一条是用户:先开一条空的助手消息
        assert_eq!(
            merge_into_last_assistant(&mut messages, 0, content(""), "plan"),
            1
        );
        assert_eq!(messages[1].timestamp, None);
        let big = "x".repeat(MAX_MSG_TEXT);
        let long_thinking = "t".repeat(MAX_TOOL_IO);
        for ts in [2, 3] {
            merge_into_last_assistant(&mut messages, ts, content(&big), &long_thinking);
        }
        assert_eq!(messages.len(), 2);
        let reply = &messages[1];
        assert_eq!(reply.timestamp, Some(2), "还没有时间的用第一次带时间的");
        assert!(reply.truncated);
        assert_eq!(reply.text.matches("(truncated)").count(), 1);
        let thinking = reply.thinking.clone().unwrap();
        assert!(thinking.starts_with("plan\n\n"));
        assert_eq!(thinking.matches("(truncated)").count(), 1);
        // 到上限后不再拼接,也不再反复 clip
        merge_into_last_assistant(&mut messages, 4, content(""), "more");
        assert_eq!(messages[1].thinking.as_deref(), Some(thinking.as_str()));
    }

    #[test]
    fn only_leading_reminders_are_split_off() {
        let attr = "<system-reminder data-role=\"user-context\">open file</system-reminder>";
        for (text, context, said) in [
            (&*format!("{attr}\n问题\n  第二行"), attr, "问题\n  第二行"),
            (
                "<system-reminder>a</system-reminder> <system-reminder>b</system-reminder>x",
                "<system-reminder>a</system-reminder> <system-reminder>b</system-reminder>",
                "x",
            ),
            (
                "<system-reminder>only</system-reminder>",
                "<system-reminder>only</system-reminder>",
                "",
            ),
            // 后面夹着的、没闭合的、名字更长的别的标签都不动
            (
                "问题 <system-reminder>c</system-reminder>",
                "",
                "问题 <system-reminder>c</system-reminder>",
            ),
            ("<system-reminder>unclosed", "", "<system-reminder>unclosed"),
            (
                "<system-reminders>x</system-reminders>",
                "",
                "<system-reminders>x</system-reminders>",
            ),
        ] {
            assert_eq!(split_leading_reminders(text), (context, said), "{text}");
        }
    }

    /// 切下来的上下文成一条 Meta,剩下的话交回、图片的正文偏移跟着平移;没有上下文时原样交回
    #[test]
    fn leading_context_becomes_its_own_meta_message() {
        let mut content = ParsedContent::default();
        content.push_text("<system-reminder>status</system-reminder>");
        content.push_image(ImageAttachment {
            media_type: "image/png".into(),
            bytes: vec![1],
            text_offset: 0,
        });
        content.push_text("看下部署成功没有");
        let mut messages = Vec::new();
        let said = push_leading_context(content, 7, &mut messages);
        assert_eq!(said.text, "看下部署成功没有");
        assert_eq!(said.images[0].text_offset, 0);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MessageKind::Meta);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(
            messages[0].text,
            "<system-reminder>status</system-reminder>"
        );
        assert_eq!(messages[0].timestamp, Some(7));

        let mut plain = ParsedContent::default();
        plain.push_text("问题");
        let said = push_leading_context(plain, 7, &mut messages);
        assert_eq!(said.text, "问题");
        assert_eq!(messages.len(), 1);
    }

    /// 图片在前:不解码(扫描,图片是 `[image]`)与解码(详情,图片没有字)切法一样,上下文都单独
    /// 一条,剩下的话里图片占位或图片照留
    #[test]
    fn leading_context_ignores_leading_image_placeholders() {
        let text = "<system-reminder>status</system-reminder>\n截图里这个按钮对吗";
        let mut scanned = ParsedContent::default();
        scanned.push_text(IMAGE_PLACEHOLDER);
        scanned.push_text(text);
        let mut shown = ParsedContent::default();
        shown.push_image(ImageAttachment {
            media_type: "image/png".into(),
            bytes: vec![1],
            text_offset: 0,
        });
        shown.push_text(text);

        let (mut a, mut b) = (Vec::new(), Vec::new());
        let scanned = push_leading_context(scanned, 1, &mut a);
        let shown = push_leading_context(shown, 1, &mut b);
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_eq!(a[0].text, b[0].text);
        assert_eq!(scanned.text, "[image]\n\n截图里这个按钮对吗");
        assert_eq!(shown.text, "截图里这个按钮对吗");
        assert_eq!(shown.images[0].text_offset, 0);
    }

    #[test]
    fn rows_are_human_unless_an_origin_says_otherwise() {
        for (row, human) in [
            (serde_json::json!({}), true),
            (
                serde_json::json!({"turnOrigin": "human", "origin": {"kind": "human"}}),
                true,
            ),
            (serde_json::json!({"turnOrigin": "host_synthetic"}), false),
            (serde_json::json!({"turnOrigin": "sdk"}), false),
            (
                serde_json::json!({"origin": {"kind": "task-notification"}}),
                false,
            ),
        ] {
            assert_eq!(from_human(&row), human, "{row}");
        }
    }

    #[test]
    fn percent_decode_walks_bytes() {
        assert_eq!(percent_decode("/Users/a%20b/%E4%B8%AD"), "/Users/a b/中");
        // 不成对的 `%`、后面跟多字节字符的 `%` 原样留着,不 panic
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%中%z9"), "%中%z9");
    }

    /// Windows 形状的项目路径换成别家记的写法:盘符大写、Windows 上正斜杠换反斜杠;
    /// POSIX 路径(含远程镜像来的)原样
    #[test]
    fn project_paths_are_canonical() {
        assert_eq!(canonical_project_path("/Users/x/app"), "/Users/x/app");
        assert_eq!(canonical_project_path(r"c:\Users\x\app"), r"C:\Users\x\app");
        if cfg!(windows) {
            assert_eq!(canonical_project_path("c:/Users/x/app"), r"C:\Users\x\app");
            assert_eq!(
                canonical_project_path("//nas/share/app"),
                r"\\nas\share\app"
            );
        } else {
            assert_eq!(canonical_project_path("c:/Users/x/app"), "C:/Users/x/app");
            assert_eq!(canonical_project_path("//nas/share/app"), "//nas/share/app");
        }
    }

    #[test]
    fn clip_chars_counts_characters() {
        let s = "记忆正文一共九个字";
        assert_eq!(clip_chars(s, 9), (s.to_string(), false));
        let (cut, truncated) = clip_chars(s, 4);
        assert!(truncated);
        assert!(cut.starts_with("记忆正文\n…"), "{cut}");
    }

    const IMAGE_SHAPES: &str = include_str!("../../tests/fixtures/image_content_shapes.json");

    #[test]
    fn readable_agent_image_shapes_decode_only_for_transcripts() {
        let fixtures: Vec<Value> = serde_json::from_str(IMAGE_SHAPES).expect("image fixtures");
        assert_eq!(fixtures.len(), 7);

        for fixture in fixtures {
            let name = fixture["name"].as_str().expect("fixture name");
            let content = &fixture["content"];

            let indexed = content_parts(content, false);
            assert_eq!(indexed.text, IMAGE_PLACEHOLDER, "index shape {name}");
            assert!(indexed.images.is_empty(), "index bytes {name}");

            let transcript = content_parts(content, true);
            assert!(transcript.text.is_empty(), "transcript text {name}");
            assert_eq!(transcript.images.len(), 1, "transcript image {name}");
            assert_eq!(transcript.images[0].media_type, "image/png", "mime {name}");
            assert!(
                transcript.images[0].bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "png bytes {name}"
            );
        }
    }

    #[test]
    fn image_parser_is_bounded_and_never_fetches_remote_urls() {
        let remote = serde_json::json!({
            "type": "image",
            "image_url": "https://example.com/private.png"
        });
        let parsed = content_parts(&remote, true);
        assert_eq!(parsed.text, IMAGE_PLACEHOLDER);
        assert!(parsed.images.is_empty());

        let non_image = serde_json::json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="}
        });
        assert!(!is_image_part(&non_image));
        let parsed = content_parts(&non_image, true);
        assert!(parsed.text.is_empty());
        assert!(parsed.images.is_empty());

        let oversized = format!("data:image/png;base64,{}", "A".repeat(MAX_IMAGE_B64 + 1));
        assert!(decode_data_uri(&oversized).is_none());
    }

    #[test]
    fn transcript_image_budget_is_aggregate_and_restores_after_scope() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let image = serde_json::json!({
            "type": "image",
            "media_type": "image/png",
            "data": png
        });
        {
            let _budget = image_decode_budget(Some(100));
            assert_eq!(content_parts(&image, true).images.len(), 1);
            let rejected = content_parts(&image, true);
            assert!(rejected.images.is_empty());
            assert_eq!(rejected.text, IMAGE_PLACEHOLDER);
        }
        assert_eq!(content_parts(&image, true).images.len(), 1);
    }

    #[test]
    fn image_parser_handles_wrappers_mimeless_data_and_tool_fallbacks() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let wrapped = serde_json::json!({
            "content": {"type": "image", "data": png}
        });
        let parsed = content_parts(&wrapped, true);
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].media_type, "image/png");

        let object = serde_json::json!({"exit_code": 0, "duration_ms": 12});
        let parsed = tool_result_parts(&object, true);
        assert!(parsed.images.is_empty());
        let expected: serde_json::Value =
            serde_json::from_str(r#"{"duration_ms":12,"exit_code":0}"#).unwrap();
        let actual: serde_json::Value = serde_json::from_str(&parsed.text).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn image_parser_preserves_text_image_order() {
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let content = serde_json::json!([
            {"type": "text", "text": "前面的说明"},
            {"type": "image", "media_type": "image/png", "data": png},
            {"type": "text", "text": "后面的说明"}
        ]);
        let parsed = content_parts(&content, true);
        assert_eq!(parsed.text, "前面的说明\n\n后面的说明");
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].text_offset, "前面的说明".len());
    }

    #[test]
    fn image_parser_never_dereferences_local_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("secret.png");
        std::fs::write(&path, b"not really an image").expect("fixture");
        let content = serde_json::json!({
            "type": "image",
            "media_type": "image/png",
            "path": path
        });
        let parsed = content_parts(&content, true);
        assert_eq!(parsed.text, IMAGE_PLACEHOLDER);
        assert!(parsed.images.is_empty());
    }

    #[test]
    fn codex_wrappers_keep_only_the_user_request() {
        let wrapped = concat!(
            "# Files mentioned by the user:\n\n",
            "## Screenshot.png: /tmp/Screenshot.png\n\n",
            "## My request:\n",
            "看看这里 <image name=[Image #1] path=\"/tmp/missing.png\">ignored</image> 怎么了"
        );
        let request = unwrap_file_preamble(wrapped).expect("wrapped request");
        assert_eq!(strip_image_tags(&request), "看看这里  怎么了");
        // IDE 扩展的编辑器上下文只在标题与交接里拆掉;正文不经这里,选中的代码还在
        let ide = concat!(
            "# Context from my IDE setup:\n\n",
            "## Active file: src/login.rs\n\n",
            "## Active selection of the file:\nfn login() {}\n\n",
            "## My request for Codex:\n",
            "修一下登录按钮\n"
        );
        assert_eq!(unwrap_file_preamble(ide), None);
        assert_eq!(clean_title_candidate(ide), "修一下登录按钮");
        assert_eq!(clean_prompt_line(ide), "修一下登录按钮");
        let ordinary = "为什么 <image src=\"avatar.png\"></image> 没有显示？";
        assert_eq!(strip_image_tags(ordinary), ordinary);
        assert_eq!(clean_title_candidate("[image] 看看这里"), "看看这里");
    }

    #[test]
    fn title_skips_pure_image_messages() {
        let mut image = text_msg(Role::User, IMAGE_PLACEHOLDER, 1);
        image.images.push(ImageAttachment {
            media_type: "image/png".to_string(),
            bytes: vec![1],
            text_offset: 0,
        });
        let text = text_msg(Role::User, "后面的真实问题", 2);
        assert_eq!(
            title_from_messages(&[image, text]).as_deref(),
            Some("后面的真实问题")
        );
    }
}
