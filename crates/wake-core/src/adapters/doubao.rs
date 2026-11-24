//! 豆包工作(Doubao Work,字节跳动的办公 agent;起初是豆包电脑版的"任务"模式,2026-08 拆成独立
//! 品牌)。选"本地电脑"跑的任务,会话落在豆包电脑版每个 Chromium profile 里(issue #86 报的布局,
//! 2026-11-23 在豆包电脑版 2.37.16 上对着真实会话核对过——一个 agent 问两轮、一个派了组织者与助手的
//! 任务;"云电脑"跑的任务本地没有文件):
//!
//! ```text
//! <用户数据>/<profile>/.doubao/agent_mode/workspace/.sessions/<会话 id>/
//! ├── board.md                         子任务进度表(不是会话标题)
//! ├── memory/MEMORY.md、attachments/   会话记忆与附件(不读)
//! └── agents/<agent id>/               m_… 主代理、o_… 组织者、s_… 助手
//!     ├── socket/、scratch/、poll-monitors/、agent 自己写的脚本
//!     └── system/
//!         ├── assignment.md            头(`- Role:`、`- Assigned by:`……)+ 一节一句交给它的话
//!         ├── trajectory.jsonl         这个 agent 的转录
//!         └── tool-results/
//! ```
//!
//! `trajectory.jsonl` 是 OpenAI 消息形的 JSONL:role 只有 user / assistant / tool,assistant 的
//! `tool_calls[].function.arguments` 已经是对象,工具结果按 `tool_call_id` 配对,没有时间、没有
//! system 记录、没有 thinking(见到了计未知行)。**第一句提问不在转录里**:它是 assignment.md 的
//! 第一节(`## [<RFC 3339 时间>] 需求`),转录从回答开始;追问两边都有(转录里一条 user 行,
//! assignment.md 里再多一节)。所以第一节排在转录前面,每句提问拿它那一节的时间,主代理的标题就是
//! 第一句提问。
//!
//! 每个 agent 一条会话,native id 是路径上的 `<会话>/<agent>`:只由路径定,谁是主代理认错了也不会换
//! key(同一个文件换 key 会撞 file_path 的 UNIQUE)。主代理那条是会话本身——头里 `Role: main`,
//! 没有头的(只有一个 agent 的会话,实测)认 `m_` 前缀,再不行会话里只有一个 agent 就是它。别的
//! agent 是派出去的子会话,经 `parent_links` 挂在头里 `Assigned by` 点名的那个下面(没写就看谁的
//! `create_agent` 派了它,再不行挂主代理),标题取派它的那次 `create_agent` 结果里的 `title`,那次
//! 调用也能点进子会话。
//!
//! 只有本地跑的 agent 有转录:实测三个调研助手的 `system/` 一直是空的(云端跑完、文件同步下来),
//! 合并成 Word 的那个才在本地。**没核实过的**:Windows 版的位置。删除只隐藏,不动豆包自己的目录。
use super::parse_utils::*;
use super::AgentAdapter;
use crate::models::*;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// profile 目录 → 会话根,逐段拼(Windows 上才是反斜杠路径)
const SESSIONS_REL: [&str; 4] = [".doubao", "agent_mode", "workspace", ".sessions"];
const AGENTS: &str = "agents";
/// agent 目录里放转录的那一层(issue 里写作 `<sysDir>`,实测就叫这个)
const SYSTEM: &str = "system";
const TRAJECTORY: &str = "trajectory.jsonl";
const ASSIGNMENT: &str = "assignment.md";
/// 主代理目录名的前缀(实测 `m_0cweC6udWVG`;组织者 `o_…`、助手 `s_…`)
const MAIN_PREFIX: &str = "m_";
/// 派 agent 的工具 `create_agent` 的结果:这一行起头,往下 `agent_id:`、`title:` 等一行一项
const CREATED: &str = "create_agent ok";

pub struct DoubaoAdapter {
    /// 会话根(`…/.sessions`),每个 profile 一个。构造时快照(不变量 8):之后新建的 profile
    /// 重启 Wake 才收
    roots: Vec<PathBuf>,
    assignments: StampCache<Assignment>,
    /// 每份转录用 `create_agent` 派出去的 agent → 派它时起的标题
    spawned: StampCache<HashMap<String, String>>,
}

/// 豆包电脑版的用户数据目录(Chromium 的 user data dir,profile 在它下面一层)。macOS 是
/// `~/Library/Application Support/Doubao`;Windows 没核实过——Chromium 系的惯例是
/// `%LOCALAPPDATA%\<App>\User Data`,网上的说法是 `%LOCALAPPDATA%\Doubao`,两处都探。Linux 没有
/// 豆包电脑版,给一个 Electron 形态的位置只是为了每个实例都有根
fn user_data_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "windows") {
        let local = super::home_dir()
            .unwrap_or_default()
            .join("AppData")
            .join("Local")
            .join("Doubao");
        vec![local.join("User Data"), local]
    } else {
        vec![super::electron_user_data("Doubao")]
    }
}

