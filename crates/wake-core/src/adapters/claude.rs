use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    /// 指的是写下会话的那台机器上的路径,自定义根(别处拷来的、远程镜像)反推不得;
    /// 文件的创建时间也只在这里可信(分支识别)
    local: bool,
    /// auto-memory 文档,按目录指纹缓存(每轮扫描都会列,指纹没变不重读)
    memories: super::MemoryCache,
    /// 目录名 → 反推出的项目路径,进程内记住(含反推失败):反推要从 `/` 逐层
    /// read_dir + stat,答案又几乎不变,每轮扫描重走一遍是白费;目录后来才出现的
    /// 极少数情况重启即可
    decoded: Mutex<HashMap<String, Option<String>>>,
    /// 分支识别的缓存(见 `BranchCache`)
    branches: Mutex<BranchCache>,
}

// ---------------- 分支(续接)识别 ----------------
//
// Claude Code 从一段已有对话分出新会话(桌面端的分支、`--fork-session`)时,会新建一份
// 会话文件、把原对话到分叉点为止的行**原样复制**进去:uuid、message.id、时间戳都不变,
// 只改写 sessionId。原样入库就是同一段对话列两遍、搜一次命中两次、Insights 的 prompt 与
// token 多算(本机 159 份里 24 份带着复制来的历史,多算约 5%)。做法照 Craft 分支与 Codex
// fork 的先例:同一项目目录里,第一条带 uuid 的行相同的文件是同一棵对话树;树里写下得
// 更晚的那份是分支,它开头连续落在更早那些文件 uuid 集合里的行折成一条 Meta,分支挂到
// 最早那份(原件)下面。

/// 一份会话文件开头的身份,写下后就不变(Claude 只往后追加)。`root` 是第一条带 uuid 的
/// 行(整棵对话树的根,复制时原样带过来);`first_ts` 是文件里第一个时间戳——原件是会话
/// 开始,分支是复制发生的时刻(它头几行的 queue-operation 写在复制之前),所以分支恒不早于
/// 原件;`born` 是文件的创建时间,两份的第一个时间戳一样时(分支没有那几行头)才拿来比
#[derive(Clone)]
struct Head {
    root: Option<String>,
    first_ts: i64,
    born: Option<i64>,
}

/// 一份分支:同根组里比它早写下的文件,以及其中最早的那份(原件)
#[derive(Clone)]
struct Branch {
    earlier: Vec<PathBuf>,
    original: PathBuf,
}

/// 整份文件的 uuid 集合与最后一条自定义标题。只给分支之前的那几份读(绝大多数会话没有
/// 分支,一份都不用读)
#[derive(Clone)]
struct Lineage {
    uuids: Arc<HashSet<String>>,
    custom_title: Option<String>,
}

/// 一个项目目录的分支分组,按目录 mtime 校验:文件进出、改名都会改它;文件追加不会,也
/// 不影响分组(头写下就不变)。根那一行还没写的文件(刚建、头不全)让这份分组不作数,
/// 下次重算;已经读全的头留着复用,目录变了也只读新来的文件
struct DirBranches {
    mtime: i64,
    complete: bool,
    heads: HashMap<PathBuf, Head>,
    branches: Arc<HashMap<PathBuf, Branch>>,
}

#[derive(Default)]
struct BranchCache {
    dirs: HashMap<PathBuf, DirBranches>,
    /// 文件 → ((mtime, size), uuid 集合)
    lineages: HashMap<PathBuf, ((i64, i64), Lineage)>,
}

/// 一份分支会话的继承段:比它早写下的同根文件里出现过的 uuid。文件开头连续落在这些集合
/// 里的行是从原件复制来的
struct Inherited {
    earlier: Vec<Lineage>,
    /// 原件最后一条自定义标题:分支的自定义标题跟它一样(分支时照抄过来)就不当标题用,
    /// 改用分支自己的第一句话——否则父子两行标题一模一样,看不出分支在问什么
    parent_title: Option<String>,
}

impl Inherited {
    fn contains(&self, uuid: &str) -> bool {
        self.earlier.iter().any(|l| l.uuids.contains(uuid))
    }
}

