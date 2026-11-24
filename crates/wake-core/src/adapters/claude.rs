use super::claude_desktop::{DesktopIndex, Owned, PLACEHOLDER_TITLE};
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
    // 2.1 起的记账记录:会话名(改名或自动起名时与 custom-title / ai-title 一起写、同值,
    // 不另当标题)、累计费用、请求元数据与按哈希去重存放的请求内容
    "agent-name",
    "cost-state",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    // EnterWorktree 进出 worktree 时记下的工作目录。项目仍按会话起始的目录归:worktree
    // 用完就删,恢复时 Claude Code 自己按 worktree-state 回到 worktree
    "relocated",
    "worktree-state",
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
    /// Claude 桌面端的会话记录(归档位在那里,见 `claude_desktop`)。只给默认根:记录里的
    /// CLI 会话 id 指的是本机 ~/.claude 里的转录
    desktop: Option<DesktopIndex>,
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

/// 整份文件的 uuid 集合与最后一条自定义标题、AI 标题。只给分支之前的那几份读(绝大多数
/// 会话没有分支,一份都不用读)
#[derive(Clone)]
struct Lineage {
    uuids: Arc<HashSet<String>>,
    custom_title: Option<String>,
    ai_title: Option<String>,
}

/// 一个项目目录的分支分组,按目录 mtime 校验:文件进出、改名都会改它;文件追加不会,也
/// 不影响分组(头写下就不变)。根那一行还没写的文件(刚建、头不全)让这份分组不作数,
/// 下次重算;已经读全的头留着复用,目录变了也只读新来的文件
///
/// mtime 是按时钟节拍走的(Linux 的粗粒度时钟几毫秒一跳、Windows 约 16 ms、FAT 2 s):读目录
/// 那一刻离它上次变动不到 `MTIME_SETTLE_MS`,同一个节拍里紧接着的增删就不会再改 mtime,这份
/// 分组也不作数(racy git 同款判法;2026-10-09 Linux / Windows CI 抓到,macOS 的 APFS 时间戳
/// 够细,本机测不出)
const MTIME_SETTLE_MS: i64 = 2_000;

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

/// 一份分支会话的继承段:比它早写下的同根文件(原件在其中)。文件开头连续落在它们 uuid 集合
/// 里的行是复制来的;分支的自定义标题、AI 标题跟其中哪一份的一样(分支时照抄过来)也不当标题
/// 用,往下退——否则父子两行标题一模一样,看不出分支在问什么。每一份都比、不只比原件:A 分出
/// B、B 改了名再分出 C,C 抄来的是 B 的名字
struct Inherited {
    earlier: Vec<Lineage>,
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

/// `custom-title` 行里的名字。桌面端的占位名不算名字:它写在会话刚建好的时候,之后起的名字
/// 只写进当时那一条 CLI 会话,/clear、resume 之前的那几条就一直顶着它
fn custom_title_of(v: Option<&Value>) -> Option<String> {
    optional_string(v).filter(|t| t != PLACEHOLDER_TITLE)
}

/// `ai-title` 行里命令行自己起的名字(2.1 起,#74)。解析与 read_lineage 都经这里取,分支的
/// 标题与原件的才比得上
fn ai_title_of(v: Option<&Value>) -> Option<String> {
    optional_string(v)
}

/// 读整份文件的 uuid 集合与最后一条自定义标题、AI 标题。逐行只反序列化这几个字段;坏行
/// 跳过,读错误整份失败(读不全就宁可不折)
fn read_lineage(path: &Path) -> Result<Lineage> {
    #[derive(Deserialize)]
    struct Row {
        uuid: Option<String>,
        #[serde(rename = "type")]
        typ: Option<String>,
        #[serde(rename = "customTitle")]
        custom_title: Option<Value>,
        #[serde(rename = "aiTitle")]
        ai_title: Option<Value>,
    }
    let mut uuids = HashSet::new();
    let mut title = None;
    let mut ai_title = None;
    for row in jsonl_rows::<Row, _>(BufReader::with_capacity(1 << 20, fs::File::open(path)?)) {
        let Some(row) = row? else { continue };
        if let Some(uuid) = row.uuid.filter(|u| !u.is_empty()) {
            uuids.insert(uuid);
        }
        // 与解析同一个取法,分支标题与原件标题才比得上
        match row.typ.as_deref() {
            Some("custom-title") => title = custom_title_of(row.custom_title.as_ref()).or(title),
            Some("ai-title") => ai_title = ai_title_of(row.ai_title.as_ref()).or(ai_title),
            _ => {}
        }
    }
    Ok(Lineage {
        uuids: Arc::new(uuids),
        custom_title: title,
        ai_title,
    })
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let home = super::home_dir().unwrap_or_default().join(".claude");
        Self {
            desktop: Some(DesktopIndex::new(super::claude_desktop::sessions_root())),
            ..Self::at(home.join("projects"), Some(home), true)
        }
    }

