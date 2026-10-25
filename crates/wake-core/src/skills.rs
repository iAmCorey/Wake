//! 本机 agent skill 的只读总览(issue #62)。skill 是 `<目录>/<名字>/SKILL.md`(YAML
//! frontmatter + Markdown 正文),各家 agent 各收在自己的目录里(`~/.claude/skills`、
//! `~/.codex/skills`……),另有多家共读的 `~/.agents/skills`(vercel-labs 的 `npx skills`
//! 把它当正本、往各家目录里链)。这里只做"看":按名字把各处的安装归拢成一个 skill,看出
//! 哪些是同一份(符号链接到同一个真实目录)、哪些是各自的拷贝、拷贝之间 SKILL.md 有没有
//! 分叉。**只读**(不变量 2):不安装、不拷贝、不链接、不同步、不删除——跨 agent 装 skill
//! 是 `npx skills add` 的事(用户 2026-10-15 定)。
//!
//! 取舍:
//! - 不进索引库、不建 FTS:skill 只有几百份,进页与 Refresh 时现扫,SKILL.md 的解析结果按
//!   (mtime, size) 缓存在 `SkillCache`。
//! - **只读每个条目顶层的 SKILL.md,绝不往 skill 目录里递归**:有的 skill 是整个仓库的检出
//!   (本机 `~/.claude/skills/gstack` 1.3 GB)。"内容一致"因此只比 SKILL.md 的哈希,有意不比
//!   整个目录;点开头的条目(`.system` 一类)也不看。唯一的下探是 Hermes 的分类目录。
//! - 目录从 roster 派生(不变量 8,不另建名单):各家用户级目录挂在本地实例**确认过的 home**
//!   下(`AgentAdapter::homes`,`CODEX_HOME` / `QODER_CONFIG_DIR` 这类在构造时已解析;直接选了
//!   数据目录的自定义根没有 home,不越界摸父目录),远程实例不看;家目录 / 配置目录下的固定
//!   位置与项目级目录是各家静态的事实,与 roster 无关(该家会话 location 全停用了照样列)。
//!   归属指"文件夹是谁家的",不推算"谁读得到"——十几家都读 `~/.agents/skills`、好几家读
//!   `~/.claude/skills`,各家读哪些随版本在变。
//! - "读不出" ≠ "没有":目录或 SKILL.md 不存在(NotFound / 不是目录)就是没有;别的 I/O
//!   错误记进 `SkillScan::errors`,界面照实提示,不当成空。

use crate::adapters::parse_utils::{mtime_ms, project_name_of};
use crate::adapters::{self, AgentAdapter};
use crate::models::AgentId;
use crate::text;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// skill 的入口文件名(Agent Skills 规范)
const SKILL_MD: &str = "SKILL.md";

/// 多家共读的 skill 文件夹:家目录下(`~/.agents/skills`)与每个项目根下同一个相对路径
pub const SHARED_DIR: &str = ".agents/skills";
/// `npx skills` 新版的落点:`$XDG_CONFIG_HOME/agents/skills`
const SHARED_CONFIG_DIR: &str = "agents/skills";

/// 一个 skill 文件夹归谁:共享目录(`.agents/skills`,多家共读),或某一家 agent 自己的
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SkillOwner {
    Shared,
    Agent(AgentId),
}

/// 一处安装的层级。派生的 Ord 即展示序:用户级、插件、项目级
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SkillScope {
    /// home 下的(`~/.claude/skills`、`~/.agents/skills`……)
    User,
    /// Claude Code 插件自带的;`id` 是 `installed_plugins.json` 的键(`<插件>@<市场>`),
    /// `enabled` 读自 settings.json 的 `enabledPlugins`(没写的算开着)
    Plugin { id: String, enabled: bool },
    /// 已索引项目根下的(`<项目>/.claude/skills`……)
    Project { root: PathBuf },
}

impl SkillScope {
    /// 插件名(`id` 里 `@` 前那段),skill 在 Claude Code 里叫 `<插件>:<名字>`
    pub fn plugin_name(&self) -> Option<&str> {
        match self {
            SkillScope::Plugin { id, .. } => Some(id.split('@').next().unwrap_or(id)),
            _ => None,
        }
    }
}

/// 一处安装:某个 skill 文件夹里的一个条目
#[derive(Debug, Clone)]
pub struct SkillInstall {
    pub owner: SkillOwner,
    pub scope: SkillScope,
    /// 列出来的条目目录 `<skill 文件夹>/<条目名>`
    pub path: PathBuf,
    /// 解析掉符号链接后的真实目录:"同一份" = 同一个它
    pub real_path: PathBuf,
    /// 条目本身或它所在的 skill 文件夹是符号链接
    pub linked: bool,
    /// SKILL.md 的 sha256 前 8 字节:"内容一致" = 它相等(只比 SKILL.md,见模块文档)
    pub content_hash: u64,
    /// frontmatter 的 `name`,没有就用目录名;插件的加 `<插件>:` 前缀
    pub name: String,
    /// frontmatter 的 `description`,没有给空串
    pub description: String,
    /// SKILL.md 的修改时间(epoch ms)
    pub updated_at: i64,
}