/// 一个会话里的各个 agent 与它们的关系:列会话、file_ref、父子关系、解析共用这一个认法
struct Plan {
    /// 会话目录名(会话 id)
    session: String,
    runs: Vec<Run>,
    main: Option<usize>,
    /// 会话里各份转录 `create_agent` 派出去的 agent → (派它的那个, 派它时起的标题;没起是空串)
    spawned: HashMap<String, (usize, String)>,
}

/// 会话里一个有转录的 agent。两个文件的戳只 stat 一次,缓存与引用都用它
struct Run {
    trajectory: PathBuf,
    agent: String,
    stamp: (i64, i64),
    /// assignment.md 的戳;没有这个文件是 None
    assignment_stamp: Option<(i64, i64)>,
    /// None = 在却读不出(又没读到过)
    assignment: Option<Arc<Assignment>>,
}

impl Run {
    /// 读不出的 assignment.md 认主代理、找上级时照没有它算(解析那一步另外报错)
    fn assignment(&self) -> &Assignment {
        static NONE: Assignment = Assignment {
            role: String::new(),
            assigned_by: String::new(),
            created: 0,
            sections: Vec::new(),
        };
        self.assignment.as_deref().unwrap_or(&NONE)
    }
}

impl Plan {
    /// 主代理:头里 `Role: main` 的(几个都写了取路径序第一个);否则目录名以 `m_` 起头的,
    /// 恰好一个;否则会话里只有一个 agent,就是它
    fn main_of(runs: &[Run]) -> Option<usize> {
        if let Some(ix) = runs.iter().position(|run| run.assignment().is_main()) {
            return Some(ix);
        }
        let mut prefixed = runs
            .iter()
            .enumerate()
            .filter(|(_, run)| run.agent.starts_with(MAIN_PREFIX))
            .map(|(ix, _)| ix);
        match (prefixed.next(), prefixed.next(), runs.len()) {
            (Some(ix), None, _) => Some(ix),
            (_, _, 1) => Some(0),
            _ => None,
        }
    }

    fn id(&self, ix: usize) -> String {
        native_id(&self.session, &self.runs[ix].agent)
    }

    /// 派这个 agent 的那个:头里 `Assigned by` 点名的;没有头的(云端跑过、后来接着在本地跑的助手,
    /// 实测 assignment.md 没有头)看会话里哪份转录的 `create_agent` 派了它;都不在本地就挂主代理
    fn parent(&self, ix: usize) -> Option<usize> {
        if self.main == Some(ix) {
            return None;
        }
        let run = &self.runs[ix];
        let by = &run.assignment().assigned_by;
        self.runs
            .iter()
            .position(|other| other.agent == *by)
            .or_else(|| self.spawned.get(&run.agent).map(|(creator, _)| *creator))
            .filter(|&parent| parent != ix)
            .or(self.main)
    }

    /// 子会话的标题:派它的那次 `create_agent` 结果里的 `title`
    fn spawned_title(&self, ix: usize) -> Option<&str> {
        self.spawned
            .get(&self.runs[ix].agent)
            .map(|(_, title)| title.as_str())
            .filter(|title| !title.is_empty())
    }

    /// 一个 agent 那条会话的引用。assignment.md 并进来——第一句提问与各句的时间在它里面;出自同会话
    /// 别的文件的(是不是主代理、谁派的、派它时起的标题)按指纹并进 size,认法变了、标题后写进上级的
    /// 转录时,全量扫描认得出
    fn file_ref(&self, ix: usize) -> SessionFileRef {
        let run = &self.runs[ix];
        let (mut mtime_ms, mut size) = run.stamp;
        if let Some((assignment_mtime, assignment_size)) = run.assignment_stamp {
            mtime_ms = mtime_ms.max(assignment_mtime);
            size += assignment_size;
        }
        let parent = self
            .parent(ix)
            .map(|parent| self.runs[parent].agent.as_str());
        let main = if self.main == Some(ix) { "main" } else { "" };
        SessionFileRef {
            agent: AgentId::DoubaoWork,
            native_id: self.id(ix),
            file_path: run.trajectory.to_string_lossy().into_owned(),
            mtime_ms,
            size: size
                + sidecar_fingerprint(&[
                    main,
                    parent.unwrap_or_default(),
                    self.spawned_title(ix).unwrap_or_default(),
                ]),
        }
    }
}

impl DoubaoAdapter {
    pub fn new() -> Self {
        let user_data = user_data_dirs();
        let roots: Vec<PathBuf> = user_data
            .iter()
            .flat_map(|dir| profile_roots(dir))
            .collect();
        if roots.is_empty() {
            // 还没用过任务模式:默认 profile 的位置
            return Self::with_roots(vec![sessions_root(&user_data[0].join("Default"))]);
        }
        Self::with_roots(roots)
    }

    fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            assignments: StampCache::default(),
            spawned: StampCache::default(),
        }
    }

    /// 全部会话目录(`<根>/<会话>`)。根不在是没有;根在却列不出是这一刻读不出,报错让 scanner 冻结
    /// 这一家这一轮——当成空,整家会话会被当成磁盘上已删
    fn session_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for root in &self.roots {
            match subdirs(root) {
                Ok(dirs) => out.extend(dirs),
                Err(e) if is_absent(&e) => {}
                Err(e) => return Err(anyhow!("{}: {e}", root.display())),
            }
        }
        Ok(out)
    }

    /// 会话的各个 agent(`agents/<agent>/system/trajectory.jsonl`,空文件不算——云端跑的助手只有
    /// 一个空的 system/)与它们的关系。agent 目录列不出、转录读不出元数据是这一刻读不出:报错,
    /// 列会话那一步让 scanner 冻结这一家这一轮——当成没有,少列的会话会被当成磁盘上已删(2026-11-23
    /// Codex review)。不是会话目录(名字不是 UTF-8)给 None
    fn plan(&self, session: &Path) -> io::Result<Option<Plan>> {
        let Some(name) = session.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        let dirs = match subdirs(&session.join(AGENTS)) {
            Ok(dirs) => dirs,
            Err(e) if is_absent(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut runs = Vec::new();
        for dir in dirs {
            let Some(agent) = dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let trajectory = dir.join(SYSTEM).join(TRAJECTORY);
            let Some(stamp) = file_stamp(&trajectory)?.filter(|(_, size)| *size > 0) else {
                continue;
            };
            let assignment_path = assignment_of(&trajectory);
            // assignment.md 读不出:这一条照列,解析那一步报错、下一轮再来
            let (assignment_stamp, assignment) = match file_stamp(&assignment_path) {
                Ok(stamp) => (
                    stamp,
                    self.assignments
                        .get(&assignment_path, stamp, Assignment::read)
                        .ok(),
                ),
                Err(_) => (None, None),
            };
            runs.push(Run {
                agent: agent.to_string(),
                trajectory,
                stamp,
                assignment_stamp,
                assignment,
            });
        }
        let main = Plan::main_of(&runs);
        let mut spawned = HashMap::new();
        // 只有一个 agent 的会话(最常见)没有谁派谁,不读。派活清单只是锦上添花:读不出就当没派过谁,
        // 上级退回头里写的或主代理
        if runs.len() > 1 {
            for (creator, run) in runs.iter().enumerate() {
                let made = self
                    .spawned
                    .get(&run.trajectory, Some(run.stamp), spawned_titles)
                    .unwrap_or_default();
                for (agent, title) in made.iter() {
                    spawned.insert(agent.clone(), (creator, title.clone()));
                }
            }
        }
        Ok(Some(Plan {
            session: name.to_string(),
            runs,
            main,
            spawned,
        }))
    }

    /// 一条事件路径(转录或同目录的 assignment.md)→ 它所在会话的认法与它是哪个 agent;不是会话
    /// 文件、或这个 agent 没有转录给 None
    fn locate(&self, path: &Path) -> io::Result<Option<(Plan, usize)>> {
        let Some(trajectory) = trajectory_for(path) else {
            return Ok(None);
        };
        let Some((session, agent)) = run_of(&trajectory) else {
            return Ok(None);
        };
        let Some(plan) = self.plan(session)? else {
            return Ok(None);
        };
        Ok(plan
            .runs
            .iter()
            .position(|run| run.agent == agent)
            .map(|ix| (plan, ix)))
    }

    fn parse(
        &self,
        r: &SessionFileRef,
        decode_images: bool,
    ) -> Result<(SessionMeta, Vec<TranscriptMessage>, u32)> {
        let (plan, ix) = self
            .locate(Path::new(&r.file_path))?
            .ok_or_else(|| anyhow!("not a Doubao Work transcript: {}", r.file_path))?;
        let run = &plan.runs[ix];
        // assignment.md 在却读不出(又没读到过):这次解析失败、下一轮再来——按没有它入库,第一句
        // 提问就丢了,而引用里记着它此刻的戳,文件不再变就一直缺着
        let assignment = run.assignment.clone().ok_or_else(|| {
            anyhow!(
                "{} could not be read",
                assignment_of(&run.trajectory).display()
            )
        })?;
        let (messages, unknown) = read_trajectory(&run.trajectory, decode_images)?;
        let mut messages = with_requests(&assignment.sections, messages);
        link_spawned_agents(&plan.session, &mut messages);
        assign_seq(&mut messages);
        let created = assignment
            .sections
            .first()
            .map(|section| section.ts)
            .or(Some(assignment.created).filter(|ts| *ts > 0))
            .or_else(|| {
                fs::metadata(&run.trajectory)
                    .ok()
                    .and_then(|meta| created_ms(&meta))
            })
            .filter(|ts| *ts > 0)
            .unwrap_or(r.mtime_ms);
        let meta = SessionMeta {
            key: session_key(AgentId::DoubaoWork, "", &r.native_id),
            host: String::new(),
            id: r.native_id.clone(),
            agent: AgentId::DoubaoWork,
            // 豆包界面上的会话标题是云端起的,本地没有:主代理用第一句提问,子会话用派它时起的标题
            title: session_title(plan.spawned_title(ix).unwrap_or(""), false, &messages),
            // 产出放在 `~/Doubao/chats/<日期>/<名字>`,只写在给 agent 的提示里,本地文件没记
            project_path: String::new(),
            project_name: project_name_of(""),
            file_path: r.file_path.clone(),
            created_at: created.min(r.mtime_ms),
            updated_at: r.mtime_ms,
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
            // 派出去干活的;它们收到的第一句是上级写的任务
            spawned: plan.parent(ix).is_some(),
            favorite: false,
            pinned: false,
        };
        Ok((meta, messages, unknown))
    }
}

/// 一个 agent 那条会话的 native id(`create_agent` 结果里的 agent id 点进子会话也用它)
fn native_id(session: &str, agent: &str) -> String {
    format!("{session}/{agent}")
}

/// 文件的 (mtime, size);不在(或不是普通文件)是 None,在却读不出元数据报错——`stat_stamp` 把两者都当
/// 没有,这里要分开
fn file_stamp(path: &Path) -> io::Result<Option<(i64, i64)>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file().then(|| (mtime_ms(&meta), meta.len() as i64))),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 目录下的子目录(点开头的不算),按路径排
fn subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()))
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// profile 目录下的会话根
fn sessions_root(profile: &Path) -> PathBuf {
    let mut root = profile.to_path_buf();
    root.extend(SESSIONS_REL);
    root
}