/// `a` 是否严格早于 `b` 写下:先比文件里第一个时间戳;一样时只在 `trust_born`(默认根,
/// 本机自己写下的文件)上再比创建时间——远程镜像与拷贝来的目录里,创建时间是同步 / 拷贝
/// 的时刻,宁可不判也不瞎判。没有时间戳的文件不参与排序
fn written_before(a: &Head, b: &Head, trust_born: bool) -> bool {
    if a.first_ts <= 0 || b.first_ts <= 0 {
        return false;
    }
    if a.first_ts != b.first_ts {
        return a.first_ts < b.first_ts;
    }
    trust_born && matches!((a.born, b.born), (Some(x), Some(y)) if x < y)
}

/// 按根分组,算出每份分支之前有哪些同根文件、最早的是哪份(并列时取路径字典序小的,
/// 保证稳定)。没有更早文件的(原件、和原件分不出先后的)不在结果里
fn group_branches(heads: &HashMap<PathBuf, Head>, trust_born: bool) -> HashMap<PathBuf, Branch> {
    let mut trees: HashMap<&str, Vec<(&PathBuf, &Head)>> = HashMap::new();
    for (path, head) in heads {
        if let Some(root) = &head.root {
            trees.entry(root).or_default().push((path, head));
        }
    }
    let mut out = HashMap::new();
    for tree in trees.values().filter(|tree| tree.len() > 1) {
        for &(path, head) in tree {
            let earlier: Vec<(&PathBuf, &Head)> = tree
                .iter()
                .copied()
                .filter(|&(other, h)| other != path && written_before(h, head, trust_born))
                .collect();
            let original = earlier
                .iter()
                .filter(|(_, h)| {
                    !earlier
                        .iter()
                        .any(|(_, o)| written_before(o, h, trust_born))
                })
                .map(|(p, _)| *p)
                .min();
            if let Some(original) = original {
                out.insert(
                    path.clone(),
                    Branch {
                        earlier: earlier.iter().map(|(p, _)| (*p).clone()).collect(),
                        original: original.clone(),
                    },
                );
            }
        }
    }
    out
}

/// 读文件头:第一个时间戳与第一条带 uuid 的行。头几行都是很短的元数据行,找到根就停
fn read_head(path: &Path, born: Option<i64>) -> Head {
    #[derive(Deserialize)]
    struct Row {
        uuid: Option<String>,
        timestamp: Option<Value>,
    }
    let mut head = Head {
        root: None,
        first_ts: 0,
        born,
    };
    let Ok(file) = fs::File::open(path) else {
        return head;
    };
    for row in jsonl_rows::<Row, _>(BufReader::new(file)).take(200) {
        let Ok(row) = row else { break };
        let Some(row) = row else { continue };
        if head.first_ts == 0 {
            head.first_ts = row.timestamp.as_ref().map(to_epoch_ms).unwrap_or(0);
        }
        if let Some(uuid) = row.uuid.filter(|u| !u.is_empty()) {
            head.root = Some(uuid);
            break;
        }
    }
    head
}