impl SkillInstall {
    pub fn skill_md(&self) -> PathBuf {
        self.path.join(SKILL_MD)
    }
}

/// 同名的安装归成一个 skill。汇总字段在扫描时算好一次(列表每帧都要读)
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    /// 按 (归属, 层级, 路径) 排好
    pub installs: Vec<SkillInstall>,
    /// 几份真实拷贝(符号链接指向同一处的算一份)
    pub copies: usize,
    /// 各处 SKILL.md 内容不全一样
    pub differs: bool,
    /// 有一处是符号链接
    pub linked: bool,
    /// 有它的各方(去重、按 SkillOwner 序)
    pub owners: Vec<SkillOwner>,
    /// 有一处是插件自带的(列表行挂 "plugin" 徽章)
    pub plugin: bool,
    /// 最近改过的那处(`installs` 下标,平局取靠前的);列表的说明与阅读面默认显示它
    pub newest: usize,
    /// 折叠过大小写的名字与各处说明(`descriptions` 与 `installs` 一一对应):搜索拿它比,
    /// 扫描时折一次,不必每次输入把几百份说明再折一遍
    folded: FoldedSkill,
}

#[derive(Debug, Clone)]
struct FoldedSkill {
    name: String,
    descriptions: Vec<String>,
}

impl Skill {
    fn new(name: String, mut installs: Vec<SkillInstall>) -> Self {
        installs.sort_by(|a, b| (a.owner, &a.scope, &a.path).cmp(&(b.owner, &b.scope, &b.path)));
        let mut newest = 0;
        for (ix, install) in installs.iter().enumerate() {
            if install.updated_at > installs[newest].updated_at {
                newest = ix;
            }
        }
        // installs 先按归属排,相邻去重就是各方
        let mut owners: Vec<SkillOwner> = installs.iter().map(|i| i.owner).collect();
        owners.dedup();
        let folded = FoldedSkill {
            name: text::fold_case(&name),
            descriptions: installs
                .iter()
                .map(|i| text::fold_case(&i.description))
                .collect(),
        };
        Self {
            copies: installs
                .iter()
                .map(|i| &i.real_path)
                .collect::<HashSet<_>>()
                .len(),
            differs: installs
                .iter()
                .any(|i| i.content_hash != installs[0].content_hash),
            linked: installs.iter().any(|i| i.linked),
            plugin: installs
                .iter()
                .any(|i| matches!(i.scope, SkillScope::Plugin { .. })),
            owners,
            newest,
            folded,
            name,
            installs,
        }
    }

    pub fn description(&self) -> &str {
        &self.installs[self.newest].description
    }

    pub fn updated_at(&self) -> i64 {
        self.installs[self.newest].updated_at
    }
}

/// 读不出来的位置(不是"没有"):路径 + 英文错误
#[derive(Debug, Clone)]
pub struct SkillError {
    pub path: PathBuf,
    pub message: String,
}