/// 用户数据目录里用过任务模式的 profile(按名字排),各一个会话根
fn profile_roots(user_data: &Path) -> Vec<PathBuf> {
    subdirs(user_data)
        .unwrap_or_default()
        .iter()
        .map(|profile| sessions_root(profile))
        .filter(|root| root.is_dir())
        .collect()
}

/// 自定义 location 的会话根。名字认得出的层(`.doubao`、`agent_mode`、`workspace`、`.sessions`)
/// 直接接上余下几段;其余看里面:本身是一个 profile(有 `.doubao/`)、装着几个 profile 的用户数据
/// 目录,都不是(`.sessions` 的拷贝、空目录、还不存在)就当会话根本身
fn custom_roots(dir: PathBuf) -> Vec<PathBuf> {
    let named = dir
        .file_name()
        .and_then(|name| SESSIONS_REL.iter().position(|seg| name == *seg));
    if let Some(ix) = named {
        let mut root = dir;
        root.extend(&SESSIONS_REL[ix + 1..]);
        return vec![root];
    }
    if dir.join(SESSIONS_REL[0]).is_dir() {
        return vec![sessions_root(&dir)];
    }
    let profiles = profile_roots(&dir);
    if profiles.is_empty() {
        vec![dir]
    } else {
        profiles
    }
}

/// 转录同目录的 assignment.md
fn assignment_of(trajectory: &Path) -> PathBuf {
    trajectory.with_file_name(ASSIGNMENT)
}

/// 事件路径 → 它所属 agent 的转录:转录本身,或同目录的 assignment.md(提问、角色、上级都在里面)
fn trajectory_for(path: &Path) -> Option<PathBuf> {
    match path.file_name()?.to_str()? {
        TRAJECTORY => Some(path.to_path_buf()),
        ASSIGNMENT => Some(path.with_file_name(TRAJECTORY)),
        _ => None,
    }
}

/// `…/<会话>/agents/<agent>/system/trajectory.jsonl` → (会话目录, agent);别的形状给 None
fn run_of(path: &Path) -> Option<(&Path, &str)> {
    if path.file_name()? != TRAJECTORY {
        return None;
    }
    let system = path.parent()?;
    let agent = system.parent()?;
    let agents = agent.parent()?;
    if system.file_name()? != SYSTEM || agents.file_name()? != AGENTS {
        return None;
    }
    Some((agents.parent()?, agent.file_name()?.to_str()?))
}

/// assignment.md 的一节:`## [<RFC 3339 时间>] <标签>` 起头,到下一个这样的标题为止
#[derive(Debug, PartialEq)]
struct Section {
    ts: i64,
    body: String,
}

/// assignment.md:开头一个头(`- Agent ID:`、`- Role:`、`- Assigned by:`、`- Created:` 一行一项;
/// 只有一个 agent 的会话、云端跑过又回本地的助手实测没有头),往下一节一句交给这个 agent 的话
#[derive(Debug, Default)]
struct Assignment {
    /// main / organizer / subagent
    role: String,
    /// 主代理是 user,别的是派它的那个 agent 的目录名
    assigned_by: String,
    created: i64,
    sections: Vec<Section>,
}

impl Assignment {
    fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::parse(&fs::read_to_string(path)?))
    }

    fn parse(text: &str) -> Self {
        let mut assignment = Self::default();
        for line in text.lines() {
            if let Some(ts) = section_time(line) {
                assignment.sections.push(Section {
                    ts,
                    body: String::new(),
                });
            } else if let Some(section) = assignment.sections.last_mut() {
                section.body.push_str(line);
                section.body.push('\n');
            } else if let Some((key, value)) = header_field(line) {
                match key.to_ascii_lowercase().as_str() {
                    "role" => assignment.role = value,
                    "assigned by" => assignment.assigned_by = value,
                    "created" => assignment.created = iso_ms(&value),
                    _ => {}
                }
            }
        }
        for section in &mut assignment.sections {
            section.body = section.body.trim().to_string();
        }
        assignment
            .sections
            .retain(|section| !section.body.is_empty());
        assignment
    }

    fn is_main(&self) -> bool {
        self.role.eq_ignore_ascii_case("main")
    }
}