    fn at(root: PathBuf, home: Option<PathBuf>, local: bool) -> Self {
        Self {
            root,
            home,
            local,
            memories: super::MemoryCache::new(),
            decoded: Mutex::new(HashMap::new()),
            branches: Mutex::new(BranchCache::default()),
            desktop: None,
        }
    }

    /// 测试用:桌面端记录放在给定目录
    #[cfg(test)]
    pub(crate) fn with_desktop(root: PathBuf, desktop: PathBuf) -> Self {
        Self {
            desktop: Some(DesktopIndex::new(desktop)),
            ..Self::at(root, None, false)
        }
    }

    /// Claude 桌面端记着的 CLI 会话:归档位与标题(没有桌面端记录的实例恒空)
    fn desktop_owned(&self) -> HashMap<String, Owned> {
        self.desktop
            .as_ref()
            .map(DesktopIndex::owned)
            .unwrap_or_default()
    }

    /// 一个项目目录里的分支(见 `DirBranches`)。目录没变就直接用上次的分组,一次 stat;
    /// 变了只读新来文件的头。文件 I/O 不持锁——扫描、watcher、GUI 打开详情、导出可能同时问
    fn branches_in(&self, dir: &Path) -> Arc<HashMap<PathBuf, Branch>> {
        let checked_at = chrono::Utc::now().timestamp_millis();
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
        let settled = mtime.saturating_add(MTIME_SETTLE_MS) < checked_at;
        let complete = settled && !unwritten && heads.values().all(|head| head.root.is_some());
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

    /// `file_ref` 去掉桌面端那几位:子代理转录(及边车)或会话主文件
    fn own_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // 子代理转录与它的边车。会话目录得在数据根下的项目目录里
        let in_project =
            |session: &Path| session.parent().and_then(Path::parent) == Some(self.root.as_path());
        if let Some(r) = subagent_event_ref(AgentId::ClaudeCode, path, in_project) {
            return Some(r);
        }
        // 会话主文件只在 `<projects>/<项目目录>/` 这一层:更深的(memory 边车、Workflow 的
        // 运行日志 journal.jsonl)都不是。按路径分段比,Windows 的反斜杠路径一样认
        let dir = path.parent()?;
        if dir.parent() != Some(self.root.as_path()) {
            return None;
        }
        self.session_ref(path, &self.branches_in(dir))
    }

    /// 桌面端一份会话记录名下的转录(连同子代理的):watcher 看到记录变了,交它们按引用重比
    fn transcripts_of(&self, ids: &[String]) -> Vec<PathBuf> {
        let Ok(projects) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut transcripts = Vec::new();
        for project in projects.flatten() {
            let dir = project.path();
            for id in ids {
                let transcript = dir.join(format!("{id}.jsonl"));
                if transcript.is_file() {
                    transcripts.push(transcript);
                }
                // 主转录被清掉、会话目录还在时,里面的子代理照样在索引里
                transcripts.extend(subagent_transcripts(&dir.join(id)));
            }
        }
        transcripts
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

    /// `file` 是分支的话,它的继承段;不是分支给 None。更早的文件读不出来就报错,
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
        Ok(Some(Inherited { earlier }))
    }