impl SkillError {
    fn new(path: &Path, e: impl std::fmt::Display) -> Self {
        Self {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillScan {
    /// 按名字排(不分大小写)
    pub skills: Vec<Skill>,
    pub errors: Vec<SkillError>,
}

/// 侧栏导航的计数(都是 skill 数,不是安装数)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCounts {
    pub total: usize,
    /// 有两份及以上真实拷贝的
    pub duplicated: usize,
    /// 按 SkillOwner 序
    pub owners: Vec<(SkillOwner, usize)>,
    /// 有项目级 skill 的项目,按项目名排
    pub projects: Vec<(PathBuf, usize)>,
}

impl SkillScan {
    pub fn counts(&self) -> SkillCounts {
        let mut owners: BTreeMap<SkillOwner, usize> = BTreeMap::new();
        let mut projects: BTreeMap<&Path, usize> = BTreeMap::new();
        for skill in &self.skills {
            for &owner in &skill.owners {
                *owners.entry(owner).or_default() += 1;
            }
            let roots: BTreeSet<&Path> = skill
                .installs
                .iter()
                .filter_map(|i| match &i.scope {
                    SkillScope::Project { root } => Some(root.as_path()),
                    _ => None,
                })
                .collect();
            for root in roots {
                *projects.entry(root).or_default() += 1;
            }
        }
        let mut projects: Vec<(PathBuf, usize)> = projects
            .into_iter()
            .map(|(root, n)| (root.to_path_buf(), n))
            .collect();
        projects.sort_by_cached_key(|(root, _)| (project_name(root).to_lowercase(), root.clone()));
        SkillCounts {
            total: self.skills.len(),
            duplicated: self.skills.iter().filter(|s| s.copies > 1).count(),
            owners: owners.into_iter().collect(),
            projects,
        }
    }

    /// 按名字找回一个 skill:重扫后下标会变,界面记的是名字
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.skills.iter().position(|s| s.name == name)
    }

    /// 按名字与说明找 skill(⌘K 面板的 Skills 栏;SKILL.md 正文不搜):与会话、记忆同一套分词,
    /// 每个词都得出现在名字或某一处安装的说明里,大小写折叠与片段的高亮同一个取法。排序:词全在
    /// 名字里的在前,其中名字越短越靠前(名字恰好就是查询的排第一);其余按名字里缺几个词;同档
    /// 按名字
    pub fn search(&self, q: &str) -> Vec<SkillHit> {
        let terms = text::fts_terms(q);
        if terms.is_empty() {
            return Vec::new();
        }
        let needles: Vec<String> = terms.iter().map(|t| text::fold_case(t)).collect();
        let mut ranked: Vec<((usize, usize), SkillHit)> = Vec::new();
        for (ix, skill) in self.skills.iter().enumerate() {
            let folded = &skill.folded;
            let described = |i: usize, needle: &str| folded.descriptions[i].contains(needle);
            // 名字里没有的词,都得在某一处的说明里
            let unnamed: Vec<&str> = needles
                .iter()
                .map(String::as_str)
                .filter(|needle| !folded.name.contains(needle))
                .collect();
            if !unnamed
                .iter()
                .all(|needle| (0..skill.installs.len()).any(|i| described(i, needle)))
            {
                continue;
            }
            // 显示、打开哪一处(片段也取自它):名字里没有的那些词,说明里凑得最齐的那一处;一样齐的
            // 取最近改过的那份,再按排序在前的。只看"有没有命中",搜 staging cluster 会落到只写着
            // staging 的最新那份上(Codex review 2026-10-25)
            let covered = |i: usize| unnamed.iter().filter(|needle| described(i, needle)).count();
            let (mut install, mut best) = (skill.newest, covered(skill.newest));
            for i in 0..skill.installs.len() {
                let n = covered(i);
                if n > best {
                    (install, best) = (i, n);
                }
            }
            let length = if unnamed.is_empty() {
                skill.name.chars().count()
            } else {
                0
            };
            ranked.push((
                (unnamed.len(), length),
                SkillHit {
                    skill: ix,
                    install,
                    snippet: text::make_snippet(&skill.installs[install].description, &terms),
                },
            ));
        }
        // 稳定排序:同档保持 skills 的顺序,即按名字
        ranked.sort_by_key(|(rank, _)| *rank);
        ranked.into_iter().map(|(_, hit)| hit).collect()
    }
}

/// skill 搜索的一条命中(`SkillScan::search`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHit {
    /// `SkillScan::skills` 的下标
    pub skill: usize,
    /// 片段取自哪一处安装的说明(`Skill::installs` 的下标),打开时显示的也是它
    pub install: usize,
    /// 说明的片段,带 HL_OPEN / HL_CLOSE 高亮;没有说明是空串
    pub snippet: String,
}

/// 项目根的显示名(与会话、记忆的项目名同一个取法)
pub fn project_name(root: &Path) -> String {
    project_name_of(&root.to_string_lossy())
}

// ---------------- 位置:各家的 skill 文件夹在哪 ----------------

/// 一家的用户级 skill 文件夹怎么推。依据各家官方文档 / 源码(2026-10-15 核对),只收 Wake
/// 名单里的 agent 自己的文件夹;没核实的(Kimi Code 文档写 `~/.kimi/skills`,而 Wake 读的
/// Kimi Code 数据在 `~/.kimi-code`,对不上;WorkBuddy 只有第三方说法;Grok Bot 没有本地目录)
/// 不猜
enum UserDirs {
    /// 本地实例确认过的 home(`AgentAdapter::homes`)下的相对路径;里面的 `*` 段是"每个
    /// 子目录"(Craft 的每个工作区)
    FromHome(&'static str),
    /// 固定位置:该家没有改 home 的开关,或 skill 不随它走(ZCode 的 skill 恒在
    /// `homedir()/.zcode/skills`,`ZCODE_STORAGE_DIR` 只挪数据)
    Fixed(Base, &'static [&'static str]),
}

#[derive(Clone, Copy)]
enum Base {
    Home,
    /// `$XDG_CONFIG_HOME`(缺省 `~/.config`)
    Config,
    /// 应用配置目录:Windows 是 `%APPDATA%`,其余同 Config(Devin)
    AppConfig,
}

fn user_dirs(agent: AgentId) -> Option<UserDirs> {
    use UserDirs::*;
    Some(match agent {
        // `$CODEX_HOME/skills` 官方已标弃用(改读 ~/.agents/skills)但仍读;`.system` 是
        // Codex 自带的,点开头的条目不看
        AgentId::ClaudeCode
        | AgentId::Codex
        | AgentId::Qoder
        | AgentId::Cursor
        | AgentId::Codebuddy
        | AgentId::Grok
        | AgentId::Dsh
        | AgentId::Pi
        | AgentId::Omp
        | AgentId::Kiro
        | AgentId::Gemini
        | AgentId::Copilot
        // HERMES_HOME 与各 profile 各一个
        | AgentId::Hermes
        | AgentId::Openclaw => FromHome("skills"),
        // 每个工作区一个
        AgentId::CraftAgents => FromHome("workspaces/*/skills"),
        // 新版桌面端 / IDE、CLI,与旧版(本机还有)
        AgentId::Antigravity => Fixed(
            Base::Home,
            &[
                ".gemini/config/skills",
                ".gemini/antigravity-cli/skills",
                ".gemini/antigravity/skills",
            ],
        ),
        AgentId::Zcode => Fixed(Base::Home, &[".zcode/skills"]),
        // 新版与旧版扩展
        AgentId::Kilo => Fixed(Base::Home, &[".kilo/skills", ".kilocode/skills"]),
        AgentId::Opencode => Fixed(Base::Config, &["opencode/skills"]),
        // OpenCode 分支,同一套 xdg-basedir(MIMOCODE_HOME 改道不跟,OpenCode 的 OPENCODE_CONFIG_DIR
        // 同样);它还读 `skill/` 单数与 `~/.mimocode`,常用的是这一处
        AgentId::MimoCode => Fixed(Base::Config, &["mimocode/skills"]),
        AgentId::Devin => Fixed(Base::AppConfig, &["devin/skills"]),
        // app 内置与用户装的 skill 都落在这里(本机核对,每个子目录一份 SKILL.md)
        AgentId::QwenWork => Fixed(Base::Home, &[".qwenworkcn/skills"]),
        AgentId::Kimi | AgentId::GrokBot | AgentId::Workbuddy => return None,
    })
}