/// `## [2026-11-23T08:17:14.767849979Z] 需求` → 那个时间(毫秒);不是这样的标题给 None
fn section_time(line: &str) -> Option<i64> {
    let (stamp, _label) = line.strip_prefix("## [")?.split_once(']')?;
    Some(iso_ms(stamp.trim())).filter(|ts| *ts > 0)
}

/// 头里的一项 `- Role: main`:列表符号、粗体、反引号、全角冒号都放过
fn header_field(line: &str) -> Option<(String, String)> {
    let line = line
        .trim()
        .trim_start_matches(['-', '*', '+'])
        .replace(['*', '`'], "");
    let (key, value) = line.split_once([':', '：'])?;
    Some((key.trim().to_string(), value.trim().to_string()))
}

/// 转录配上 assignment.md:第一节起头、转录里没有(转录从回答开始),排在最前;追问在转录里有
/// user 行,按先后对上各节,拿那一节的时间(转录的行没有时间)
fn with_requests(
    sections: &[Section],
    mut messages: Vec<TranscriptMessage>,
) -> Vec<TranscriptMessage> {
    let asked: Vec<TranscriptMessage> = sections
        .iter()
        .map(|section| text_msg(Role::User, &section.body, section.ts))
        .collect();
    let Some(first) = asked.first() else {
        return messages;
    };
    let echoed = messages
        .first()
        .is_some_and(|m| m.role == Role::User && m.text == first.text);
    if !echoed {
        messages.insert(0, first.clone());
    }
    let mut next = 0;
    for message in messages.iter_mut().filter(|m| m.role == Role::User) {
        if let Some(offset) = asked[next..].iter().position(|a| a.text == message.text) {
            message.timestamp = asked[next + offset].timestamp;
            next += offset + 1;
        }
    }
    messages
}

/// `create_agent` 的结果 → (派出去的 agent, 派它时起的标题;没起是空串)
fn created_agent(output: &str) -> Option<(&str, &str)> {
    if !output.starts_with(CREATED) {
        return None;
    }
    let field = |key: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
            .map(str::trim)
    };
    let agent = field("agent_id").filter(|agent| !agent.is_empty())?;
    Some((agent, field("title").unwrap_or_default()))
}

/// 派 agent 的调用点进它那条子会话(子会话没在本地跑过的,界面上不画按钮)
fn link_spawned_agents(session: &str, messages: &mut [TranscriptMessage]) {
    for call in messages.iter_mut().flat_map(|m| m.tool_calls.iter_mut()) {
        if let Some((agent, _)) = call.output.as_deref().and_then(created_agent) {
            call.child_id = Some(native_id(session, agent));
        }
    }
}

/// 派活清单只读 tool 行的这两项;content 是 part 数组的对不上形状,跳过
#[derive(serde::Deserialize)]
struct ToolRow {
    role: Option<String>,
    content: Option<String>,
}

/// 一份转录派出去的 agent → 派它时起的标题(组织者是"笔记工具并行调研",助手是"合并Word对比表";
/// 没起标题的记空串,谁派的照样认得出)。只有含 `create_agent ok` 的行才解析
fn spawned_titles(path: &Path) -> io::Result<HashMap<String, String>> {
    let reader = BufReader::with_capacity(1 << 20, fs::File::open(path)?);
    let mut titles = HashMap::new();
    for row in jsonl_rows_containing::<ToolRow, _>(reader, CREATED) {
        let Some(row) = row? else {
            continue;
        };
        if row.role.as_deref() != Some("tool") {
            continue;
        }
        if let Some((agent, title)) = row.content.as_deref().and_then(created_agent) {
            titles.insert(agent.to_string(), title.to_string());
        }
    }
    Ok(titles)
}

fn read_trajectory(path: &Path, decode_images: bool) -> Result<(Vec<TranscriptMessage>, u32)> {
    let _image_budget = transcript_image_decode_budget(decode_images);
    let reader = BufReader::with_capacity(1 << 20, fs::File::open(path)?);
    parse_trajectory(reader, decode_images)
}