    /// 会话的解析(parse_session 与 parse_transcript 共用):分支先拿继承段;子代理的转录整份
    /// 都是 sidechain 行(`isSidechain: true`),是 agent 派出去的,标题取派活时的任务说明
    fn parse(&self, r: &SessionFileRef, decode_images: bool) -> Result<(SessionMeta, ParseResult)> {
        let path = Path::new(&r.file_path);
        let subagent = is_subagent_transcript(&r.file_path);
        let inherited = if subagent {
            None
        } else {
            self.inherited_for(path)?
        };
        let parsed = parse_claude_jsonl(path, subagent, decode_images, inherited.as_ref())?;
        let mut meta = build_meta(r, &parsed);
        mark_subagent(&mut meta, path);
        let desktop = self.desktop_owned();
        meta.archived = archived_in(&desktop, r);
        // 转录里没起过名字(或只有桌面端的占位名):用桌面会话的名字(用户在桌面端看到的
        // 就是它),再退回 AI 标题、首条提问
        if !parsed.titled {
            if let Some(title) = desktop_title(&desktop, r) {
                meta.title = title.to_string();
            }
        }
        Ok((meta, parsed))
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
    /// 标题是转录里起的名字(`custom-title`),不是 AI 标题或退回来的首条提问
    titled: bool,
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
    // 2.1 起命令行自己起的标题;只用命令行、没有桌面端写 custom-title 的会话全靠它(#74)
    let mut ai_title = String::new();
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
            if let Some(t) = custom_title_of(row.get("customTitle")) {
                custom_title = t;
            }
            continue;
        }
        if typ == "ai-title" {
            if let Some(t) = ai_title_of(row.get("aiTitle")) {
                ai_title = t;
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

            let is_meta = row.get("isMeta").and_then(|v| v.as_bool()) == Some(true);
            let is_compact = row.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true);
            // 桌面端垫在人说的话前面的状态("Linked sessions now…"):单独一条 Meta,话照常判
            let ParsedContent { text, images } = if is_meta || is_compact || !from_human(&row) {
                parsed_message
            } else {
                push_leading_context(parsed_message, ts, &mut messages)
            };
            if text.is_empty() && images.is_empty() {
                continue;
            }
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
        let earlier = &inh.earlier;
        if earlier
            .iter()
            .any(|l| l.custom_title.as_deref() == Some(&custom_title))
        {
            custom_title.clear();
        }
        if earlier
            .iter()
            .any(|l| l.ai_title.as_deref() == Some(&ai_title))
        {
            ai_title.clear();
        }
    }
    assign_seq(&mut messages);

    let titled = !custom_title.is_empty();
    let title = [custom_title, ai_title, fallback_title]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string());
    Ok(ParseResult {
        titled,
        title,
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

/// 这份转录在 Claude 桌面端归档了没有。子代理跟着派它的会话:父会话归档了、子代理还开着,
/// 列表会把子代理当成没有父会话的顶层会话列出来
fn archived_in(desktop: &HashMap<String, Owned>, r: &SessionFileRef) -> bool {
    let parent = subagent_parent(Path::new(&r.file_path));
    desktop
        .get(parent.as_deref().unwrap_or(&r.native_id))
        .is_some_and(|owned| owned.archived)
}

/// 桌面会话的名字(子代理不用它,它的标题是派活时的任务说明)
fn desktop_title<'a>(desktop: &'a HashMap<String, Owned>, r: &SessionFileRef) -> Option<&'a str> {
    if is_subagent_transcript(&r.file_path) {
        return None;
    }
    desktop.get(&r.native_id)?.title.as_deref()
}