/// 一家在项目根下的 skill 文件夹(相对路径)。共享的 `.agents/skills` 单列(`SHARED_DIR`)
fn project_dirs(agent: AgentId) -> &'static [&'static str] {
    match agent {
        AgentId::ClaudeCode => &[".claude/skills"],
        AgentId::Codex => &[".codex/skills"],
        AgentId::Qoder => &[".qoder/skills"],
        AgentId::Copilot => &[".github/skills"],
        AgentId::Cursor => &[".cursor/skills"],
        AgentId::Opencode => &[".opencode/skills"],
        AgentId::Kiro => &[".kiro/skills"],
        AgentId::Gemini => &[".gemini/skills"],
        AgentId::Pi => &[".pi/skills"],
        AgentId::Omp => &[".omp/skills"],
        AgentId::Grok => &[".grok/skills"],
        // 旧版;新版读的是共享的 .agents/skills
        AgentId::Antigravity => &[".agent/skills"],
        AgentId::Dsh => &[".dsh/skills"],
        AgentId::Hermes => &[".hermes/skills"],
        AgentId::Codebuddy => &[".codebuddy/skills"],
        AgentId::Zcode => &[".zcode/skills"],
        AgentId::Devin => &[".devin/skills"],
        AgentId::Kilo => &[".kilo/skills"],
        AgentId::MimoCode => &[".mimocode/skills"],
        AgentId::Openclaw
        | AgentId::CraftAgents
        | AgentId::Kimi
        | AgentId::GrokBot
        | AgentId::Workbuddy
        | AgentId::QwenWork => &[],
    }
}

/// 这家的 skill 文件夹是分类目录布局:没有 SKILL.md 的条目是一个分类,往下找(Hermes 的
/// `<分类>/<skill>/SKILL.md`,它自己按 os.walk 发现;mlops 下还有一层)
fn has_categories(owner: SkillOwner) -> bool {
    owner == SkillOwner::Agent(AgentId::Hermes)
}

/// 分类目录最多往下找几层(到有 SKILL.md 的目录即停,绝不进 skill 目录里面)
const CATEGORY_DEPTH: usize = 3;

/// 推固定位置用的两个根。`current()` 读进程环境;测试用 `at(假 home)`,不碰环境变量
#[derive(Debug, Clone)]
pub struct SkillHomes {
    home: PathBuf,
    /// `$XDG_CONFIG_HOME`,缺省 `<home>/.config`
    config: PathBuf,
}

impl SkillHomes {
    pub fn current() -> Self {
        let home = adapters::home_dir().unwrap_or_default();
        let config = adapters::env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Self { home, config }
    }

    pub fn at(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            config: home.join(".config"),
        }
    }

    fn base(&self, base: Base) -> PathBuf {
        match base {
            Base::Home => self.home.clone(),
            Base::Config => self.config.clone(),
            // Windows `%APPDATA%`:与 adapters 同一约定,从 home 派生
            Base::AppConfig if cfg!(windows) => self.home.join("AppData").join("Roaming"),
            Base::AppConfig => self.config.clone(),
        }
    }
}

/// `a/b` 形式的相对路径按段拼(Windows 上也对)
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(base.to_path_buf(), |p, seg| p.join(seg))
}

/// 读不读得出:不存在(含"中间某段不是目录")是没有,别的是读不出
fn absent(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// 一个待读的 skill 文件夹
struct Location {
    owner: SkillOwner,
    scope: SkillScope,
    dir: PathBuf,
    /// `dir` 的真实路径:里面不是链接的条目,真实路径就是它接上条目名,不必逐个 canonicalize
    real: PathBuf,
    /// `dir` 本身就是一个 skill(插件清单直接点名的 skill 目录),不是装 skill 的文件夹
    single: bool,
    /// `dir` 自己是符号链接(里面每一项都算链接)
    linked: bool,
}

/// 收集位置:同一归属、同一层级下同一个真实文件夹只读一次(Codex 的 sessions 与 archived 同一个
/// home、一家多个实例);不同归属或不同项目指到同一处(整个文件夹是符号链接)各列各的——那正是
/// "链接"
#[derive(Default)]
struct Plan {
    locations: Vec<Location>,
    seen: HashSet<(SkillOwner, SkillScope, PathBuf)>,
    /// 已列的用户级 / 插件文件夹的**逻辑路径**(父目录解析掉链接、末段原样):项目级展开出
    /// 同一个路径时跳过——家目录被索引成项目时,`~/.claude/skills` 不能再当一份项目级来一遍。
    /// 按真实路径比会把"项目里的 skills 目录链到 ~/.agents/skills"这种真安装也吞掉(Codex
    /// review 2026-10-15)
    covered: HashSet<PathBuf>,
    errors: Vec<SkillError>,
}

/// 父目录解析掉链接、末段原样:同一个文件夹换个写法(经由链接的家目录)认得出,而一个本身
/// 是链接的 skills 目录仍是它自己
fn logical_path(dir: &Path) -> PathBuf {
    match (dir.parent(), dir.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| dir.to_path_buf()),
        _ => dir.to_path_buf(),
    }
}