/// 一份转录逐行解码(seq 由调用方在配上 assignment.md 之后编)。助手在工具结果前后的几行并成一条
/// 回复(OpenAI 形的一个回合拆在好几行里,Hermes 同款);工具结果按 `tool_call_id` 回填到调用上
fn parse_trajectory<R: io::BufRead>(
    reader: R,
    decode_images: bool,
) -> Result<(Vec<TranscriptMessage>, u32)> {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    // tool_call_id → (消息下标, tool_calls 下标)
    let mut calls: HashMap<String, (usize, usize)> = HashMap::new();
    let mut unknown = 0u32;
    for row in jsonl_values(reader) {
        let Some(row) = row? else {
            unknown += 1;
            continue;
        };
        let content = row.get("content").unwrap_or(&Value::Null);
        match row.get("role").and_then(Value::as_str) {
            Some("user") => {
                let parsed = content_parts(content, decode_images);
                if parsed.text.is_empty() && parsed.images.is_empty() {
                    continue;
                }
                let mut message = text_msg(Role::User, &parsed.text, 0);
                message.images = parsed.images;
                messages.push(message);
            }
            Some("assistant") => {
                let parsed = content_parts(content, decode_images);
                let tools = tool_calls(&row);
                if parsed.text.is_empty() && parsed.images.is_empty() && tools.is_empty() {
                    continue;
                }
                let ix = merge_into_last_assistant(&mut messages, 0, parsed, "");
                for call in tools {
                    let slot = (ix, messages[ix].tool_calls.len());
                    if !call.id.is_empty() {
                        calls.insert(call.id.clone(), slot);
                    }
                    messages[ix].tool_calls.push(call);
                }
            }
            Some("tool") => {
                let slot = text_at(&row, "tool_call_id").and_then(|id| calls.get(id).copied());
                let Some((mi, ti)) = slot else {
                    continue;
                };
                let parsed = tool_result_parts(content, decode_images);
                let message = &mut messages[mi];
                if !parsed.text.is_empty() {
                    message.tool_calls[ti].output = Some(clip(&parsed.text, MAX_TOOL_IO).0);
                }
                append_images_to_message_end(message, parsed.images);
            }
            _ => unknown += 1,
        }
    }
    Ok((messages, unknown))
}

/// assistant 的 `tool_calls`:`[{id, type, function: {name, arguments}}]`,arguments 是对象;写成
/// JSON 字符串的也解开
fn tool_calls(row: &Value) -> Vec<ToolCallView> {
    row.get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| {
            let function = item.get("function").unwrap_or(item);
            let args = function.get("arguments").unwrap_or(&Value::Null);
            tool_call_view(
                text_at(item, "id").unwrap_or_default().to_string(),
                text_at(function, "name").unwrap_or_default(),
                &decoded_arguments(args),
                None,
                false,
            )
        })
        .collect()
}

impl AgentAdapter for DoubaoAdapter {
    fn agent(&self) -> AgentId {
        AgentId::DoubaoWork
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let mut seen = HashSet::new();
        for session in self.session_dirs()? {
            let Some(plan) = self
                .plan(&session)
                .map_err(|e| anyhow!("{}: {e}", session.display()))?
            else {
                continue;
            };
            for (ix, run) in plan.runs.iter().enumerate() {
                seen.insert(run.trajectory.clone());
                refs.push(plan.file_ref(ix));
            }
        }
        self.assignments
            .retain(|path| seen.contains(&path.with_file_name(TRAJECTORY)));
        self.spawned.retain(|path| seen.contains(path));
        Ok(refs)
    }