/// 读整份文件的 uuid 集合与最后一条自定义标题。逐行只反序列化这三个字段;坏行跳过,
/// 读错误整份失败(读不全就宁可不折)
fn read_lineage(path: &Path) -> Result<Lineage> {
    #[derive(Deserialize)]
    struct Row {
        uuid: Option<String>,
        #[serde(rename = "type")]
        typ: Option<String>,
        #[serde(rename = "customTitle")]
        custom_title: Option<Value>,
    }
    let mut uuids = HashSet::new();
    let mut title = None;
    for row in jsonl_rows::<Row, _>(BufReader::with_capacity(1 << 20, fs::File::open(path)?)) {
        let Some(row) = row? else { continue };
        if let Some(uuid) = row.uuid.filter(|u| !u.is_empty()) {
            uuids.insert(uuid);
        }
        // 与解析同一个取法(optional_string),分支标题与原件标题才比得上
        if row.typ.as_deref() == Some("custom-title") {
            title = optional_string(row.custom_title.as_ref()).or(title);
        }
    }
    Ok(Lineage {
        uuids: Arc::new(uuids),
        custom_title: title,
    })
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default().join(".claude");
        Self::at(home.join("projects"), Some(home), true)
    }

    fn at(root: PathBuf, home: Option<PathBuf>, local: bool) -> Self {
        Self {
            root,
            home,
            local,
            memories: super::MemoryCache::new(),
            decoded: Mutex::new(HashMap::new()),
            branches: Mutex::new(BranchCache::default()),
        }
    }

    /// 一个项目目录里的分支(见 `DirBranches`)。目录没变就直接用上次的分组,一次 stat;
    /// 变了只读新来文件的头。文件 I/O 不持锁——扫描、watcher、GUI 打开详情、导出可能同时问
    fn branches_in(&self, dir: &Path) -> Arc<HashMap<PathBuf, Branch>> {
        let mtime = fs::metadata(dir).map(|m| mtime_ms(&m)).unwrap_or(0);
        let known = {
            let cache = self.branches.lock().unwrap();
            match cache.dirs.get(dir) {
                Some(memo) if memo.mtime == mtime && memo.complete => return memo.branches.clone(),
                Some(memo) => memo.heads.clone(),
                None => HashMap::new(),
            }
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return Arc::default();
        };
        let mut heads = HashMap::new();
        // 建好了、还没写下第一行的会话文件(零字节)不进分组,但让这次分组不作数:之后的写入
        // 不改目录 mtime,当成完整缓存起来就再也看不见它,连 ⌘R 都救不回
        let mut unwritten = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let head = match known.get(&path) {
                Some(head) => head.clone(),
                None => {
                    if default_file_ref(AgentId::ClaudeCode, &path).is_none() {
                        unwritten |= path.extension().is_some_and(|ext| ext == "jsonl")
                            && fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == 0);
                        continue;
                    }
                    let born = entry.metadata().ok().and_then(|m| created_ms(&m));
                    read_head(&path, born)
                }
            };
            heads.insert(path, head);
        }
        let complete = !unwritten && heads.values().all(|head| head.root.is_some());
        let branches = Arc::new(group_branches(&heads, self.local));
        heads.retain(|_, head| head.root.is_some());
        self.branches.lock().unwrap().dirs.insert(
            dir.to_path_buf(),
            DirBranches {
                mtime,
                complete,
                heads,
                branches: branches.clone(),
            },
        );
        branches
    }

    /// 会话引用:`default_file_ref` 再把"之前有几份同根文件"加进 size。原件被删(Claude 的
    /// cleanupPeriodDays 清掉了它)或冒出更早的同根文件时,分支的引用随之变化、下一轮扫描
    /// 重解析——折叠只在原件还在时才成立,原件没了分支就该露出完整历史(file_ref 的 trait
    /// 约定:引用的戳要盖住解析读到的每个文件)。列表与 file_ref 共用
    fn session_ref(
        &self,
        path: &Path,
        branches: &HashMap<PathBuf, Branch>,
    ) -> Option<SessionFileRef> {
        let mut r = default_file_ref(AgentId::ClaudeCode, path)?;
        r.size += branches.get(path).map_or(0, |b| b.earlier.len() as i64);
        Some(r)
    }

    /// 一份文件的 uuid 集合(按 mtime + size 缓存;读文件不持锁)
    fn lineage(&self, path: &Path) -> Result<Lineage> {
        let meta = fs::metadata(path)?;
        let stamp = (mtime_ms(&meta), meta.len() as i64);
        if let Some((cached, lineage)) = self.branches.lock().unwrap().lineages.get(path) {
            if *cached == stamp {
                return Ok(lineage.clone());
            }
        }
        let lineage = read_lineage(path)?;
        self.branches
            .lock()
            .unwrap()
            .lineages
            .insert(path.to_path_buf(), (stamp, lineage.clone()));
        Ok(lineage)
    }

    /// `file` 是分支的话,它的继承段与原件的标题;不是分支给 None。更早的文件读不出来就报错,
    /// 不能当成"不是分支"照常入库:等它读得出来,分支的引用没变、扫描不会重来,索引停在没
    /// 折叠的那份,现场解析却折叠了,两边的 seq 对不上(不变量 1)。解析失败时库里的旧行
    /// 留着,下一轮再试
    fn inherited_for(&self, file: &Path) -> Result<Option<Inherited>> {
        let Some(dir) = file.parent() else {
            return Ok(None);
        };
        let branches = self.branches_in(dir);
        let Some(branch) = branches.get(file) else {
            return Ok(None);
        };
        let earlier = branch
            .earlier
            .iter()
            .map(|path| {
                self.lineage(path).with_context(|| {
                    format!("reading the session it branched from: {}", path.display())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let parent_title = self.lineage(&branch.original)?.custom_title;
        Ok(Some(Inherited {
            earlier,
            parent_title,
        }))
    }

    /// 主会话的解析(parse_session 与 parse_transcript 共用):分支先拿继承段
    fn parse_main(&self, r: &SessionFileRef, decode_images: bool) -> Result<ParseResult> {
        let path = Path::new(&r.file_path);
        let inherited = self.inherited_for(path)?;
        parse_claude_jsonl(path, false, decode_images, inherited.as_ref())
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

/// 这一行是不是 pending 那条助手消息的续写:同一个 message.id,或者这一行 / pending 没有
/// id(逐块落盘的中间行)。助手消息的合并与分支边界的判断共用这一条规则
fn continues_pending(pending: &Option<PendingAssistant>, msg_id: Option<&str>) -> bool {
    match (pending, msg_id) {
        (None, _) => false,
        (Some(p), Some(mid)) => p.msg_id.as_deref().is_none_or(|pid| pid == mid),
        (Some(_), None) => true,
    }
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

/// `inherited`:这份是分支时它从原件复制来的 uuid(见 `ClaudeAdapter::inherited_for`)。
/// 文件开头连续落在集合里的行折成一条 Meta(`collapse_inherited`),token、创建时间与
/// 兜底标题只算分支自己的行。parse_session 与 parse_transcript 都经这里,seq 两边一致
fn parse_claude_jsonl(
    path: &Path,
    include_sidechain: bool,
    decode_images: bool,
    inherited: Option<&Inherited>,
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
    // 分支:继承段在第几条消息处结束(None = 还在继承段里,或者不是分支);分支自己的
    // 第一个时间戳
    let mut cut: Option<usize> = None;
    let mut own_created_at: i64 = 0;

    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            unknown_lines += 1;
            continue;
        };
        let typ = row.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // 继承段的边界:第一条 uuid 不在更早文件里的行(attachment 这类跳过的行也带 uuid,
        // 必须在类型过滤之前判)。边界行若是 pending 那条助手消息的续写,那条消息算分支
        // 自己的;否则先把继承段里还没落下的助手消息落下,再记边界
        if let Some(inh) = inherited.filter(|_| cut.is_none()) {
            let uuid = row.get("uuid").and_then(|v| v.as_str());
            if uuid.is_some_and(|uuid| !inh.contains(uuid)) {
                let msg_id = row.pointer("/message/id").and_then(|v| v.as_str());
                if !(typ == "assistant" && continues_pending(&pending, msg_id)) {
                    flush_assistant(&mut pending, &mut messages, &mut tool_index);
                }
                cut = Some(messages.len());
            }
        }
        let in_prefix = inherited.is_some() && cut.is_none();
        if !in_prefix && own_created_at == 0 {
            own_created_at = row.get("timestamp").map(to_epoch_ms).unwrap_or(0);
        }

        if typ == "custom-title" {
            if let Some(t) = optional_string(row.get("customTitle")) {
                custom_title = t;
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
            // 分支的兜底标题取它自己的第一句,不拿原件开头那句
            if kind == MessageKind::Text && fallback_title.is_empty() && !in_prefix {
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
        if !continues_pending(&pending, msg_id.as_deref()) {
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
        // 复制来的助手消息带着原件那次调用的 usage,算进分支就重复计了
        if let Some(usage) = message.get("usage").filter(|_| !in_prefix) {
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
    if let Some(inh) = inherited {
        // 整份都是复制来的(分出来还没说话就关了)也折,只剩那条 Meta
        let cut = cut.unwrap_or(messages.len());
        collapse_inherited(&mut messages, cut, "session");
        if own_created_at > 0 {
            created_at = own_created_at;
        }
        if inh.parent_title.as_deref() == Some(custom_title.as_str()) {
            custom_title.clear();
        }
    }
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
            // 引用要现 stat(mtime / size 是扫描的变更判据);分支分组每个目录取一次
            let dir = project.path();
            let branches = self.branches_in(&dir);
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            refs.extend(
                entries
                    .flatten()
                    .filter_map(|entry| self.session_ref(&entry.path(), &branches)),
            );
        }
        Ok(refs)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = self.parse_main(r, false)?;
        let meta = build_meta(r, &parsed);
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = self.parse_main(r, true)?;
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
        let parsed = parse_claude_jsonl(&file, true, true, None)?;
        Ok(parsed.messages)
    }

    /// 项目目录里的会话文件进出(原件被删、被放回)会改变同目录分支的引用(`session_ref`
    /// 数着更早的同根文件):交出同目录的其余会话文件,watcher 按引用重比、变了的重解析
    fn ref_dependents(&self, path: &Path) -> Vec<PathBuf> {
        let Some(dir) = path
            .parent()
            .filter(|dir| dir.parent() == Some(self.root.as_path()))
        else {
            return Vec::new();
        };
        let jsonl = |p: &Path| p.extension().is_some_and(|ext| ext == "jsonl");
        if !jsonl(path) {
            return Vec::new();
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|sibling| sibling != path && jsonl(sibling))
            .collect()
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // subagents 转录与 memory 边车不是主会话。按所在目录的名字判:按路径分段,Windows 的
        // 反斜杠路径一样认(原先拿带 `/` 的字符串去比,watcher 报来的路径一条都对不上);只看
        // 直接所在的目录,数据根上层恰好有叫 memory / subagents 的目录也不误伤
        let dir = path.parent()?;
        let parent = dir.file_name()?;
        if parent == "subagents" || parent == "memory" {
            return None;
        }
        self.session_ref(path, &self.branches_in(dir))
    }

    /// 分支挂到原件下面(见本文件「分支识别」)。关系由分支自己的文件头与同目录里更早的
    /// 同根文件推出,所以算"写在子会话里"(多 location 下按子会话的胜出副本认边)
    fn manages_parent_links(&self) -> bool {
        true
    }

    fn parent_links_in_child(&self) -> bool {
        true
    }

    /// 分支是自己完整的一段对话:删原件不带上它们,原件没了它们变回顶层会话、露出完整历史
    /// (重解析由 watcher 经 `ref_dependents` 触发)
    fn children_outlive_parent(&self) -> bool {
        true
    }

    /// 全量快照只用文件头(不读 uuid 集合):每个分支 → 它所在同根组里最早的那份。分支
    /// 的分支也直接挂到原件上——scanner 本来就把链压平到根。目录没变的,分组直接复用
    /// (每批 watcher 事件都会问一遍,每个目录一次 stat)。projects 目录不在 = 确定没有
    /// 关系;在却列不出来是"不知道",交 None 让 scanner 保留库里的关系
    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        let projects = match fs::read_dir(&self.root) {
            Ok(projects) => projects,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
            Err(_) => return None,
        };
        let key = |path: &Path| {
            path.file_stem()
                .map(|s| session_key(AgentId::ClaudeCode, "", &s.to_string_lossy()))
        };
        let mut links = Vec::new();
        for project in projects.flatten() {
            for (path, branch) in self.branches_in(&project.path()).iter() {
                if let (Some(child), Some(parent)) = (key(path), key(&branch.original)) {
                    links.push((child, parent));
                }
            }
        }
        Some(links)
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
        Box::new(Self::at(root, home, false))
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(root: &str, first_ts: i64, born: Option<i64>) -> Head {
        Head {
            root: Some(root.to_string()),
            first_ts,
            born,
        }
    }

    /// 谁先写下:先比文件里第一个时间戳;一样(分支没有那几行头)时只在可信的默认根上
    /// 比创建时间;没有时间戳的不参与排序
    #[test]
    fn branch_order_uses_the_first_timestamp_then_trusted_creation_time() {
        let original = head("r", 100, Some(5_000));
        let with_header = head("r", 200, Some(1_000));
        assert!(written_before(&original, &with_header, false));
        assert!(!written_before(&with_header, &original, false));

        let no_header = head("r", 100, Some(9_000));
        assert!(
            !written_before(&original, &no_header, false),
            "镜像 / 拷贝目录里不信创建时间"
        );
        assert!(written_before(&original, &no_header, true));
        assert!(!written_before(&no_header, &original, true));

        let untimed = head("r", 0, Some(1));
        assert!(!written_before(&untimed, &original, true));
        assert!(!written_before(&original, &untimed, true));
    }

    /// 同根组里每份分支都挂到最早那份上(分支的分支也是);别的根、分不出先后的不算
    #[test]
    fn every_branch_points_at_the_earliest_file_of_its_tree() {
        let heads: HashMap<PathBuf, Head> = [
            ("/p/a.jsonl", head("r", 100, None)),
            ("/p/b.jsonl", head("r", 200, None)),
            ("/p/c.jsonl", head("r", 300, None)),
            ("/p/x.jsonl", head("other", 50, None)),
            ("/p/tie.jsonl", head("r", 100, None)),
        ]
        .into_iter()
        .map(|(path, head)| (PathBuf::from(path), head))
        .collect();
        let branches = group_branches(&heads, false);
        let of = |name: &str| {
            branches
                .get(Path::new(name))
                .map(|b| (b.earlier.len(), b.original.clone()))
        };
        assert_eq!(of("/p/a.jsonl"), None);
        assert_eq!(of("/p/tie.jsonl"), None, "和原件分不出先后就不判");
        assert_eq!(of("/p/x.jsonl"), None);
        assert_eq!(of("/p/b.jsonl"), Some((2, PathBuf::from("/p/a.jsonl"))));
        assert_eq!(of("/p/c.jsonl"), Some((3, PathBuf::from("/p/a.jsonl"))));
    }

    /// 项目目录里的一份会话:一行 queue-operation 定下"什么时候写下",一条共有的提问(根
    /// uuid 相同),分支再加一条自己的。返回 (目录, 原件, 分支)
    fn stage_branch(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let dir = root.join("-w");
        fs::create_dir_all(&dir).unwrap();
        let session = |id: &str, opened: &str, own: Option<&str>| {
            let mut lines = vec![
                format!(
                    r#"{{"type":"queue-operation","timestamp":"{opened}","sessionId":"{id}"}}"#
                ),
                format!(
                    r#"{{"type":"user","uuid":"u1","timestamp":"2026-09-01T08:00:01.000Z","sessionId":"{id}","message":{{"role":"user","content":"why does login time out"}}}}"#
                ),
            ];
            if let Some(own) = own {
                lines.push(format!(
                    r#"{{"type":"user","uuid":"b2","timestamp":"2026-09-02T09:00:10.000Z","sessionId":"{id}","message":{{"role":"user","content":"{own}"}}}}"#
                ));
            }
            lines.join("\n") + "\n"
        };
        let (original, branch) = (dir.join("a.jsonl"), dir.join("b.jsonl"));
        fs::write(&original, session("a", "2026-09-01T08:00:00.000Z", None)).unwrap();
        fs::write(
            &branch,
            session("b", "2026-09-02T09:00:00.000Z", Some("use a queue")),
        )
        .unwrap();
        (dir, original, branch)
    }

    /// 建好了、还没写下第一行的会话文件(零字节)不进分组,分组却不能记成完整:之后的写入
    /// 不改目录 mtime,缓存成完整就再也认不出它是分支
    #[test]
    fn an_unwritten_session_file_keeps_the_grouping_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, _, branch) = stage_branch(tmp.path());
        let content = fs::read(&branch).unwrap();
        fs::write(&branch, "").unwrap();
        let adapter = ClaudeAdapter::at(tmp.path().to_path_buf(), None, false);
        assert!(adapter.branches_in(&dir).is_empty());
        fs::write(&branch, content).unwrap();
        assert!(
            adapter.branches_in(&dir).contains_key(&branch),
            "写下第一行以后要认出分支"
        );
    }

    /// 已经认出是分支、原件却读不出来(权限、I/O 错误):这次解析失败,不能当成"不是分支"
    /// 交出完整历史——那样入了库,读得出来以后引用不变、不会重来,索引与现场解析的 seq
    /// 就一直对不上。读得出来就照常折叠
    #[cfg(unix)]
    #[test]
    fn a_branch_whose_original_cannot_be_read_fails_to_parse() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (dir, original, branch) = stage_branch(tmp.path());
        let adapter = ClaudeAdapter::at(tmp.path().to_path_buf(), None, false);
        assert!(adapter.branches_in(&dir).contains_key(&branch));
        let r = default_file_ref(AgentId::ClaudeCode, &branch).unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&original).is_ok() {
            return; // root 读得动任何文件,这条测不了
        }
        assert!(adapter.parse_main(&r, false).is_err());
        fs::set_permissions(&original, fs::Permissions::from_mode(0o644)).unwrap();
        let parsed = adapter.parse_main(&r, false).unwrap();
        assert_eq!(
            parsed.messages[0].kind,
            MessageKind::Meta,
            "复制来的那一轮折叠掉"
        );
    }

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