impl Plan {
    fn add(&mut self, owner: SkillOwner, scope: SkillScope, dir: PathBuf) {
        self.add_location(owner, scope, dir, false);
    }

    fn add_location(&mut self, owner: SkillOwner, scope: SkillScope, dir: PathBuf, single: bool) {
        // 先 lstat:项目级展开问的大多是不存在的路径,一次系统调用就够,是不是链接顺带知道;
        // canonicalize 只给真有的
        let meta = match std::fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if absent(&e) => return,
            Err(e) => return self.errors.push(SkillError::new(&dir, e)),
        };
        let linked = meta.file_type().is_symlink();
        if !linked && !meta.is_dir() {
            return;
        }
        let real = match std::fs::canonicalize(&dir) {
            Ok(real) if real.is_dir() => real,
            Ok(_) => return,
            Err(e) if absent(&e) => return,
            Err(e) => return self.errors.push(SkillError::new(&dir, e)),
        };
        // 不是链接的文件夹,逻辑路径就是真实路径
        let logical = if linked {
            logical_path(&dir)
        } else {
            real.clone()
        };
        let project = matches!(scope, SkillScope::Project { .. });
        if project && self.covered.contains(&logical) {
            return;
        }
        if !self.seen.insert((owner, scope.clone(), real.clone())) {
            return;
        }
        if !project {
            self.covered.insert(logical);
        }
        self.locations.push(Location {
            owner,
            scope,
            dir,
            real,
            single,
            linked,
        });
    }

    /// `rel` 里的 `*` 段展开成每个子目录(不看点开头的)
    fn add_under_home(&mut self, owner: SkillOwner, home: &Path, rel: &str) {
        let Some((parent, rest)) = rel.split_once("/*/") else {
            return self.add(owner, SkillScope::User, join_rel(home, rel));
        };
        let parent = join_rel(home, parent);
        match sorted_entries(&parent) {
            Ok(names) => {
                for (name, _) in names {
                    self.add(owner, SkillScope::User, join_rel(&parent.join(name), rest));
                }
            }
            Err(e) if absent(&e) => {}
            Err(e) => self.errors.push(SkillError::new(&parent, e)),
        }
    }
}

/// 目录里不以点开头的条目(名字, 是不是符号链接),按名字排。链接与否取自目录项本身
/// (Unix 的 d_type),不再逐个 lstat
fn sorted_entries(dir: &Path) -> std::io::Result<Vec<(std::ffi::OsString, bool)>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        names.push((name, entry.file_type()?.is_symlink()));
    }
    names.sort();
    Ok(names)
}

/// Claude Code 插件自带的 skill 文件夹:`<home>/plugins/installed_plugins.json` 里每个插件
/// 的 `installPath` 下的 `skills/`,外加清单(`.claude-plugin/plugin.json`)`skills` 字段
/// 列的路径。**只认 installPath**:插件缓存里堆着几十个旧版本(带 `.orphaned_at`),扫缓存
/// 目录会把它们全算进来
fn add_claude_plugins(plan: &mut Plan, home: &Path) {
    let index = home.join("plugins").join("installed_plugins.json");
    let text = match std::fs::read_to_string(&index) {
        Ok(text) => text,
        Err(e) if absent(&e) => return,
        Err(e) => return plan.errors.push(SkillError::new(&index, e)),
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(json) => json,
        Err(e) => return plan.errors.push(SkillError::new(&index, e)),
    };
    let Some(plugins) = json.get("plugins").and_then(|p| p.as_object()) else {
        return;
    };
    // 开关在 settings.json 的 enabledPlugins;读不出就当都开着(只是少标一个 disabled)
    let enabled: HashMap<String, bool> = std::fs::read_to_string(home.join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|s| {
            s.get("enabledPlugins")?.as_object().map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_bool()?)))
                    .collect()
            })
        })
        .unwrap_or_default();
    for (id, entries) in plugins {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        let scope = SkillScope::Plugin {
            id: id.clone(),
            enabled: enabled.get(id).copied().unwrap_or(true),
        };
        for entry in entries {
            let Some(install) = entry.get("installPath").and_then(|p| p.as_str()) else {
                continue;
            };
            for (dir, single) in plugin_skill_dirs(Path::new(install), &mut plan.errors) {
                let owner = SkillOwner::Agent(AgentId::ClaudeCode);
                plan.add_location(owner, scope.clone(), dir, single);
            }
        }
    }
}

