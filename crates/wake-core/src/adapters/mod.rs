pub mod antigravity;
pub mod claude;
pub mod codebuddy;
pub mod codex;
pub mod copilot;
pub mod craft;
pub mod cursor;
pub mod cursor_ide;
pub mod devin;
pub mod dsh;
pub mod gemini;
pub mod grok;
pub mod hermes;
pub mod kimi;
pub mod kiro;
pub mod openclaw;
pub mod opencode;
pub mod pi;
pub mod qoder;
pub mod zcode;

pub mod remote;

pub(crate) mod grok_group;
pub(crate) mod parse_utils;
pub(crate) mod pi_format;
pub(crate) mod sqlite_ro;

/// The local source file for a session, including mirrored remote sessions.
/// Database-backed sessions resolve to their shared database, without `#<id>`.
pub use sqlite_ro::strip_virtual_path as session_source_path;

use crate::models::*;
use anyhow::{Context as _, Result};
use std::path::Path;

/// agent 数据源适配器。列表扫描与详情解析共用同一核心解析器,
/// 保证 FTS 的 seq 与详情页消息序号一致(搜索跳转依赖)。
pub trait AgentAdapter: Send + Sync {
    fn agent(&self) -> AgentId;
    /// 实例服务的远程 host;空 = 本地。只有 remote::RemoteAdapter 覆写。
    /// scanner 的同家同 ID 去重按 (host, agent, native_id) 分域——两台机器
    /// 各自续跑过的同 UUID 会话是两条会话,不能按 mtime 互吞
    fn host(&self) -> &str {
        ""
    }
    /// 本机是否有这家的数据。由 data_roots 派生,**不要覆写**——它必须与
    /// 面板逐路径的 exists() 同一判据,手写版本(is_dir/is_file)与之打架
    /// 正是 2026-08-24 数轮 review 反复修的源头之一
    fn detect(&self) -> bool {
        self.data_roots().iter().any(|p| p.exists())
    }
    /// 枚举全部会话文件。契约是"枚举必须廉价、绝不做全量解析":多数家纯 stat,
    /// SQLite 型跑元数据查询,dsh 读有界首行(子代理标志只存在于文件头)。
    /// 故障就地降级为空列表,不外溢炸掉整轮扫描。
    fn list_session_files(&self) -> Result<Vec<SessionFileRef>>;
    /// watcher 事件路径 → 本 adapter 的会话文件引用;None = 非会话文件
    /// (边车、子代理转录等)。默认:非空 .jsonl,stem 即 native_id。
    /// 各家的路径布局知识收敛在此,watcher 不再硬编码任何 agent 特例。
    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        parse_utils::default_file_ref(self.agent(), path)
    }
    /// 快路径:不解析文件直接给出 meta(Codex 走 state DB)。None = 无快路径
    fn quick_meta(
        &self,
        _refs: &[SessionFileRef],
    ) -> Option<std::collections::HashMap<String, SessionMeta>> {
        None
    }
    /// Refresh project metadata from sidecars even when the transcript did not
    /// change. Keys are source file paths; only existing, unchanged winning
    /// copies are updated, without replacing their body or source timestamps.
    fn project_path_updates(
        &self,
        _refs: &[SessionFileRef],
    ) -> std::collections::HashMap<String, String> {
        Default::default()
    }
    /// quick 与 parsed 的合并策略:默认 parsed 为准、quick 补缺。
    /// Codex 覆写(state DB 的 title 是用户手动命名,优先级更高)。
    fn merge_quick_meta(&self, mut parsed: SessionMeta, quick: &SessionMeta) -> SessionMeta {
        if parsed.source.is_none() {
            parsed.source = quick.source.clone();
        }
        if parsed.model.is_none() {
            parsed.model = quick.model.clone();
        }
        if parsed.tokens_used.is_none() {
            parsed.tokens_used = quick.tokens_used;
        }
        parsed
    }
    /// 全解析:meta + FTS 单元
    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession>;
    /// 详情解析
    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript>;
    /// 加载 sidechain 消息(仅 Claude/Cursor subagents)
    fn load_sidechain(
        &self,
        _r: &SessionFileRef,
        _sidechain_id: &str,
    ) -> Result<Vec<TranscriptMessage>> {
        Ok(Vec::new())
    }
    /// 会话在磁盘上的全部归属路径(删除时一并 trash)。默认仅主文件;
    /// 有边车/目录布局的 adapter 覆写。
    fn session_paths(&self, meta: &SessionMeta) -> Vec<String> {
        vec![meta.file_path.clone()]
    }
    /// Explicit opt-in for independently owned local files. Database and remote
    /// adapters must leave this unsupported; cleanup also verifies containment.
    fn cleanup_paths(&self, _meta: &SessionMeta) -> Option<Vec<String>> {
        None
    }
    /// 读这些来源的记忆文档——Wake 只读:列出、搜索、阅读。`sources` 已按用户配置
    /// 裁决过(停用的不在、自定义的与项目模式在),`projects` 是已索引的本地项目根,
    /// 给项目模式展开(远程实例传空)。正文整份带上(都是小文件),但每轮扫描都会调,
    /// 实现方要**先 stat 后读**:指纹没变就交 `MtimeCache`(`cached_memory_docs`),
    /// 缺根返回 Ok(空)、读失败报 Err(scanner 只冻结这些来源的行);scanner 在每轮
    /// 扫描收尾按 (agent, host) 整组替换入库,消失的文件随之出库。项目归属不在这里
    /// 算:填 `session_key` 指向所属会话,读库时按它连 sessions 表解析(见 MemoryDoc)。
    /// 默认实现读通用形态(目录 / 文件 / 项目模式)、不缓存——没有自家记忆的 agent
    /// 只有被用户加了自定义来源或有项目指令文件时才走到。远程装饰器必须转发并改写
    /// key / host / session_key
    fn list_memories(
        &self,
        sources: &[MemorySource],
        projects: &[std::path::PathBuf],
    ) -> Result<Vec<MemoryDoc>> {
        cached_memory_docs(
            &MemoryCache::new(),
            self.agent(),
            &generic_units(sources, projects),
        )
    }
    /// 本实例默认读记忆的来源(Settings → Memory locations 的行):按实例的根派生,
    /// 不看存不存在。项目模式(`<project>/CLAUDE.md` 一类)不在这里报——那是 agent 级
    /// 的,由 `project_instruction_sources` 给、scanner 只挂到该家第一个本地实例上。
    /// 停用 / 自定义由 scanner 按 store 的配置裁决后经 `list_memories` 交回。默认没有
    fn memory_sources(&self) -> Vec<MemorySource> {
        Vec::new()
    }
    /// 一轮扫描开始前刷新 adapter 的跨会话快照。默认 adapter 没有这类状态。
    fn begin_scan(&self) {}
    /// 此 adapter 是否负责维护会话父子关系。单独的能力位用于区分“当前没有
    /// 子会话”和“不支持父子关系”，前者必须清掉数据库中的陈旧关系。
    fn manages_parent_links(&self) -> bool {
        false
    }
    /// 当前数据根内的 `(child_key, direct_parent_key)` 全量快照。scanner 会在
    /// 合并同 agent 的所有 location 后统一扁平到 root。**`None` 是"这一刻读不出来"**
    /// (state DB 打不开、读到一半出错),scanner 对这一家整段跳过、库里的关系原样
    /// 保留;`Some(空)` 才是"确定没有关系"——把读失败折成空会让 sync_parent_links
    /// 当成关系全解除,一句整家清空(2026-09-21 review)
    fn parent_links(&self) -> Option<Vec<(String, String)>> {
        Some(Vec::new())
    }
    /// watcher 事件是否会改变本家的跨会话快照(`parent_links` / `claimed_sessions`)。
    /// 这类文件未必是 `file_ref` 收的会话主文件(Grok 的关系边车、Craft 隐藏会话的
    /// 首行与回合锚点),命中后 watcher 在写库前刷新认领、写库后刷新父子关系——
    /// 不论事件种类(删除也算)
    fn is_snapshot_event(&self, _path: &Path) -> bool {
        false
    }
    /// 本家会话文件所在的根位置(目录,或 SQLite 型的库文件),**不论当前存不存在**。
    /// 这是路径的**唯一事实源**:watch_paths 由它派生,"Scanned locations" 面板
    /// 直接展示它,按路径前缀统计会话数也依赖它——故语义定死为"其子树(或其本身)
    /// 拥有本家 session 文件的位置",凭据/配置/索引这类不产生会话的文件不列。
    /// 新增 adapter 必须实现:没有默认值,漏了编译就过不去
    fn data_roots(&self) -> Vec<std::path::PathBuf>;
    /// 文件监听根目录。默认 = data_roots 中现存的目录,二十一家实测全部吻合:
    /// 目录型给出自己的 root,SQLite 型的根是库文件、天然筛空(watcher 只认
    /// .jsonl,库变更靠启动/手动刷新),codex 的 sessions + archived 一并覆盖。
    /// 只有当监听范围确实不同于数据根时才覆写——否则一次根路径搬迁
    /// (如 CODEX_HOME / XDG_DATA_HOME)就要在两处各改一遍,漏一处则静默失去实时更新
    fn watch_paths(&self) -> Vec<std::path::PathBuf> {
        self.data_roots()
            .into_iter()
            .filter(|p| p.is_dir())
            .collect()
    }
    /// 以自定义数据根构造本家的第二实例("Session locations" 的 Add location)。
    /// `dir` 是用户在系统目录选择器里选中的目录;各家把它整形成自己默认根的形态
    /// (允许选"家目录"或数据目录任一层,SQLite 型在其中找库文件),整形判据只看
    /// `dir` 内的现有结构,与默认实例的 env 探测同一快照语义。
    /// 唯一消费方是 create_adapters_with(常驻 roster 的自定义实例);
    /// 侧档(gemini projects.json / kimi session_index / codex state DB)必须
    /// 全部相对 `dir` 派生,落回默认家目录就会拿错树。新增 adapter 必须实现。
    fn with_custom_root(&self, dir: std::path::PathBuf) -> Box<dyn AgentAdapter>;
    /// 是否允许 Session locations 单独压制某条默认数据根。多数 adapter 的多个
    /// 根共同组成一个不可拆 location；OpenCode 的 stable/next 库则彼此独立。
    fn supports_individual_root_removal(&self) -> bool {
        false
    }
    /// 返回排除指定数据根后的同类 adapter。默认 location 的 Remove 只在上一
    /// 能力为 true 时调用；逐行启停也用它过滤多根 adapter，但不改变 Remove
    /// 的产品语义。None 表示不支持局部裁剪。
    fn excluding_data_roots(&self, _roots: &[std::path::PathBuf]) -> Option<Box<dyn AgentAdapter>> {
        None
    }
    /// 同家同 native_id 多副本裁决时本实例的位次:scanner 先按它升序、同级再按
    /// mtime 新者(不变量 8⑦)。默认 0。一家有多个数据源且要固定偏好某一源时
    /// 覆写——Cursor 的 IDE 库实例返回 1,转录带正文时永远是 CLI 那份胜出;
    /// 败方仍留作解析失败的回退顺位。远程装饰器必须转发
    fn dedup_rank(&self) -> u8 {
        0
    }
    /// `parent_links` 报的边是不是一张**全局**总表(Codex 的 state DB `thread_spawn_edges`
    /// 在 home 里,parent 的胜出文件可能归同家另一个 location)。默认 false = 边车长在
    /// parent 自己的 location 里(Grok),scanner 只认"报边者拥有 parent"的边;true 的才
    /// 走"parent 在库里就接受"的兜底——对 location 级边车放开兜底,备份目录里过期的边车
    /// 会把用户已解除的关系重新挂上(2026-09-22 review)。远程装饰器必须转发
    fn parent_links_global(&self) -> bool {
        false
    }
    /// `parent_links` 的边写在**子会话**自己的文件里(Craft 的首行 parentSessionId / 分支来源),
    /// 不是长在 parent 那个 location 里的边车:多 location 下 scanner 改认"报边者拥有 child 的
    /// 胜出文件"——落选的旧副本里过期的关系不采纳,parent 归别的 location 时边也不丢。
    /// 远程装饰器必须转发
    fn parent_links_in_child(&self) -> bool {
        false
    }
    /// 本家会话在**别家**目录里的替身。外壳产品(Craft Agents)跑的是别家的引擎,
    /// 引擎自己也往自家目录落了一份转录(Claude Agent SDK → `~/.claude/projects`),
    /// 同一段对话就会以两家身份各列一次。返回那些替身的 `(别家, 别家 native id)`:
    /// scanner 按本实例的 host 拼成会话 key(`session_key` 单点,远程装饰器因此照原样
    /// 转发),不索引它们、已入库的删掉;本家会话消失后认领随之撤销,替身在下一轮扫描
    /// 回来——Wake 只在原件也在索引里时才藏副本。要现读当前状态(或按文件戳缓存):
    /// scanner 不保证之前调过 begin_scan。**`None` 是"这一刻读不出来"**,scanner 保留库里
    /// 这一家的认领(与 parent_links 同一纪律)
    fn claimed_sessions(&self) -> Option<Vec<(AgentId, String)>> {
        Some(Vec::new())
    }
    /// 是否认领别家会话(见 claimed_sessions)。能力位与 manages_parent_links 同理:
    /// 只有它为 true 的家参与认领对账,其余家不会每轮往表里写一遍空集
    fn manages_claims(&self) -> bool {
        false
    }
}

