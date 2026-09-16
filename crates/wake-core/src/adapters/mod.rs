pub mod antigravity;
pub mod claude;
pub mod codebuddy;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod cursor_ide;
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

pub mod remote;

pub(crate) mod grok_group;
pub(crate) mod parse_utils;
pub(crate) mod pi_format;
pub(crate) mod sqlite_ro;

use crate::models::*;
use anyhow::Result;
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
    /// 一轮扫描开始前刷新 adapter 的跨会话快照。默认 adapter 没有这类状态。
    fn begin_scan(&self) {}
    /// 此 adapter 是否负责维护会话父子关系。单独的能力位用于区分“当前没有
    /// 子会话”和“不支持父子关系”，前者必须清掉数据库中的陈旧关系。
    fn manages_parent_links(&self) -> bool {
        false
    }
    /// 当前数据根内的 `(child_key, direct_parent_key)` 全量快照。scanner 会在
    /// 合并同 agent 的所有 location 后统一扁平到 root。
    fn parent_links(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// watcher 事件是否会改变父子关系快照。关系边车不是会话主文件，不能塞进
    /// `file_ref`；命中后 watcher 会单独刷新同 agent 的所有关系快照。
    fn is_parent_link_event(&self, _path: &Path) -> bool {
        false
    }
    /// 本家会话文件所在的根位置(目录,或 SQLite 型的库文件),**不论当前存不存在**。
    /// 这是路径的**唯一事实源**:watch_paths 由它派生,"Scanned locations" 面板
    /// 直接展示它,按路径前缀统计会话数也依赖它——故语义定死为"其子树(或其本身)
    /// 拥有本家 session 文件的位置",凭据/配置/索引这类不产生会话的文件不列。
    /// 新增 adapter 必须实现:没有默认值,漏了编译就过不去
    fn data_roots(&self) -> Vec<std::path::PathBuf>;
    /// 文件监听根目录。默认 = data_roots 中现存的目录,十八家实测全部吻合:
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
}

/// 入库前的自定义根归一化:把用户选中的目录整形成本家"该存哪一层"的形态。
/// **按 agent 静态分派、不依赖 roster**——该家默认实例被用户移除且无自定义
/// 实例时,归一化仍必须生效(2026-08-24 Codex review);各家实现放各家文件,
/// 这里只做路由。归一化在构造之外做,是为维持"with_custom_root 派生根都在
/// 落库目录之下"的契约(构造器越界摸父目录会破坏契约测试与面板行标记)
pub fn normalize_custom_root(agent: AgentId, dir: std::path::PathBuf) -> std::path::PathBuf {
    match agent {
        AgentId::Codex => codex::normalize_custom_root(dir),
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
/// 契约测试靠它把十八家指向 fixture 目录,而 `dirs::home_dir()` 只在
/// POSIX 上看 `$HOME`,Windows 上走 SHGetKnownFolderPath、无论如何都指向
/// 真实用户目录(于是 Windows 上的契约测试全部落空,2026-08-25 review)。
/// 对用户它顺带是便携安装/多档案切换的手动开关。
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    env_dir("WAKE_HOME").or_else(dirs::home_dir)
}

/// 全量十八家 roster,**不按 detect 过滤**。这是全应用唯一的构造点:
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
pub(crate) fn units_from_messages(messages: &[TranscriptMessage]) -> Vec<IndexUnit> {
    messages
        .iter()
        .filter(|m| m.kind == MessageKind::Text)
        .filter_map(|m| {
            let mut parts = vec![m.text.clone()];
            for tc in &m.tool_calls {
                if is_wake_lookup(tc) {
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

/// 这次工具调用是不是 agent 在查 Wake:MCP 工具按 `mcp::tools::NAMES` 认(加第五家
/// 自动覆盖),客户端会给名字加自己的前缀(Claude Code / Codex 是 `mcp__wake__wake_search`,
/// 别家形态不一),所以只看结尾、并要求前一个字符不是字母数字(`awake_search` 不算);
/// 命令行则要求 wake-cli / wake-mcp 以整词出现在 shell 工具的 **command 字段**里——
/// Grep 的 pattern、Read 的路径提到 wake-cli 是在读代码,不是查询(Codex review 2026-09-14)
fn is_wake_lookup(tc: &ToolCallView) -> bool {
    crate::mcp::tools::NAMES
        .iter()
        .any(|t| ends_with_word(&tc.name, t))
        || (WAKE_BINARIES
            .iter()
            .any(|bin| names_binary(&tc.input_preview, bin))
            && command_names_binary(tc))
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

    #[test]
    fn wake_lookup_detection_is_narrow() {
        let lookup = |name: &str, preview: &str, input: Option<&str>| {
            is_wake_lookup(&tc(name, preview, input))
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