/// (目录, 它本身是不是一个 skill):默认的 `skills/` 与清单里列的,既可以是装 skill 的文件夹,
/// 也可以直接是一个 skill 的目录——里面就有 SKILL.md 的按单个 skill 读(Codex review
/// 2026-10-15;同一个目录先按文件夹登记、再按单个 skill 来就会被去重吞掉,所以一律按内容判)
/// 清单不在就只有默认目录;在却读不出、JSON 坏了要报出来(它列的目录就看不到了),不当成
/// 没有(Codex review 2026-10-15)
fn plugin_skill_dirs(install: &Path, errors: &mut Vec<SkillError>) -> Vec<(PathBuf, bool)> {
    let mut dirs = vec![install.join("skills")];
    let manifest = install.join(".claude-plugin").join("plugin.json");
    let parsed = match std::fs::read_to_string(&manifest) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| errors.push(SkillError::new(&manifest, e)))
            .ok(),
        Err(e) if absent(&e) => None,
        Err(e) => {
            errors.push(SkillError::new(&manifest, e));
            None
        }
    };
    if let Some(listed) = parsed.and_then(|m| m.get("skills").cloned()) {
        let rels: Vec<String> = match listed {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(a) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        dirs.extend(
            rels.iter()
                .map(|r| join_rel(install, r.trim_start_matches("./").trim_end_matches('/'))),
        );
    }
    dirs.into_iter()
        .map(|dir| {
            let single = dir.join(SKILL_MD).is_file();
            (dir, single)
        })
        .collect()
}

/// 各位置的先后:共享目录、各家用户级(按 AgentId 序)、Claude Code 插件、各项目。`roster`
/// 是每个本地实例的 (agent, 确认过的 homes)
fn plan(roster: &[(AgentId, Vec<PathBuf>)], homes: &SkillHomes, projects: &[PathBuf]) -> Plan {
    let homes_of = |agent: AgentId| {
        roster
            .iter()
            .filter(move |(a, _)| *a == agent)
            .flat_map(|(_, homes)| homes)
    };
    let mut plan = Plan::default();
    plan.add(
        SkillOwner::Shared,
        SkillScope::User,
        join_rel(&homes.home, SHARED_DIR),
    );
    plan.add(
        SkillOwner::Shared,
        SkillScope::User,
        join_rel(&homes.config, SHARED_CONFIG_DIR),
    );
    for agent in AgentId::ALL {
        let owner = SkillOwner::Agent(agent);
        match user_dirs(agent) {
            Some(UserDirs::FromHome(rel)) => {
                for home in homes_of(agent) {
                    plan.add_under_home(owner, home, rel);
                }
            }
            Some(UserDirs::Fixed(base, rels)) => {
                let base = homes.base(base);
                for rel in rels {
                    plan.add(owner, SkillScope::User, join_rel(&base, rel));
                }
            }
            None => {}
        }
    }
    for home in homes_of(AgentId::ClaudeCode) {
        add_claude_plugins(&mut plan, home);
    }
    let roots: BTreeSet<&PathBuf> = projects.iter().filter(|p| p.is_absolute()).collect();
    for root in roots {
        let scope = SkillScope::Project { root: root.clone() };
        plan.add(
            SkillOwner::Shared,
            scope.clone(),
            join_rel(root, SHARED_DIR),
        );
        for agent in AgentId::ALL {
            for rel in project_dirs(agent) {
                plan.add(SkillOwner::Agent(agent), scope.clone(), join_rel(root, rel));
            }
        }
    }
    plan
}

// ---------------- 读 ----------------

/// SKILL.md 解析结果的缓存:键是真实路径(多处链到同一份只读一次),戳是 (mtime, size)。
/// 每轮扫描后只留这轮见过的
#[derive(Default)]
pub struct SkillCache(Mutex<HashMap<PathBuf, ParsedSkillMd>>);

#[derive(Clone)]
struct ParsedSkillMd {
    stamp: (i64, u64),
    name: Option<String>,
    description: Option<String>,
    hash: u64,
}

impl SkillCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get_or_read(&self, md: &Path, meta: &std::fs::Metadata) -> std::io::Result<ParsedSkillMd> {
        let stamp = (mtime_ms(meta), meta.len());
        if let Some(hit) = self.0.lock().unwrap().get(md).filter(|p| p.stamp == stamp) {
            return Ok(hit.clone());
        }
        let bytes = std::fs::read(md)?;
        let text = String::from_utf8_lossy(&bytes);
        let digest = Sha256::digest(&bytes);
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&digest[..8]);
        let parsed = ParsedSkillMd {
            stamp,
            name: adapters::frontmatter_ident(&text, "name"),
            description: adapters::frontmatter_field(&text, "description"),
            hash: u64::from_be_bytes(prefix),
        };
        self.0
            .lock()
            .unwrap()
            .insert(md.to_path_buf(), parsed.clone());
        Ok(parsed)
    }

    fn retain(&self, seen: &HashSet<PathBuf>) {
        self.0.lock().unwrap().retain(|k, _| seen.contains(k));
    }
}