/// 入库前的自定义根归一化:把用户选中的目录整形成本家"该存哪一层"的形态。
/// **按 agent 静态分派、不依赖 roster**——该家默认实例被用户移除且无自定义
/// 实例时,归一化仍必须生效(2026-08-24 Codex review);各家实现放各家文件,
/// 这里只做路由。归一化在构造之外做,是为维持"with_custom_root 派生根都在
/// 落库目录之下"的契约(构造器越界摸父目录会破坏契约测试与面板行标记)
pub fn normalize_custom_root(agent: AgentId, dir: std::path::PathBuf) -> std::path::PathBuf {
    match agent {
        AgentId::Codex => codex::normalize_custom_root(dir),
        AgentId::Zcode => zcode::normalize_custom_root(dir),
        AgentId::CraftAgents => craft::normalize_custom_root(dir),
        AgentId::Devin => devin::normalize_custom_root(dir),
        _ => dir,
    }
}

/// 环境变量指定的数据根。**只能读进程环境**:从 Dock 启动的 GUI 不继承用户
/// shell 的 env,kooky 处理 CODEX_HOME 时同样只能做到这一步("the best
/// available")。所以调用方拿它当**候选**而非唯一真相,默认路径仍要探。
/// 空值视作未设。
/// **调用方必须把返回值当候选**:探到真实数据(目录型看会话子目录、SQLite 型
/// 看库文件)才采信,否则回落默认位置——变量指向一个存在但空的目录时,不该让
/// 整家会话凭空消失
pub(crate) fn env_dir(key: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// 各家数据根共用的 HOME。**全部 adapter 必须走这里**,不要直接
/// `dirs::home_dir()`——`WAKE_HOME` 是整组 adapter 的统一改道开关:
/// 契约测试靠它把二十一家指向 fixture 目录,而 `dirs::home_dir()` 只在
/// POSIX 上看 `$HOME`,Windows 上走 SHGetKnownFolderPath、无论如何都指向
/// 真实用户目录(于是 Windows 上的契约测试全部落空,2026-08-25 review)。
/// 对用户它顺带是便携安装/多档案切换的手动开关。
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    env_dir("WAKE_HOME").or_else(dirs::home_dir)
}