    /// 转录本身;assignment.md 换成同目录的转录
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let (plan, ix) = self.locate(path).ok()??;
        Some(plan.file_ref(ix))
    }

    /// 会话里多了 / 少了一个 agent:别的 agent 是不是主代理、挂在谁下面可能跟着变,它们的引用要重比。
    /// assignment.md 每轮追问都会写、头只在建 agent 时写一次,不为它重比——父子关系由
    /// `is_snapshot_event` 当场刷新,引用里的其余部分等全量扫描
    fn ref_dependents(&self, path: &Path) -> Vec<PathBuf> {
        let Some((session, _)) = run_of(path) else {
            return Vec::new();
        };
        subdirs(&session.join(AGENTS))
            .unwrap_or_default()
            .into_iter()
            .map(|dir| dir.join(SYSTEM).join(TRAJECTORY))
            .filter(|run| run != path && stat_stamp(run).is_some_and(|(_, size)| size > 0))
            .collect()
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages, unknown) = self.parse(r, false)?;
        Ok(ParsedSession::derive(meta, &messages, unknown))
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, mainline, unknown_line_count) = self.parse(r, true)?;
        Ok(ParsedTranscript {
            meta,
            mainline,
            sidechains: Vec::new(),
            unknown_line_count,
        })
    }

    /// 只隐藏:会话目录在豆包自己的 profile 里,移走它豆包会怎样没核实过
    fn session_paths(&self, _meta: &SessionMeta) -> Vec<String> {
        Vec::new()
    }

    fn manages_parent_links(&self) -> bool {
        true
    }

    /// 每个会话里主代理之外的 agent 挂到派它的那个下面(scanner 再压到主代理上)
    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        let mut pairs = Vec::new();
        for session in self.session_dirs().ok()? {
            // 读不出就是不知道:整份交 None,库里的关系原样保留
            let Some(plan) = self.plan(&session).ok()? else {
                continue;
            };
            for ix in 0..plan.runs.len() {
                if let Some(parent) = plan.parent(ix) {
                    pairs.push((plan.id(ix), plan.id(parent)));
                }
            }
        }
        Some(super::link_keys(
            AgentId::DoubaoWork,
            pairs
                .iter()
                .map(|(child, parent)| (child.as_str(), parent.as_str())),
        ))
    }

    /// 关系出自子会话自己的 assignment.md(`Assigned by`)与同会话的转录
    fn parent_links_in_child(&self) -> bool {
        true
    }

    /// 挂上的都是派出去的。assignment.md 后写下的头让别的 agent 有了上级时,它们的文件没动、不会
    /// 当场重解析(见 `ref_dependents`),靠关系同步补标
    fn linked_children_are_spawned(&self) -> bool {
        true
    }

    /// 角色与上级写在 assignment.md 里
    fn is_snapshot_event(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == ASSIGNMENT)
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        Box::new(Self::with_roots(custom_roots(dir)))
    }

    /// profile 之间互不相干,可以单独停用
    fn supports_individual_root_removal(&self) -> bool {
        self.roots.len() > 1
    }

    fn excluding_data_roots(&self, roots: &[PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        let kept = self
            .roots
            .iter()
            .filter(|root| !roots.contains(root))
            .cloned()
            .collect();
        Some(Box::new(Self::with_roots(kept)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 豆包电脑版 2.37.16 写下的 assignment.md:只有一个 agent 的会话没有头,问了两句
    const ASKED_TWICE: &str = "## [2026-11-23T08:17:14.767849979Z] 需求\n你是谁\n\
                               ## [2026-11-23T08:18:58.115801238Z] 需求\n写一首关于秋天的短诗，保存成 Word 文档\n";

    /// 派了组织者的会话里,组织者的 assignment.md(同一个版本,内容截短)
    const ORGANIZER: &str = "# Assignment\n- Agent ID: o_000VAghDiLg\n- Role: organizer\n\
                             - Assigned by: m_0cweNMSf6TS\n- Created: 2026-11-23T08:37:23.373419431Z\n\n\
                             > append-only: 每个 turn 的需求按时间戳追加在下方，最新在末尾。\n\n\
                             ## [2026-11-23T08:37:23.373419431Z] 需求\n<system-reminder> The user's original query was: 调研三款笔记工具\n</system-reminder>\n";

    fn run(agent: &str, assignment: &str) -> Run {
        Run {
            trajectory: PathBuf::from(agent),
            agent: agent.to_string(),
            stamp: (0, 1),
            assignment_stamp: None,
            assignment: Some(Arc::new(Assignment::parse(assignment))),
        }
    }

    #[test]
    fn assignments_have_a_header_and_sections_of_requests() {
        let asked = Assignment::parse(ASKED_TWICE);
        assert_eq!((asked.role.as_str(), asked.assigned_by.as_str()), ("", ""));
        assert_eq!(
            asked.sections,
            [
                Section {
                    ts: iso_ms("2026-11-23T08:17:14.767Z"),
                    body: "你是谁".into(),
                },
                Section {
                    ts: iso_ms("2026-11-23T08:18:58.115Z"),
                    body: "写一首关于秋天的短诗，保存成 Word 文档".into(),
                },
            ]
        );
        let organizer = Assignment::parse(ORGANIZER);
        assert_eq!(organizer.role, "organizer");
        assert_eq!(organizer.assigned_by, "m_0cweNMSf6TS");
        assert_eq!(organizer.created, iso_ms("2026-11-23T08:37:23.373Z"));
        assert_eq!(organizer.sections.len(), 1);
        assert!(organizer.sections[0].body.starts_with("<system-reminder>"));
        // 粗体、全角冒号;不是时间的方括号是正文
        let odd = Assignment::parse(
            "* **Role**：Main\n## [2026-11-23T08:00:00Z] 任务\n第一行\n\n第二行\n## [不是时间] 备注\n",
        );
        assert!(odd.is_main());
        assert_eq!(odd.sections[0].body, "第一行\n\n第二行\n## [不是时间] 备注");
    }

    #[test]
    fn the_first_request_leads_and_follow_ups_take_their_times() {
        let sections = Assignment::parse(ASKED_TWICE).sections;
        let reply = |text: &str| text_msg(Role::Assistant, text, 0);
        // 转录从回答开始,追问是一条 user 行
        let merged = with_requests(
            &sections,
            vec![
                reply("我是你的 AI 助手"),
                text_msg(Role::User, "写一首关于秋天的短诗，保存成 Word 文档", 0),
                reply("完成了"),
            ],
        );
        let shape: Vec<(Role, &str, Option<i64>)> = merged
            .iter()
            .map(|m| (m.role, m.text.as_str(), m.timestamp))
            .collect();
        assert_eq!(
            shape,
            [
                (Role::User, "你是谁", Some(sections[0].ts)),
                (Role::Assistant, "我是你的 AI 助手", None),
                (
                    Role::User,
                    "写一首关于秋天的短诗，保存成 Word 文档",
                    Some(sections[1].ts)
                ),
                (Role::Assistant, "完成了", None),
            ]
        );
        // 转录自己就以第一句开头的,不重复
        let echoed = with_requests(
            &sections,
            vec![text_msg(Role::User, "你是谁", 0), reply("你好")],
        );
        assert_eq!(echoed.len(), 2);
        assert_eq!(echoed[0].timestamp, Some(sections[0].ts));
    }

    #[test]
    fn the_main_agent_and_who_assigned_whom() {
        // 头里写明的:组织者派了助手,助手挂在组织者下面
        let runs = vec![
            run("m_main", "- Role: main\n- Assigned by: user\n"),
            run("o_org", "- Role: organizer\n- Assigned by: m_main\n"),
            run("s_help", "- Role: subagent\n- Assigned by: o_org\n"),
            run("s_lost", "- Role: subagent\n- Assigned by: s_cloud\n"),
            run(
                "s_orphan",
                "## [2026-11-23T08:47:35Z] 需求\n<system-reminder>执行模式</system-reminder>\n",
            ),
        ];
        let main = Plan::main_of(&runs);
        let mut spawned = HashMap::new();
        // 没有头的助手:组织者的 create_agent 派了它
        spawned.insert("s_orphan".to_string(), (1, "飞书文档 工具简报".to_string()));
        spawned.insert("s_help".to_string(), (1, String::new()));
        let plan = Plan {
            session: "38455712340636674".into(),
            runs,
            main,
            spawned,
        };
        assert_eq!(plan.main, Some(0));
        let parents: Vec<Option<usize>> = (0..5).map(|ix| plan.parent(ix)).collect();
        // 点名的上级不在本地(云端跑的助手)就挂主代理;没有头的看谁的 create_agent 派了它
        assert_eq!(parents, [None, Some(0), Some(1), Some(0), Some(1)]);
        assert_eq!(plan.spawned_title(4), Some("飞书文档 工具简报"));
        // 派的时候没起标题:不算有标题
        assert_eq!(plan.spawned_title(2), None);
        assert_eq!(plan.id(4), "38455712340636674/s_orphan");
        // 没有头:m_ 起头的恰好一个、或者只有一个 agent
        assert_eq!(Plan::main_of(&[run("a_x", ""), run("m_y", "")]), Some(1));
        assert_eq!(Plan::main_of(&[run("a_x", "")]), Some(0));
        assert_eq!(Plan::main_of(&[run("a_x", ""), run("a_y", "")]), None);
        assert_eq!(Plan::main_of(&[run("m_x", ""), run("m_y", "")]), None);
    }

    #[test]
    fn create_agent_results_name_and_link_the_child() {
        let output = "create_agent ok\nagent_id: o_000VAghDiLg\nparent_id: main_agent\n\
                      session_id: o_000VAghDiLg\ntitle: 笔记工具并行调研\nagent_type: organizer";
        assert_eq!(
            created_agent(output),
            Some(("o_000VAghDiLg", "笔记工具并行调研"))
        );
        assert_eq!(
            created_agent("create_agent ok\nagent_id: s_1\ntitle: \n"),
            Some(("s_1", ""))
        );
        assert_eq!(created_agent("wait_agent ok\nagent_id: s_1"), None);
        let mut messages = vec![text_msg(Role::Assistant, "派一个组织者", 0)];
        let mut call = tool_call_view("c1".into(), "create_agent", &Value::Null, None, false);
        call.output = Some(output.into());
        messages[0].tool_calls.push(call);
        link_spawned_agents("38455712340636674", &mut messages);
        assert_eq!(
            messages[0].tool_calls[0].child_id.as_deref(),
            Some("38455712340636674/o_000VAghDiLg")
        );
    }

    #[test]
    fn events_map_to_the_agent_transcript() {
        let trajectory: PathBuf = [
            "/x",
            ".sessions",
            "38455902111943938",
            AGENTS,
            "m_0cweC6udWVG",
            SYSTEM,
            TRAJECTORY,
        ]
        .iter()
        .collect();
        let (session, agent) = run_of(&trajectory).unwrap();
        assert!(session.ends_with("38455902111943938"));
        assert_eq!(agent, "m_0cweC6udWVG");
        assert_eq!(
            trajectory_for(&assignment_of(&trajectory)),
            Some(trajectory.clone())
        );
        // agent 目录里 agent 自己写的文件、system 之外的层都不是转录
        let agent_dir = trajectory.parent().unwrap().parent().unwrap();
        assert_eq!(run_of(&agent_dir.join(TRAJECTORY)), None);
        assert_eq!(run_of(&agent_dir.join("scratch").join(TRAJECTORY)), None);
        assert_eq!(trajectory_for(&agent_dir.join("make_poem_docx.py")), None);
    }

    #[test]
    fn custom_roots_take_their_shape_from_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let user_data = tmp.path().join("Doubao");
        let profile = user_data.join("Profile 2");
        let root = sessions_root(&profile);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(user_data.join("Crashpad")).unwrap();

        // 名字认得出的几层
        assert_eq!(custom_roots(root.clone()), vec![root.clone()]);
        assert_eq!(custom_roots(profile.join(".doubao")), vec![root.clone()]);
        assert_eq!(
            custom_roots(profile.join(".doubao").join("agent_mode").join("workspace")),
            vec![root.clone()]
        );
        // 一个 profile、装着 profile 的用户数据目录
        assert_eq!(custom_roots(profile.clone()), vec![root.clone()]);
        assert_eq!(custom_roots(user_data.clone()), vec![root]);
        // 别的(会话根的拷贝、还不存在的目录)当会话根本身
        let copy = tmp.path().join("backup");
        assert_eq!(custom_roots(copy.clone()), vec![copy]);
    }
}