/// 读各个 skill 文件夹:每个条目只 stat / 读它顶层的 SKILL.md
struct Reader<'a> {
    cache: &'a SkillCache,
    /// 这轮读到的 SKILL.md(真实路径),扫完按它修剪缓存
    seen: HashSet<PathBuf>,
    /// 已记下的安装 (归属, 层级, 条目路径):插件清单点名的 skill 也在它列的(或默认的)
    /// 文件夹里时,两条读法读到同一处,只记一次。在读的时候按实际条目去重,与清单里的先后
    /// 无关——/simplify 2026-10-23 曾挪到规划阶段按文件夹判,清单写成
    /// `["./extra/deploy", "./extra"]` 就读两遍、`skills/` 本身是 skill 时又漏掉点名的那个
    /// (Codex review 同日),改回来
    listed: HashSet<(SkillOwner, SkillScope, PathBuf)>,
    installs: Vec<SkillInstall>,
    errors: Vec<SkillError>,
}

impl Reader<'_> {
    /// `dir` 里的每个条目:有 SKILL.md 就是一个 skill;没有、而这家是分类布局就当分类往下找。
    /// `real` 是 `dir` 的真实路径,`dir_linked` 是这一路上有没有经过符号链接
    fn read_dir(
        &mut self,
        loc: &Location,
        dir: &Path,
        real: &Path,
        dir_linked: bool,
        depth: usize,
    ) {
        let names = match sorted_entries(dir) {
            Ok(names) => names,
            Err(e) if absent(&e) => return,
            Err(e) => return self.errors.push(SkillError::new(dir, e)),
        };
        for (entry, entry_link) in names {
            let path = dir.join(&entry);
            // 条目自身是不是链接单独判:文件夹是链接、里面的条目又链到别处时,真实路径得现
            // 解析(Codex review 2026-10-15);不是链接的条目,真实路径就是所在文件夹的真实
            // 路径接上条目名
            let real_path = if entry_link {
                match std::fs::canonicalize(&path) {
                    Ok(real_path) => real_path,
                    Err(e) if absent(&e) => continue,
                    Err(e) => {
                        self.errors.push(SkillError::new(&path, e));
                        continue;
                    }
                }
            } else {
                real.join(&entry)
            };
            let linked = dir_linked || entry_link;
            let missing = self.read_entry(loc, &path, &real_path, linked);
            if missing && has_categories(loc.owner) && depth + 1 < CATEGORY_DEPTH && path.is_dir() {
                self.read_dir(loc, &path, &real_path, linked, depth + 1);
            }
        }
    }

    /// 一个条目:有 SKILL.md 就记一处安装。返回"这里没有 SKILL.md"(NotFound;分类目录布局
    /// 据此往下找)
    fn read_entry(&mut self, loc: &Location, path: &Path, real_path: &Path, linked: bool) -> bool {
        let key = (loc.owner, loc.scope.clone(), path.to_path_buf());
        if self.listed.contains(&key) {
            return false;
        }
        let md = path.join(SKILL_MD);
        let meta = match std::fs::metadata(&md) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => return false,
            Err(e) if e.kind() == ErrorKind::NotFound => return true,
            Err(e) if absent(&e) => return false,
            Err(e) => {
                self.errors.push(SkillError::new(&md, e));
                return false;
            }
        };
        let real_md = real_path.join(SKILL_MD);
        let parsed = match self.cache.get_or_read(&real_md, &meta) {
            Ok(parsed) => parsed,
            Err(e) if absent(&e) => return false,
            Err(e) => {
                self.errors.push(SkillError::new(&md, e));
                return false;
            }
        };
        self.seen.insert(real_md);
        self.listed.insert(key);
        let base = parsed.name.unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let name = match loc.scope.plugin_name() {
            Some(plugin) => format!("{plugin}:{base}"),
            None => base,
        };
        self.installs.push(SkillInstall {
            owner: loc.owner,
            scope: loc.scope.clone(),
            linked,
            path: path.to_path_buf(),
            real_path: real_path.to_path_buf(),
            content_hash: parsed.hash,
            name,
            description: parsed.description.unwrap_or_default(),
            updated_at: mtime_ms(&meta),
        });
        false
    }
}

/// 扫一遍:各家本地实例的 home、固定位置与共享目录、Claude Code 插件,加上各项目的项目级
/// 目录。`projects` 是已索引的本地项目根(`Store::local_project_roots`)
pub fn scan(
    adapters: &[Box<dyn AgentAdapter>],
    projects: &[PathBuf],
    cache: &SkillCache,
) -> SkillScan {
    let roster: Vec<(AgentId, Vec<PathBuf>)> = adapters
        .iter()
        .filter(|a| a.host().is_empty())
        .map(|a| (a.agent(), a.homes()))
        .collect();
    scan_in(&roster, &SkillHomes::current(), projects, cache)
}