/// 全量二十一家 roster,**不按 detect 过滤**。这是全应用唯一的构造点:
/// scanner/watcher/resume/Session locations 面板共享 Workbench 启动时的
/// 同一份实例。缺根的家由各自 list_session_files 降级为 Ok(空)(scanner
/// 对 Err 会 `?` 截断整轮,新 adapter 必须维持这条降级约定,contract 测试
/// 有卡)。**不要为任何用途二次构造 roster**:根路径是构造时刻对 env
/// (CODEX_HOME/XDG_DATA_HOME)与文件系统的快照,两份实例可能解析出不同的
/// 根,UI 就会展示一个扫描器并不在读的路径。
///
/// **家数 ≠ 实例数**:Cursor 一家有两个数据源(CLI 的 agent-transcripts
/// 与 IDE 的 state.vscdb),各占一个实例、共用 `AgentId::Cursor`。两源对
/// 同一 composer 都有记录时(IDE 会话在 JSONL 侧是只含 turn_ended 的空壳),
/// 由 scanner 的同家同 ID 去重按 mtime/size 裁决
pub fn create_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(codex::CodexAdapter::new()),
        Box::new(qoder::QoderAdapter::new()),
        Box::new(copilot::CopilotAdapter::new()),
        Box::new(cursor::CursorAdapter::new()),
        Box::new(cursor_ide::CursorIdeAdapter::new()),
        Box::new(opencode::OpencodeAdapter::new()),
        Box::new(kiro::KiroAdapter::new()),
        Box::new(gemini::GeminiAdapter::new()),
        Box::new(pi::PiAdapter::new()),
        Box::new(pi::PiAdapter::omp()),
        Box::new(grok::GrokAdapter::new()),
        Box::new(kimi::KimiAdapter::new()),
        Box::new(antigravity::AntigravityAdapter::new()),
        Box::new(dsh::DshAdapter::new()),
        Box::new(hermes::HermesAdapter::new()),
        Box::new(openclaw::OpenclawAdapter::new()),
        Box::new(codebuddy::CodebuddyAdapter::new()),
        Box::new(codebuddy::CodebuddyAdapter::workbuddy()),
        Box::new(zcode::ZcodeAdapter::new()),
        Box::new(craft::CraftAdapter::new()),
        Box::new(devin::DevinAdapter::new()),
    ]
}

/// 用户 location 配置下的完整 roster:默认实例在前(被用户移除的家除外),
/// 自定义实例按存储顺序追加在后。顺序是契约的一部分——"按 agent 找第一个"
/// 的兜底路径(watcher file_ref、adapter_ix_for 的 fallback)落在该家现存的
/// 首个实例上。自定义实例始终从默认模板构造(即便该家默认被移除,模板仍是
/// with_custom_root 的 ctor 来源)。构造点与 create_adapters 同属唯一化范围:
/// 换代必须整体换(见不变量 8)
pub fn create_adapters_with(
    custom_roots: &[(AgentId, std::path::PathBuf)],
    removed_defaults: &[AgentId],
) -> Vec<Box<dyn AgentAdapter>> {
    create_adapters_with_root_overrides(create_adapters(), custom_roots, removed_defaults, &[])
}

/// `base` 由调用方传入而非内部再造:create_adapter_roster_for 要把同一批
/// 模板先借给远程实例构造——整个 roster 组装保持**单次** create_adapters(),
/// 不变量 8 的"唯一构造时刻"不开例外
fn create_adapters_with_root_overrides(
    base: Vec<Box<dyn AgentAdapter>>,
    custom_roots: &[(AgentId, std::path::PathBuf)],
    removed_defaults: &[AgentId],
    removed_default_roots: &[(AgentId, std::path::PathBuf)],
) -> Vec<Box<dyn AgentAdapter>> {
    let customs: Vec<Box<dyn AgentAdapter>> = custom_roots
        .iter()
        .filter_map(|(agent, root)| {
            base.iter()
                .find(|a| a.agent() == *agent)
                .map(|a| a.with_custom_root(root.clone()))
        })
        .collect();
    let mut v: Vec<Box<dyn AgentAdapter>> = Vec::new();
    for adapter in base {
        if removed_defaults.contains(&adapter.agent()) {
            continue;
        }
        let excluded: Vec<std::path::PathBuf> = removed_default_roots
            .iter()
            .filter(|(agent, _)| *agent == adapter.agent())
            .map(|(_, path)| path.clone())
            .collect();
        if excluded.is_empty() {
            v.push(adapter);
        } else if let Some(filtered) = adapter.excluding_data_roots(&excluded) {
            if !filtered.data_roots().is_empty() {
                v.push(filtered);
            }
        } else {
            v.push(adapter);
        }
    }
    v.extend(customs);
    v
}

/// 管理界面中的一条真实数据根。`locations` 保留停用项；扫描与监听只消费
/// `AdapterRoster::active`，两者由同一批 adapter 实例派生，避免环境变量或
/// 文件系统探测在二次构造时产生不同快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterLocation {
    pub agent: AgentId,
    pub path: std::path::PathBuf,
    pub enabled: bool,
    /// 默认 location 的编辑表单是否允许只移除这一根（目前仅 OpenCode）。
    pub individually_removable: bool,
}

pub struct AdapterRoster {
    pub active: Vec<Box<dyn AgentAdapter>>,
    pub locations: Vec<AdapterLocation>,
}

/// 按索引库配置同时构造“全部 location 快照”和“仅启用的扫描 roster”。停用
/// 记录按 (agent, 真实数据根) 精确匹配，因此同一 Agent 的多个 location 可独立
/// 控制；单根 adapter 全关后直接移出 active，多根 adapter 走局部裁剪。
///
/// 启用的远程 host 以装饰器实例(adapters::remote)追加在 active **尾部**:
/// 顺序契约不变("按 agent 找第一个"的兜底仍落默认实例);远程实例不进
/// `locations`——Session locations 面板只管本地路径,远程在 Remote hosts 页。
pub fn create_adapter_roster_for(store: &crate::db::Store) -> AdapterRoster {
    let (customs, removed, removed_roots) = store.location_overrides();
    let base = create_adapters();
    // 远程实例先借同一批模板构造(未经 location 覆盖的原始形态),base 随后
    // move 进 overrides——全函数只有一次 create_adapters()
    let mut remote_instances: Vec<Box<dyn AgentAdapter>> = Vec::new();
    if let Some(db_dir) = store.db_dir() {
        for host in store.enabled_remote_host_names() {
            let host_cache = crate::remote::host_cache_dir(&db_dir, &host);
            remote_instances.extend(remote::create_remote_adapters(&base, &host, &host_cache));
        }
    }
    let configured = create_adapters_with_root_overrides(base, &customs, &removed, &removed_roots);
    let disabled: std::collections::HashSet<(AgentId, std::path::PathBuf)> =
        store.disabled_locations().into_iter().collect();

    let mut locations = Vec::new();
    let mut active = Vec::new();
    for adapter in configured {
        let agent = adapter.agent();
        let individually_removable = adapter.supports_individual_root_removal();
        let roots = adapter.data_roots();
        let excluded: Vec<std::path::PathBuf> = roots
            .iter()
            .filter(|root| disabled.contains(&(agent, (*root).clone())))
            .cloned()
            .collect();
        locations.extend(roots.iter().cloned().map(|path| AdapterLocation {
            agent,
            enabled: !disabled.contains(&(agent, path.clone())),
            path,
            individually_removable,
        }));

        if excluded.is_empty() {
            active.push(adapter);
        } else if excluded.len() < roots.len() {
            // 只有多根 adapter 会到这里。若某个新 adapter 尚未实现局部裁剪，
            // 保守地整实例停用，绝不能继续扫描用户明确关掉的路径。
            if let Some(filtered) = adapter.excluding_data_roots(&excluded) {
                if !filtered.data_roots().is_empty() {
                    active.push(filtered);
                }
            }
        }
    }

    // 远程实例无条件追加(缓存树可能还没同步下来——各家挂载点选的是"目录
    // 不存在也整形正确"的那层,缺根由 list_session_files 降级为空),同步
    // 落盘后 watcher/补扫自然收编
    active.extend(remote_instances);

    AdapterRoster { active, locations }
}