/// 桌面端记的归档位与标题合起来取指纹,并进引用的 size:在桌面端归档、取消归档、改名时转录
/// 未必动,全量扫描按引用认出它变了、重解析。不是 +1:分支的引用也往 size 里加着更早的同根
/// 文件数,两样同时变(删掉中间那份分支、又归档了它的分支)会正好抵消。列表与 file_ref 各在
/// 出口盖一次
fn stamp_desktop(mut r: SessionFileRef, desktop: &HashMap<String, Owned>) -> SessionFileRef {
    let archived = archived_in(desktop, &r);
    let title = desktop_title(desktop, &r);
    if archived || title.is_some() {
        let archived = if archived { "archived" } else { "" };
        r.size += sidecar_fingerprint(&[archived, title.unwrap_or("")]);
    }
    r
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
        spawned: false,
        favorite: false,
        pinned: false,
    }
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
            for entry in entries.flatten() {
                let path = entry.path();
                // 会话目录(`<id>/`)里是它派的子代理,各自作为子会话
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    refs.extend(subagent_refs(AgentId::ClaudeCode, &path));
                } else if let Some(r) = self.session_ref(&path, &branches) {
                    refs.push(r);
                }
            }
        }
        let desktop = self.desktop_owned();
        Ok(refs
            .into_iter()
            .map(|r| stamp_desktop(r, &desktop))
            .collect())
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, parsed) = self.parse(r, false)?;
        Ok(ParsedSession::derive(
            meta,
            &parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, parsed) = self.parse(r, true)?;
        Ok(transcript_with_subagents(
            r,
            meta,
            parsed.messages,
            parsed.unknown_lines,
        ))
    }

    fn load_sidechain(
        &self,
        r: &SessionFileRef,
        sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        load_subagent(r, sidechain_id, |file| {
            Ok(parse_claude_jsonl(file, true, true, None)?.messages)
        })
    }

    /// 项目目录里的会话文件进出(原件被删、被放回)会改变同目录分支的引用(`session_ref`
    /// 数着更早的同根文件):交出同目录的其余会话文件,watcher 按引用重比、变了的重解析。
    /// 桌面端的会话记录变了(在桌面端归档、改名)同理:交出它名下的转录。记录从不入库,它的
    /// 每次改动 watcher 都会来问,而桌面端每次聚焦都重写它——Wake 用得上的内容没变就不交
    fn ref_dependents(&self, path: &Path) -> Vec<PathBuf> {
        if let Some(ids) = self.desktop.as_ref().and_then(|d| d.take_change(path)) {
            return self.transcripts_of(&ids);
        }
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
        let r = self.own_ref(path)?;
        Some(stamp_desktop(r, &self.desktop_owned()))
    }

    /// 分支挂到原件下面(见本文件「分支识别」),子代理挂到派它的会话下面(`parent_in_path`),
    /// 桌面端派出去的会话挂到派它的会话下面(`claude_desktop`)。分支的关系由它自己的文件头与
    /// 同目录里更早的同根文件推出,派生关系记在子会话自己的桌面端记录里,所以都算"写在子会话
    /// 里"(多 location 下按子会话的胜出副本认边)
    fn manages_parent_links(&self) -> bool {
        true
    }

    fn parent_links_in_child(&self) -> bool {
        true
    }

    /// 分支是自己完整的一段对话:删原件不带上它们,原件没了它们变回顶层会话、露出完整历史
    /// (重解析由 watcher 经 `ref_dependents` 触发)。子代理不是——它的文件就在派它的会话目录里
    fn child_outlives_parent(&self, child_path: &str) -> bool {
        !is_subagent_transcript(child_path)
    }

    /// 全量快照只用文件头(不读 uuid 集合):每个分支 → 它所在同根组里最早的那份。分支
    /// 的分支也直接挂到原件上——scanner 本来就把链压平到根。目录没变的,分组直接复用
    /// (每批 watcher 事件都会问一遍,每个目录一次 stat)。再加上桌面端侧栏的嵌套。projects
    /// 目录不在 = 确定没有关系;在却列不出来是"不知道",交 None 让 scanner 保留库里的关系
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
        // 桌面端侧栏的嵌套:同一个会话既是分支又是派出去的(fork 了上下文的侧会话)时,以转录里
        // 看得见的分支关系为准
        let spawned = self
            .desktop
            .as_ref()
            .map(DesktopIndex::spawn_links)
            .unwrap_or_default();
        let branched: HashSet<String> = links.iter().map(|(child, _)| child.clone()).collect();
        links.extend(
            super::link_keys(
                AgentId::ClaudeCode,
                spawned
                    .iter()
                    .map(|(child, parent)| (child.as_str(), parent.as_str())),
            )
            .into_iter()
            .filter(|(child, _)| !branched.contains(child)),
        );
        Some(links)
    }

    /// 桌面端的会话记录变了(新派出一个会话、派它的会话 /clear 换了当前那条):父子关系跟着
    /// 刷新。记录里的转录没动,收不到别的事件;只是聚焦一下重写了记录不算
    fn is_snapshot_event(&self, path: &Path) -> bool {
        self.desktop.as_ref().is_some_and(|d| d.changed(path))
    }

    /// 子代理挂到派它的会话:所在会话目录的名字就是那个会话的 id
    fn parent_in_path(&self) -> Option<super::ParentInPath> {
        Some(subagent_parent)
    }

    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        if is_subagent_transcript(&meta.file_path) {
            return subagent_paths(&meta.file_path);
        }
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

    fn homes(&self) -> Vec<PathBuf> {
        self.home.iter().cloned().collect()
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

    /// 数据根之外再听桌面端的会话记录:在桌面端归档、取消归档只改那份记录,转录不动,
    /// 不听它就要等下一轮全量扫描(`ref_dependents` 把记录换成它名下的转录)。记录不是会话
    /// 文件,不进 data_roots
    fn watch_paths(&self) -> Vec<PathBuf> {
        let desktop = self.desktop.as_ref().map(|d| d.root().to_path_buf());
        self.data_roots()
            .into_iter()
            .chain(desktop)
            .filter(|p| p.is_dir())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::claude_desktop::write_test_record;

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
    #[cfg(unix)]
    #[test]
    fn an_unwritten_session_file_keeps_the_grouping_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, _, branch) = stage_branch(tmp.path());
        let content = fs::read(&branch).unwrap();
        fs::write(&branch, "").unwrap();
        // 目录早就不动了,只剩零字节这一条理由让分组不作数
        pin_mtime(
            &dir,
            std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        let adapter = ClaudeAdapter::at(tmp.path().to_path_buf(), None, false);
        assert!(adapter.branches_in(&dir).is_empty());
        fs::write(&branch, content).unwrap();
        assert!(
            adapter.branches_in(&dir).contains_key(&branch),
            "写下第一行以后要认出分支"
        );
    }

    /// 在 Claude 桌面端归档:归档位并进引用(列表与 file_ref 一致)、解析出来的会话标成归档,
    /// 子代理跟着派它的会话;记录变了交出名下的转录让 watcher 重比;取消归档原样退回
    #[test]
    fn archiving_in_the_desktop_app_reaches_refs_and_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("-w");
        let subagents = dir.join("sess-a").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let line = |id: &str, sidechain: bool| {
            format!(
                r#"{{"type":"user","uuid":"u-{id}","isSidechain":{sidechain},"timestamp":"2026-09-01T08:00:01.000Z","sessionId":"{id}","message":{{"role":"user","content":"hello"}}}}"#
            ) + "\n"
        };
        let main = dir.join("sess-a.jsonl");
        let agent = subagents.join("agent-a1.jsonl");
        let other = dir.join("sess-b.jsonl");
        fs::write(&main, line("sess-a", false)).unwrap();
        fs::write(&agent, line("sess-a", true)).unwrap();
        fs::write(&other, line("sess-b", false)).unwrap();

        let desktop = tmp.path().join("desktop");
        let write_record = |archived: bool| {
            let fields = serde_json::json!({ "cliSessionId": "sess-a", "isArchived": archived });
            write_test_record(&desktop, "d", fields)
        };
        let record = write_record(true);

        let adapter = ClaudeAdapter::with_desktop(projects.clone(), desktop.clone());
        assert!(adapter.watch_paths().contains(&desktop));

        let state = |adapter: &ClaudeAdapter| {
            let listed = adapter.list_session_files().unwrap();
            [&main, &agent, &other].map(|path| {
                let r = adapter.file_ref(path).unwrap();
                let from_list = listed.iter().find(|l| l.file_path == r.file_path).unwrap();
                assert_eq!((from_list.mtime_ms, from_list.size), (r.mtime_ms, r.size));
                let stamped = r.size != fs::metadata(path).unwrap().len() as i64;
                (stamped, adapter.parse_session(&r).unwrap().meta.archived)
            })
        };
        assert_eq!(
            state(&adapter),
            [(true, true), (true, true), (false, false)]
        );

        let mut dependents = adapter.ref_dependents(&record);
        dependents.sort();
        assert_eq!(dependents, vec![agent.clone(), main.clone()]);

        write_record(false);
        assert_eq!(
            state(&adapter),
            [(false, false), (false, false), (false, false)]
        );

        fs::remove_file(&main).unwrap();
        assert_eq!(
            adapter.ref_dependents(&record),
            vec![agent.clone()],
            "主转录没了,会话目录里的子代理照样交出去"
        );
    }

    /// 归档位不能被分支计数抵消:分支的引用往 size 里加着更早的同根文件数,删掉中间那份分支
    /// 让它减 1,同时归档就不能正好加 1
    #[test]
    fn the_archive_stamp_is_not_a_unit_step() {
        let r = SessionFileRef {
            agent: AgentId::ClaudeCode,
            native_id: "s".to_string(),
            file_path: "/p/-w/s.jsonl".to_string(),
            mtime_ms: 1,
            size: 100,
        };
        let owned = |archived: bool, title: Option<&str>| {
            let facts = Owned {
                archived,
                title: title.map(str::to_string),
            };
            HashMap::from([("s".to_string(), facts)])
        };
        for title in [None, Some("图片下载")] {
            let open = stamp_desktop(r.clone(), &owned(false, title)).size;
            let archived = stamp_desktop(r.clone(), &owned(true, title)).size;
            assert!(
                (archived - open).abs() > 1_000,
                "{title:?}: {open} → {archived}"
            );
        }
    }

    /// 标题:转录里起过的名字照用;桌面端的占位名 "New session" 不算名字——有桌面会话的名字
    /// 用它(改名并进引用),没有就退回首条提问
    #[test]
    fn desktop_titles_replace_the_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("-w");
        fs::create_dir_all(&dir).unwrap();
        let write = |id: &str, title: &str| {
            let lines = [
                format!(r#"{{"type":"custom-title","customTitle":"{title}","sessionId":"{id}"}}"#),
                format!(
                    r#"{{"type":"user","uuid":"u-{id}","timestamp":"2026-09-01T08:00:01.000Z","sessionId":"{id}","message":{{"role":"user","content":"why does login time out"}}}}"#
                ),
            ];
            let path = dir.join(format!("{id}.jsonl"));
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        };
        let renamed_later = write("prior", PLACEHOLDER_TITLE);
        let no_record = write("lone", PLACEHOLDER_TITLE);
        let named = write("named", "Real name");

        let desktop = tmp.path().join("desktop");
        let write_record = |title: &str| {
            let fields = serde_json::json!({
                "cliSessionId": "current", "priorCliSessionIds": ["prior", "named"], "title": title,
            });
            write_test_record(&desktop, "d", fields);
        };
        write_record("图片下载");
        let adapter = ClaudeAdapter::with_desktop(projects, desktop.clone());
        let title = |path: &Path| {
            let r = adapter.file_ref(path).unwrap();
            let listed = adapter.list_session_files().unwrap();
            let from_list = listed.iter().find(|l| l.file_path == r.file_path).unwrap();
            assert_eq!(from_list.size, r.size, "列表与 file_ref 一致");
            (r.size, adapter.parse_session(&r).unwrap().meta.title)
        };
        let (before, renamed) = title(&renamed_later);
        assert_eq!(renamed, "图片下载");
        assert_eq!(title(&no_record).1, "why does login time out");
        assert_eq!(title(&named).1, "Real name", "转录里起过的名字照用");

        write_record("Image downloads");
        let (after, renamed) = title(&renamed_later);
        assert_eq!(renamed, "Image downloads");
        assert_ne!(before, after, "桌面端改名,引用跟着变");
    }

    /// 把目录的 mtime 定在给定时刻(只在 unix 上能对目录开文件句柄)
    #[cfg(unix)]
    fn pin_mtime(dir: &Path, when: std::time::SystemTime) {
        fs::File::open(dir).unwrap().set_modified(when).unwrap();
    }

    /// 刚变过的目录:同一个时钟节拍里再删一份文件,mtime 不变(Linux / Windows 的时间戳按
    /// 节拍走)——这时的分组不能当成完整缓存,否则原件没了分支还挂着
    #[cfg(unix)]
    #[test]
    fn a_directory_that_just_changed_is_not_trusted_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, original, branch) = stage_branch(tmp.path());
        let adapter = ClaudeAdapter::at(tmp.path().to_path_buf(), None, false);
        let now = std::time::SystemTime::now();
        pin_mtime(&dir, now);
        assert!(adapter.branches_in(&dir).contains_key(&branch));
        fs::remove_file(&original).unwrap();
        pin_mtime(&dir, now);
        assert!(
            adapter.branches_in(&dir).is_empty(),
            "同一节拍里删掉的原件也得看见"
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
        assert!(adapter.parse(&r, false).is_err());
        fs::set_permissions(&original, fs::Permissions::from_mode(0o644)).unwrap();
        let (_, parsed) = adapter.parse(&r, false).unwrap();
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