/// `scan` 的本体:roster 只要 (agent, 确认过的 homes),homes 显式给——测试在临时目录里搭
/// 假 home,不碰环境变量
pub fn scan_in(
    roster: &[(AgentId, Vec<PathBuf>)],
    homes: &SkillHomes,
    projects: &[PathBuf],
    cache: &SkillCache,
) -> SkillScan {
    let plan = plan(roster, homes, projects);
    let mut reader = Reader {
        cache,
        seen: HashSet::new(),
        listed: HashSet::new(),
        installs: Vec::new(),
        errors: plan.errors,
    };
    for loc in &plan.locations {
        if loc.single {
            reader.read_entry(loc, &loc.dir, &loc.real, loc.linked);
        } else {
            reader.read_dir(loc, &loc.dir, &loc.real, loc.linked, 0);
        }
    }
    cache.retain(&reader.seen);
    let mut groups: BTreeMap<(String, String), Vec<SkillInstall>> = BTreeMap::new();
    for install in reader.installs {
        groups
            .entry((install.name.to_lowercase(), install.name.clone()))
            .or_default()
            .push(install);
    }
    SkillScan {
        skills: groups
            .into_iter()
            .map(|((_, name), installs)| Skill::new(name, installs))
            .collect(),
        errors: reader.errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{HL_CLOSE, HL_OPEN};

    fn install(owner: SkillOwner, name: &str, description: &str, updated_at: i64) -> SkillInstall {
        let path = PathBuf::from(format!("/skills/{owner:?}/{name}"));
        SkillInstall {
            owner,
            scope: SkillScope::User,
            real_path: path.clone(),
            path,
            linked: false,
            content_hash: 0,
            name: name.to_string(),
            description: description.to_string(),
            updated_at,
        }
    }

    /// 与 `scan_in` 收尾一样按名字排
    fn scan_of(skills: Vec<Vec<SkillInstall>>) -> SkillScan {
        let mut skills: Vec<Skill> = skills
            .into_iter()
            .map(|installs| Skill::new(installs[0].name.clone(), installs))
            .collect();
        skills.sort_by_key(|s| s.name.to_lowercase());
        SkillScan {
            skills,
            errors: Vec::new(),
        }
    }

    fn names(scan: &SkillScan, q: &str) -> Vec<String> {
        scan.search(q)
            .iter()
            .map(|h| scan.skills[h.skill].name.clone())
            .collect()
    }

    #[test]
    fn search_puts_name_matches_first_and_the_closest_name_on_top() {
        let claude = SkillOwner::Agent(AgentId::ClaudeCode);
        let scan = scan_of(vec![
            vec![install(claude, "writer", "Use to Review drafts", 1)],
            vec![install(claude, "plan-eng-review", "Engineering plan", 1)],
            vec![install(claude, "review", "Pre-landing review", 1)],
            vec![install(claude, "code-review:code-review", "", 1)],
            vec![install(claude, "deploy", "Ship it", 1)],
        ]);
        assert_eq!(
            names(&scan, "REVIEW"),
            [
                "review",
                "plan-eng-review",
                "code-review:code-review",
                "writer"
            ],
            "词全在名字里的按名字长短,只在说明里的垫底;大小写不论"
        );
        // 每个词都得出现在名字或说明里
        assert_eq!(names(&scan, "review drafts"), ["writer"]);
        assert_eq!(names(&scan, "review plan"), ["plan-eng-review"]);
        assert!(scan.search("  ").is_empty());
        assert!(scan.search("kubernetes").is_empty());
    }

    #[test]
    fn search_snippets_come_from_the_install_that_matched() {
        let codex = SkillOwner::Agent(AgentId::Codex);
        let scan = scan_of(vec![vec![
            install(
                SkillOwner::Shared,
                "deploy",
                "Ship to the staging cluster",
                1,
            ),
            install(codex, "deploy", "Ship it", 2),
        ]]);
        let skill = &scan.skills[0];
        // 最近改过的那份(Codex 的)说明里没有这个词:片段取自有命中的那份
        let hit = &scan.search("staging")[0];
        assert_eq!(skill.installs[hit.install].owner, SkillOwner::Shared);
        assert!(hit.snippet.contains(&format!("{HL_OPEN}staging{HL_CLOSE}")));
        // 只在名字里命中:片段是最近那份的说明,不标
        let hit = &scan.search("deploy")[0];
        assert_eq!(hit.install, skill.newest);
        assert_eq!(hit.snippet, "Ship it");
        // 两处都提到 staging,只有旧的那份还写着 cluster:两个词一起搜,落在旧的那份上
        let scan = scan_of(vec![vec![
            install(SkillOwner::Shared, "deploy", "Ship to staging cluster", 1),
            install(codex, "deploy", "Deploy to staging", 2),
        ]]);
        let hit = &scan.search("staging cluster")[0];
        assert_eq!(
            scan.skills[0].installs[hit.install].owner,
            SkillOwner::Shared
        );
        assert!(hit.snippet.contains(&format!("{HL_OPEN}cluster{HL_CLOSE}")));
    }

    #[test]
    fn a_star_segment_expands_to_every_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for ws in ["alpha", "beta", ".hidden"] {
            std::fs::create_dir_all(home.join("workspaces").join(ws).join("skills")).unwrap();
        }
        let mut plan = Plan::default();
        plan.add_under_home(SkillOwner::Shared, home, "workspaces/*/skills");
        let dirs: Vec<PathBuf> = plan.locations.iter().map(|l| l.dir.clone()).collect();
        assert_eq!(
            dirs,
            vec![
                home.join("workspaces/alpha/skills"),
                home.join("workspaces/beta/skills"),
            ]
        );
    }
}