/// 按索引库里的 location 配置构造 roster——**所有打开真实索引库的入口**
/// (GUI 与 scan CLI)都必须走这里:用默认 roster 对配置过的库跑 run_scan,
/// 删除检测会把自定义根的会话当"磁盘已删"整批清掉,再把被压制的默认根加回
/// (2026-08-24 Codex review 抓到 scan bin 正是这么毁数据的)
pub fn create_adapters_for(store: &crate::db::Store) -> Vec<Box<dyn AgentAdapter>> {
    create_adapter_roster_for(store).active
}

/// 数据根是否拥有该会话文件路径。边界必须落在分隔符(目录型)或 '#'(SQLite
/// 虚拟路径 `<db>#<id>`)上,与 db 侧 counts_by_path_prefix 同判据——裸前缀
/// 会把 `…/sessions-old` 记到 `…/sessions` 头上。分隔符判定走
/// std::path::is_separator(Windows 上 `\` 与 `/` 都算),POSIX 上行为
/// 与旧 '/' 字面量逐位相同。
pub fn path_owns(root: &str, path: &str) -> bool {
    // 空根不拥有任何东西(data_roots() 里 to_string_lossy 出的空 PathBuf):
    // 走通用分支的话 strip_prefix("") 会原样返回整条路径,于是空根拥有
    // 一切绝对路径
    if root.is_empty() {
        return false;
    }
    // 文件系统根("/"、Windows 的 "C:\"):strip_prefix 剥掉的正是分隔符
    // 本身,通用分支会把一切后代判为界外(2026-08-24 Codex review)。
    // 判据必须是"自身以分隔符收尾"而**不是** parent().is_none():后者在
    // Windows 上对 UNC 共享根(`\\nas\agents`,components = [Prefix, RootDir])
    // 同样为 None,而它不以分隔符收尾——裸 starts_with 会把 `\\nas\agents-old`
    // 判为界内,正是本函数存在要防的那个 bug(2026-08-25 review)
    if root.ends_with(std::path::is_separator) {
        return path.starts_with(root);
    }
    match path.strip_prefix(root) {
        Some("") => true,
        Some(rest) => rest.starts_with(std::path::is_separator) || rest.starts_with('#'),
        None => false,
    }
}

/// 会话文件应由哪个实例服务:同 agent 中数据根最长前缀匹配者,匹配不到根时
/// 回退该 agent 的首个(默认)实例。自定义 location 让同 agent 出现多实例后,
/// "按 agent 找第一个"不再充分——gemini/kimi 的 cwd 反查、codex 的 state DB
/// 都是实例相对的侧档,拿默认实例解析自定义根下的文件会读错树
pub fn adapter_ix_for(
    adapters: &[Box<dyn AgentAdapter>],
    agent: AgentId,
    file_path: &str,
) -> Option<usize> {
    let first = adapters.iter().position(|a| a.agent() == agent)?;
    // 常态(该家只有默认一实例,零自定义 location)零分配直返;
    // data_roots() 每次调用都克隆整组 PathBuf,只该在真多实例时才付
    if !adapters[first + 1..].iter().any(|a| a.agent() == agent) {
        return Some(first);
    }
    let mut best: Option<(usize, usize)> = None; // (root 长度, 下标)
    for (ix, a) in adapters.iter().enumerate().skip(first) {
        if a.agent() != agent {
            continue;
        }
        for r in a.data_roots() {
            let rs = r.to_string_lossy();
            if path_owns(&rs, file_path) && best.is_none_or(|(len, _)| rs.len() > len) {
                best = Some((rs.len(), ix));
            }
        }
    }
    Some(best.map(|(_, ix)| ix).unwrap_or(first))
}

/// adapter_ix_for 的引用形态,详情/导出/删除等单会话路径用
pub fn adapter_for<'a>(
    adapters: &'a [Box<dyn AgentAdapter>],
    agent: AgentId,
    file_path: &str,
) -> Option<&'a dyn AgentAdapter> {
    adapter_ix_for(adapters, agent, file_path).map(|ix| adapters[ix].as_ref())
}

/// 从解析后的消息派生 FTS 单元(text + tool 名称/输入摘要)。agent 对 Wake 自己的
/// 查询(MCP 的 wake_* 工具、shell 里的 wake-cli / wake-mcp)不进索引:否则搜任何
/// 词,第一条命中都是"上次搜这个词的那次调用",越用越吵(自指回声)。只跳过工具
/// 那一段,消息正文照常——用户真写了 "wake-cli" 是内容,不是回声
fn units_from_messages(messages: &[TranscriptMessage]) -> Vec<IndexUnit> {
    messages
        .iter()
        .filter(|m| m.kind == MessageKind::Text)
        .filter_map(|m| {
            let mut parts = vec![m.text.clone()];
            for tc in &m.tool_calls {
                if wake_lookup_kind(tc).is_some() {
                    continue;
                }
                parts.push(format!("{} {}", tc.name, tc.input_preview));
            }
            let text = parse_utils::clip(&parts.join("\n"), MAX_MSG_TEXT).0;
            if text.trim().is_empty() {
                None
            } else {
                Some(IndexUnit {
                    seq: m.seq,
                    sidechain_id: None,
                    role: m.role,
                    timestamp: m.timestamp,
                    text,
                })
            }
        })
        .collect()
}

/// agent 在这些消息里查 Wake 的每一次调用:units_from_messages 把这些调用从索引里
/// 跳过,这里按同一判据逐次记下来(所在消息的 seq 与时间、走的接入、用的工具)落
/// wake_lookups 表——mcp-roadmap 定的"MCP 接入有没有真实使用"的信号,过滤掉又不记,
/// 那扇门就永远没有仪表(2026-09-16)
fn wake_lookups_from_messages(messages: &[TranscriptMessage]) -> Vec<WakeLookup> {
    messages
        .iter()
        .flat_map(|m| {
            m.tool_calls.iter().filter_map(move |tc| {
                let (channel, tool) = wake_lookup_kind(tc)?;
                Some(WakeLookup {
                    seq: m.seq,
                    timestamp: m.timestamp,
                    channel,
                    tool,
                })
            })
        })
        .collect()
}

impl ParsedSession {
    /// 各家 parse_session 的唯一出口:解析层只产 meta + messages,入库派生
    /// (FTS 单元、Wake 调用记录)全在这里——再加派生字段不用碰任何 adapter
    pub(crate) fn derive(
        meta: SessionMeta,
        messages: &[TranscriptMessage],
        unknown_line_count: u32,
    ) -> Self {
        Self {
            units: units_from_messages(messages),
            wake_lookups: wake_lookups_from_messages(messages),
            meta,
            unknown_line_count,
        }
    }
}

/// 记忆的一个读取单元:一个目录直属的 `*.<ext>`,或单个文件,带归属元数据。来源
/// (`MemorySource`)展开成单元——Claude 的记忆树按项目目录各一份,项目模式按每个
/// 已索引项目根各一份(`generic_units`)。`session_key` 是归属锚点(Claude 给该项目
/// 目录里最新的会话,项目路径读库时按它解析;用户级留空)
pub(crate) struct MemoryUnit {
    pub scope: MemoryScope,
    /// 所属来源的 id(memories.source 列;停用出库、读失败冻结、Settings 计数都按它)
    pub source: String,
    pub session_key: String,
    /// adapter 自己就知道的项目路径(Claude 目录里没会话时从目录名反推、项目模式的
    /// 项目根);空 = 交给锚点
    pub project_path: String,
    pub place: UnitPlace,
}

pub(crate) enum UnitPlace {
    Dir {
        dir: std::path::PathBuf,
        ext: &'static str,
    },
    File(std::path::PathBuf),
    /// 用户加的路径:stat 到是目录就读直属 `*.<ext>`,是文件就读它自己(`Custom` 来源)
    Path {
        path: std::path::PathBuf,
        ext: &'static str,
    },
}

impl UnitPlace {
    pub(crate) fn path(&self) -> &Path {
        match self {
            UnitPlace::Dir { dir, .. } => dir,
            UnitPlace::File(path) | UnitPlace::Path { path, .. } => path,
        }
    }

    /// 项目模式的相对路径落到某个项目根下
    fn under(&self, root: &Path) -> UnitPlace {
        match self {
            UnitPlace::Dir { dir, ext } => UnitPlace::Dir {
                dir: root.join(dir),
                ext,
            },
            UnitPlace::File(path) => UnitPlace::File(root.join(path)),
            UnitPlace::Path { path, ext } => UnitPlace::Path {
                path: root.join(path),
                ext,
            },
        }
    }
}

/// 一个来源按形态给出读取位置;记忆树与线程库是各家自己的形态,这里给 None、由 adapter
/// 自己展开。项目模式给的是相对项目根的位置,展开时再 `under` 到每个项目根
fn unit_place(source: &MemorySource) -> Option<UnitPlace> {
    let path = source.path.clone();
    Some(match source.kind {
        MemorySourceKind::Dir { ext } | MemorySourceKind::ProjectDir { ext } => {
            UnitPlace::Dir { dir: path, ext }
        }
        MemorySourceKind::File | MemorySourceKind::ProjectFile => UnitPlace::File(path),
        MemorySourceKind::Custom => UnitPlace::Path { path, ext: "md" },
        MemorySourceKind::ProjectTree | MemorySourceKind::ThreadDb => return None,
    })
}

/// 通用来源(目录 / 文件 / 自定义 / 项目模式)展开成读取单元
pub(crate) fn generic_units(
    sources: &[MemorySource],
    projects: &[std::path::PathBuf],
) -> Vec<MemoryUnit> {
    // 项目根偶尔就是某家的 home(用户在 ~/.codex 里跑过一次 codex):`<project>/AGENTS.md`
    // 会展开成全局那份 `~/.codex/AGENTS.md`——同一文件、同一 key,后写的项目级那份会把
    // 用户级那份顶成 ".codex" 项目的。具体来源(全局文件 / 目录)优先,项目模式展开到
    // 已被它们覆盖的路径就跳过(2026-09-21 用户在本机数据上发现)
    let concrete: std::collections::HashSet<&Path> = sources
        .iter()
        .filter(|s| !s.is_pattern())
        .map(|s| s.path.as_path())
        .collect();
    let mut units = Vec::new();
    for s in sources {
        let Some(place) = unit_place(s) else {
            continue;
        };
        let source = s.id();
        let unit = |project_path: String, place: UnitPlace| MemoryUnit {
            scope: s.scope(),
            source: source.clone(),
            session_key: String::new(),
            project_path,
            place,
        };
        if s.is_pattern() {
            for p in projects {
                let place = place.under(p);
                if concrete.contains(place.path()) {
                    continue;
                }
                units.push(unit(p.to_string_lossy().to_string(), place));
            }
        } else {
            units.push(unit(String::new(), place));
        }
    }
    units
}

/// 记忆树 `<root>/<项目目录>/memory/*.md` 展开成单元(Claude auto-memory 与 ZCode 同形):
/// 每个含 memory/ 的子目录一份,`attribute(子目录)` 给 (归属会话 key, 项目路径)。root
/// 不在 = 没有记忆;列不出来(权限、I/O、fd 耗尽)是"不知道",报 Err 让 scanner 冻结这一
/// 来源——折成空会把库里这家的记忆整组删光(2026-09-21 review)。按目录排序,结果稳定
pub(crate) fn project_tree_units(
    source: &MemorySource,
    attribute: impl Fn(&std::fs::DirEntry) -> (String, String),
) -> Result<Vec<MemoryUnit>> {
    let entries = match std::fs::read_dir(&source.path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::Error::new(e).context(source.path.display().to_string())),
    };
    let id = source.id();
    let mut units: Vec<MemoryUnit> = Vec::new();
    for entry in entries {
        // 逐项的读取错误也是"不知道"(吞掉会让那个项目的记忆行被当成消失而出库)
        let entry = entry.with_context(|| source.path.display().to_string())?;
        let dir = entry.path().join("memory");
        if !dir.is_dir() {
            continue;
        }
        let (session_key, project_path) = attribute(&entry);
        units.push(MemoryUnit {
            scope: MemoryScope::Project,
            source: id.clone(),
            session_key,
            project_path,
            place: UnitPlace::Dir { dir, ext: "md" },
        });
    }
    units.sort_by(|a, b| a.place.path().cmp(b.place.path()));
    Ok(units)
}

/// `list_memories` 的通用骨架:通用来源展开 + 各家自己的记忆树(有 ProjectTree 来源时
/// 交 `tree` 展开)+ 指纹缓存读取。持 `memories` 缓存的 adapter 都用它,别各写一遍
pub(crate) fn memory_docs_with_tree(
    cache: &MemoryCache,
    agent: AgentId,
    sources: &[MemorySource],
    projects: &[std::path::PathBuf],
    tree: impl FnOnce(&MemorySource) -> Result<Vec<MemoryUnit>>,
) -> Result<Vec<MemoryDoc>> {
    let mut units = generic_units(sources, projects);
    if let Some(t) = sources
        .iter()
        .find(|s| s.kind == MemorySourceKind::ProjectTree)
    {
        units.extend(tree(t)?);
    }
    cached_memory_docs(cache, agent, &units)
}

/// 没有自家记忆树的 adapter 的 `list_memories`:只有通用来源(全局文件、项目指令文件、
/// 自定义路径),经各自的 `memories` 缓存读
pub(crate) fn generic_memory_docs(
    cache: &MemoryCache,
    agent: AgentId,
    sources: &[MemorySource],
    projects: &[std::path::PathBuf],
) -> Result<Vec<MemoryDoc>> {
    cached_memory_docs(cache, agent, &generic_units(sources, projects))
}

/// 各家在项目根下读的指令文件(记忆可见层二期):按 agent 静态给,scanner 只挂到该家
/// 的第一个本地实例上(项目根是本机路径,自定义根 / 远程实例不展开)。文件名来自
/// 各家文档:Claude 的 CLAUDE.md / CLAUDE.local.md,Codex 的 AGENTS.md,Gemini 的
/// GEMINI.md,Cursor 的 .cursor/rules/*.mdc 与旧式 .cursorrules,Kiro 的
/// .kiro/steering/*.md,Copilot 的 .github/copilot-instructions.md
pub fn project_instruction_sources(agent: AgentId) -> Vec<MemorySource> {
    let file = |rel: &str| MemorySource {
        agent,
        kind: MemorySourceKind::ProjectFile,
        path: std::path::PathBuf::from(rel),
    };
    let dir = |rel: &str, ext: &'static str| MemorySource {
        agent,
        kind: MemorySourceKind::ProjectDir { ext },
        path: std::path::PathBuf::from(rel),
    };
    match agent {
        AgentId::ClaudeCode => vec![file("CLAUDE.md"), file("CLAUDE.local.md")],
        AgentId::Codex => vec![file("AGENTS.md")],
        AgentId::Gemini => vec![file("GEMINI.md")],
        AgentId::Cursor => vec![dir(".cursor/rules", "mdc"), file(".cursorrules")],
        AgentId::Kiro => vec![dir(".kiro/steering", "md")],
        AgentId::Copilot => vec![file(".github/copilot-instructions.md")],
        _ => Vec::new(),
    }
}

/// 一个实例这一轮的一个来源,按 Settings → Memory locations 的配置标好开关
pub struct PlannedSource {
    pub source: MemorySource,
    /// 没被用户停用
    pub enabled: bool,
    /// 用户添加的(Settings 里可编辑、可删);默认来源只能开关
    pub custom: bool,
}

/// 一个实例(或没有实例的一家)这一轮要读的来源
pub struct PlannedInstance {
    /// roster 下标;None = 该家没有本地实例(会话 location 全停用了),只剩 agent 级来源
    pub adapter: Option<usize>,
    pub agent: AgentId,
    /// 实例的 host(远程镜像);没有实例的一家恒为本地
    pub host: String,
    pub sources: Vec<PlannedSource>,
}

/// 各家这一轮的记忆来源:实例自己报的默认来源 + **该家第一个本地实例**再挂 agent 级的
/// 项目模式(`project_instruction_sources`)与用户的自定义来源(默认实例在 roster 前段;
/// 默认被移除时落到首个自定义实例),再按停用表标 `enabled`。会话 location 全停用、roster
/// 里没有实例的家,agent 级来源照旧归它(`adapter: None`):项目指令文件与自定义路径不在
/// 被停用的 location 里,不该随它消失,停掉的只是从那个根派生的默认来源——原先这种家直接
/// 从计划里消失,scanner 把它整组删光、Settings 里连自定义行都看不见了(2026-09-22 review)。
/// 这条规矩只在这里:scanner 读 enabled 的,Settings 面板全列(画开关),表单查重也看它——
/// 原先三处各写一遍,GUI 还自己拼来源 id、假定它等于路径(2026-09-22 /simplify)
pub fn memory_source_plan(
    adapters: &[Box<dyn AgentAdapter>],
    customs: &[(AgentId, std::path::PathBuf)],
    disabled: &std::collections::HashSet<(AgentId, String)>,
) -> Vec<PlannedInstance> {
    let agent_level = |agent: AgentId| -> Vec<(MemorySource, bool)> {
        project_instruction_sources(agent)
            .into_iter()
            .map(|s| (s, false))
            .chain(
                customs
                    .iter()
                    .filter(|(a, _)| *a == agent)
                    .map(|(_, path)| (MemorySource::custom(agent, path.clone()), true)),
            )
            .collect()
    };
    let planned = |agent: AgentId, sources: Vec<(MemorySource, bool)>| -> Vec<PlannedSource> {
        sources
            .into_iter()
            .map(|(source, custom)| PlannedSource {
                enabled: !disabled.contains(&(agent, source.id())),
                source,
                custom,
            })
            .collect()
    };
    let mut agent_level_taken: std::collections::HashSet<AgentId> =
        std::collections::HashSet::new();
    let mut plan: Vec<PlannedInstance> = adapters
        .iter()
        .enumerate()
        .map(|(ix, adapter)| {
            let agent = adapter.agent();
            let mut sources: Vec<(MemorySource, bool)> = adapter
                .memory_sources()
                .into_iter()
                .map(|s| (s, false))
                .collect();
            if adapter.host().is_empty() && agent_level_taken.insert(agent) {
                sources.extend(agent_level(agent));
            }
            PlannedInstance {
                adapter: Some(ix),
                agent,
                host: adapter.host().to_string(),
                sources: planned(agent, sources),
            }
        })
        .collect();
    for agent in AgentId::ALL.iter().copied() {
        if agent_level_taken.contains(&agent) {
            continue;
        }
        let sources = agent_level(agent);
        if sources.is_empty() {
            continue;
        }
        plan.push(PlannedInstance {
            adapter: None,
            agent,
            host: String::new(),
            sources: planned(agent, sources),
        });
    }
    plan
}

/// 记忆文档按来源缓存:key = 这次读的来源 id(多个就按序拼),值 = (指纹, docs)。scanner
/// 逐来源调 `list_memories`(一个来源读失败只冻结它自己),单槽的 `MtimeCache` 会在来源
/// 之间来回失效(2026-09-22 review)。失败不缓存
pub(crate) struct MemoryCache(
    std::sync::Mutex<std::collections::HashMap<String, (i64, Vec<MemoryDoc>)>>,
);

impl MemoryCache {
    pub(crate) fn new() -> Self {
        Self(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    fn get_or_build(
        &self,
        key: &str,
        stamp: i64,
        build: impl FnOnce() -> Result<Vec<MemoryDoc>>,
    ) -> Result<Vec<MemoryDoc>> {
        if let Some((s, docs)) = self.0.lock().unwrap().get(key) {
            if *s == stamp {
                return Ok(docs.clone());
            }
        }
        let docs = build()?;
        self.0
            .lock()
            .unwrap()
            .insert(key.to_string(), (stamp, docs.clone()));
        Ok(docs)
    }
}

/// (路径, mtime, size):读取单元下一份文件的指纹
type FileStamp = (std::path::PathBuf, i64, i64);

/// 一个读取单元下要读的文件:目录列直属 `*.<ext>`,文件就它自己,`Path` 按 stat 到的形状
/// 二选一。不存在 = 没有记忆(None);列不出来、stat 不到 = 不知道,报 Err——静默当成空
/// 会让库里那组被删(与读正文失败同一条规矩)。stat 跟符号链接(与会话枚举的
/// default_file_ref 同判据)
fn unit_files(place: &UnitPlace) -> Result<Option<Vec<FileStamp>>> {
    fn stat(path: &Path) -> Result<Option<std::fs::Metadata>> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(meta)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(path.display().to_string())),
        }
    }
    fn dir_files(dir: &Path, ext: &str) -> Result<Option<Vec<FileStamp>>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::Error::new(e).context(dir.display().to_string())),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| dir.display().to_string())?;
            let path = entry.path();
            if !path.extension().is_some_and(|x| x == ext) {
                continue;
            }
            // 挂空的符号链接、read_dir 与 stat 之间被 agent 删掉的文件 = 不在,跳过这一项;
            // 别的 stat 失败才是"不知道"(2026-09-22 review)
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(anyhow::Error::new(e).context(path.display().to_string())),
            };
            if meta.is_file() {
                files.push((path, parse_utils::mtime_ms(&meta), meta.len() as i64));
            }
        }
        Ok(Some(files))
    }
    let file = |path: &Path, meta: &std::fs::Metadata| {
        (
            path.to_path_buf(),
            parse_utils::mtime_ms(meta),
            meta.len() as i64,
        )
    };
    match place {
        UnitPlace::Dir { dir, ext } => dir_files(dir, ext),
        UnitPlace::File(path) => Ok(stat(path)?
            .filter(|meta| meta.is_file())
            .map(|meta| vec![file(path, &meta)])),
        UnitPlace::Path { path, ext } => match stat(path)? {
            None => Ok(None),
            Some(meta) if meta.is_dir() => dir_files(path, ext),
            Some(meta) => Ok(Some(vec![file(path, &meta)])),
        },
    }
}

/// 几个记忆目录的文档,经 `MemoryCache` 缓存:每轮只 stat(路径 + mtime + size +
/// 锚点拼成指纹),指纹没变就交上一轮读好的那份,变了才重读正文——`list_memories`
/// 每轮扫描都会调,159 份文件每轮都读是白费(2026-09-17 /simplify)。非 UTF-8 的
/// 文件不是记忆,跳过;别的读取失败(权限、I/O)整轮报 Err、不缓存——scanner 对
/// Err 是跳过该组不动库,下一轮再试;缓存成"少了这份"会让库里那行被删且指纹不变
/// 就再也不读(Codex review 2026-09-17)
pub(crate) fn cached_memory_docs(
    cache: &MemoryCache,
    agent: AgentId,
    units: &[MemoryUnit],
) -> Result<Vec<MemoryDoc>> {
    use std::hash::{Hash as _, Hasher as _};
    // 缓存槽按这次读的来源分:scanner 逐来源调,各来源的指纹互不干扰
    let mut key: Vec<&str> = units.iter().map(|u| u.source.as_str()).collect();
    key.sort_unstable();
    key.dedup();
    let key = key.join("\n");
    let mut listing: Vec<(&MemoryUnit, Vec<FileStamp>)> = Vec::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for unit in units {
        let Some(mut files) = unit_files(&unit.place)? else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        files.sort();
        unit.source.hash(&mut hasher);
        unit.session_key.hash(&mut hasher);
        unit.project_path.hash(&mut hasher);
        files.hash(&mut hasher);
        listing.push((unit, files));
    }
    let stamp = hasher.finish() as i64;
    cache.get_or_build(&key, stamp, || {
        let mut docs = Vec::new();
        for (unit, files) in &listing {
            for (path, updated_at, size_bytes) in files {
                if let Some(doc) =
                    memory_doc_from_file(agent, unit, path, *updated_at, *size_bytes)?
                {
                    docs.push(doc);
                }
            }
        }
        Ok(docs)
    })
}

/// 记忆标题(frontmatter 的 description)的字符上限:会话标题是 `MAX_TITLE` = 80,
/// description 是完整一句话,给宽一档
pub const MEMORY_TITLE_MAX: usize = 120;

/// 读一份 Markdown 记忆文件成 MemoryDoc:标题取 frontmatter 的 description,其次
/// name,再退文件名(带扩展名——"CLAUDE.md" / "MEMORY.md" 本身就是它的名字);正文
/// 原样保留(含 frontmatter,阅读面照渲染)。项目路径不在这里填(读库时按 session_key
/// 解析)。非 UTF-8 给 Ok(None)(不是记忆),其余读取失败原样报错
fn memory_doc_from_file(
    agent: AgentId,
    unit: &MemoryUnit,
    path: &Path,
    updated_at: i64,
    size_bytes: i64,
) -> Result<Option<MemoryDoc>> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(path.display().to_string())),
    };
    let fallback = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // description 是一句话,本机实测有 565 字符的:折行、按字符封顶,列表与 MCP 一行
    // 一条才放得下(2026-09-21 review)
    let title = frontmatter_field(&body, "description")
        .or_else(|| frontmatter_field(&body, "name"))
        .map(|t| crate::text::one_line(&t, MEMORY_TITLE_MAX))
        .unwrap_or(fallback);
    let path_str = path.to_string_lossy().to_string();
    Ok(Some(MemoryDoc {
        key: session_key(agent, "", &path_str),
        agent,
        host: String::new(),
        scope: unit.scope,
        project_path: unit.project_path.clone(),
        project_name: if unit.project_path.is_empty() {
            String::new()
        } else {
            parse_utils::project_name_of(&unit.project_path)
        },
        session_key: unit.session_key.clone(),
        path: path_str,
        title,
        updated_at,
        size_bytes,
        source: unit.source.clone(),
        body,
    }))
}

/// 阅读一份记忆的正文:文件型现场读磁盘(agent 刚改过也能看到),读不到退库里那份;
/// SQLite 型(虚拟路径)只有库里那份。MCP 的 wake_get_session 与 GUI 阅读面共用
pub fn memory_body(doc: &MemoryDoc) -> String {
    if session_source_path(&doc.path) != doc.path {
        return doc.body.clone();
    }
    std::fs::read_to_string(&doc.path).unwrap_or_else(|_| doc.body.clone())
}

/// YAML frontmatter(文件开头 `---` 围起的几行)里一个顶层 `key: 值`。不做完整
/// YAML:只认顶格的 `key:`,值去首尾引号;块标量(`key: >` / `|`,值在下面缩进的几行)
/// 把那几行折成一行——否则标题就是一个字面的 ">"。没有 frontmatter 或没这个键给 None
pub(crate) fn frontmatter_field(body: &str, key: &str) -> Option<String> {
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut lines = lines.peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "---" {
            break;
        }
        let Some(value) = line
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let value = value.trim();
        let value = if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let mut folded = Vec::new();
            while let Some(next) = lines.peek() {
                if !next.starts_with([' ', '\t']) || next.trim() == "---" {
                    break;
                }
                folded.push(next.trim());
                lines.next();
            }
            folded.join(" ")
        } else {
            value.trim_matches('"').trim_matches('\'').to_string()
        };
        return (!value.is_empty()).then_some(value);
    }
    None
}

/// 手输路径的 `~` 前缀展开(仅前缀;边界落在分隔符上,Windows 用户手输
/// `~\foo` 同样认)。HOME 与 adapter 同源(`home_dir` 的 WAKE_HOME 开关);
/// UI 的 `format::expand_tilde` 与 wake-mcp 的 project 参数都走这里
pub fn expand_tilde(p: &str) -> String {
    match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(std::path::is_separator) => {
            match home_dir() {
                Some(h) => format!("{}{rest}", h.to_string_lossy()),
                None => p.to_string(),
            }
        }
        _ => p.to_string(),
    }
}

/// Wake 自己的两个命令行二进制;在 shell 工具的 command 里以整词出现即视为在查 Wake
const WAKE_BINARIES: [&str; 2] = ["wake-cli", "wake-mcp"];

/// 这次工具调用是不是 agent 在查 Wake,是的话走的哪条接入、用的哪个工具(MCP 给契约
/// 工具名,命令行给二进制名;None = 不是)。FTS 过滤只看 is_some,wake_lookups 表记账
/// 要细分。MCP 工具按 `mcp::tools::NAMES` 认(加第五家自动覆盖),客户端会给名字加
/// 自己的前缀(Claude Code / Codex 是 `mcp__wake__wake_search`,别家形态不一),所以
/// 只看结尾、并要求前一个字符不是字母数字(`awake_search` 不算);命令行则要求
/// wake-cli / wake-mcp 以整词出现在 shell 工具的 **command 字段**里——Grep 的 pattern、
/// Read 的路径提到 wake-cli 是在读代码,不是查询(Codex review 2026-09-14)
fn wake_lookup_kind(tc: &ToolCallView) -> Option<(LookupChannel, &'static str)> {
    if let Some(tool) = crate::mcp::tools::NAMES
        .iter()
        .find(|t| ends_with_word(&tc.name, t))
    {
        return Some((LookupChannel::Mcp, *tool));
    }
    let bin = WAKE_BINARIES
        .iter()
        .find(|bin| names_binary(&tc.input_preview, bin))?;
    command_names_binary(tc).then_some((LookupChannel::Cli, *bin))
}

/// 预览(对 shell 工具就是命令的前 200 字)提到了二进制名之后再看结构:只有输入对象的
/// `command`(字符串或 argv 数组,各家 shell 工具都这么叫)里出现才算执行了 Wake。
/// 输入没有结构可看(None,或被截断到解析不了)时退回预览判断
fn command_names_binary(tc: &ToolCallView) -> bool {
    let Some(raw) = tc.input.as_deref() else {
        return true;
    };
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return true;
    };
    let text = match obj.get("command") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return false,
    };
    WAKE_BINARIES.iter().any(|bin| names_binary(&text, bin))
}

fn ends_with_word(s: &str, word: &str) -> bool {
    s.strip_suffix(word)
        .is_some_and(|head| !head.chars().next_back().is_some_and(char::is_alphanumeric))
}

/// `wake-cli search …`、`/Applications/Wake.app/Contents/MacOS/wake-cli sessions`、
/// `cd x && wake-cli projects`、`WAKE=…/wake-cli`、`wake-cli.exe` 都算;
/// `wake-cli-old`、`wake-clip` 不算
fn names_binary(preview: &str, bin: &str) -> bool {
    let joins = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    preview.match_indices(bin).any(|(i, _)| {
        let before = preview[..i].chars().next_back();
        let after = preview[i + bin.len()..].chars().next();
        !before.is_some_and(joins) && !after.is_some_and(joins)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(name: &str, preview: &str, input: Option<&str>) -> ToolCallView {
        ToolCallView {
            id: String::new(),
            name: name.to_string(),
            input_preview: preview.to_string(),
            input: input.map(String::from),
            output: None,
            is_error: false,
            sidechain_ref: None,
        }
    }

    fn msg(seq: i64, text: &str, tools: &[(&str, &str)]) -> TranscriptMessage {
        TranscriptMessage {
            seq,
            role: Role::Assistant,
            kind: MessageKind::Text,
            text: text.to_string(),
            truncated: false,
            tool_calls: tools
                .iter()
                .map(|(name, preview)| tc(name, preview, None))
                .collect(),
            thinking: None,
            timestamp: None,
            model: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn wake_lookups_stay_out_of_the_index() {
        let messages = [
            msg(
                0,
                "看看历史",
                &[
                    ("mcp__wake__wake_search", "二维码"),
                    ("Bash", "wake-cli search \"二维码\" --limit 3"),
                    (
                        "Bash",
                        "/Applications/Wake.app/Contents/MacOS/wake-cli sessions --project .",
                    ),
                    ("Read", "src/db.rs"),
                ],
            ),
            // 只有 Wake 调用、没有正文:整条不进索引
            msg(
                1,
                "",
                &[
                    ("wake_get_session", "claude-code:abc"),
                    ("Bash", "wake-mcp call wake_search '{}'"),
                ],
            ),
            // 用户正文里提到 wake-cli 是内容,不是回声
            msg(2, "wake-cli 的 --since 该怎么写", &[]),
        ];
        let units = units_from_messages(&messages);
        assert_eq!(units.iter().map(|u| u.seq).collect::<Vec<_>>(), [0, 2]);
        assert!(units[0].text.contains("看看历史"));
        assert!(units[0].text.contains("Read src/db.rs"));
        assert!(!units[0].text.contains("wake_search"));
        assert!(!units[0].text.contains("wake-cli"));
        assert!(!units[0].text.contains("二维码"), "查询词本身也不该进索引");
        assert!(units[1].text.contains("wake-cli 的 --since"));
    }

    /// 过滤掉的每一次调用都要逐次记下来,带所在消息的 seq、接入渠道与工具名;
    /// 正文里提到 wake-cli 不算
    #[test]
    fn wake_lookups_are_recorded_per_call() {
        let messages = [
            msg(
                0,
                "看看历史",
                &[
                    ("mcp__wake__wake_search", "二维码"),
                    ("Bash", "wake-cli search \"二维码\" --limit 3"),
                    ("Read", "src/db.rs"),
                ],
            ),
            msg(1, "", &[("wake_get_session", "claude-code:abc")]),
            msg(2, "wake-cli 的 --since 该怎么写", &[]),
        ];
        let found: Vec<(i64, LookupChannel, &str)> = wake_lookups_from_messages(&messages)
            .iter()
            .map(|l| (l.seq, l.channel, l.tool))
            .collect();
        assert_eq!(
            found,
            [
                (0, LookupChannel::Mcp, "wake_search"),
                (0, LookupChannel::Cli, "wake-cli"),
                (1, LookupChannel::Mcp, "wake_get_session"),
            ]
        );
        assert!(wake_lookups_from_messages(&messages[2..]).is_empty());
    }

    #[test]
    fn wake_lookup_detection_is_narrow() {
        let lookup = |name: &str, preview: &str, input: Option<&str>| {
            wake_lookup_kind(&tc(name, preview, input)).is_some()
        };
        assert!(lookup("mcp__wake__wake_list_projects", "", None));
        assert!(lookup("wake_search", "x", None));
        assert!(
            lookup("mcp_wake_wake_get_session", "", None),
            "别家客户端的前缀形态"
        );
        // 没有结构化输入可看时按预览判
        assert!(lookup("Bash", "cd repo && wake-cli projects", None));
        assert!(lookup("Bash", "C:\\Wake\\wake-cli.exe sessions", None));
        assert!(lookup(
            "Bash",
            "WAKE=/Applications/Wake.app/Contents/MacOS/wake-cli",
            None
        ));
        assert!(!lookup("Bash", "cargo test -p wake-core", None));
        assert!(!lookup("Bash", "wake-cli-old --help && wake-clip", None));
        assert!(!lookup("mcp__other__awake_search", "", None));
        assert!(!lookup("Edit", "crates/wake/src/settings.rs", None));
        // 有结构化输入时只认 command 字段:读代码、搜代码提到 wake-cli 不是查询
        assert!(lookup(
            "Bash",
            "wake-cli search \"二维码\"",
            Some(r#"{"command": "wake-cli search \"二维码\"", "description": "search history"}"#)
        ));
        assert!(
            lookup(
                "shell",
                "bash -lc wake-cli sessions --limit 5",
                Some(r#"{"command": ["bash", "-lc", "wake-cli sessions --limit 5"]}"#)
            ),
            "Codex 的 argv 数组形态"
        );
        assert!(!lookup(
            "Grep",
            "wake-cli",
            Some(r#"{"pattern": "wake-cli", "path": "docs"}"#)
        ));
        assert!(!lookup(
            "Read",
            "docs/wake-cli notes.md",
            Some(r#"{"file_path": "docs/wake-cli notes.md"}"#)
        ));
        assert!(
            !lookup(
                "Bash",
                "man wake-cli-old",
                Some(r#"{"command": "man wake-cli-old", "description": "wake-cli docs"}"#)
            ),
            "command 里没有整词就不算,哪怕 description 提到"
        );
    }
}
