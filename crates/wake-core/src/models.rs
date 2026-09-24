use serde::{Deserialize, Serialize};

/// 变体声明序即侧栏固定展示序(derive Ord 直接按此序比较)——
/// 顺序为用户指定(2026-08-20),新增 agent 问过用户再定位置,勿擅动老条目
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentId {
    ClaudeCode,
    Codex,
    Grok,
    Dsh,
    Cursor,
    Opencode,
    Pi,
    Omp,
    Kiro,
    Kimi,
    Gemini,
    Copilot,
    Antigravity,
    Qoder,
    Hermes,
    Openclaw,
    Codebuddy,
    Workbuddy,
    Zcode,
    Devin,
}

impl AgentId {
    /// 全部二十家,**枚举声明序**(= Ord = 用户钉的侧栏展示序;面板成组、
    /// 表单下拉共用同一顺序)。曾误抄 create_adapters 的构造序,下拉与侧栏
    /// 排序当场对不上——契约测试现在卡它与 Ord 一致
    pub const ALL: [AgentId; 20] = [
        AgentId::ClaudeCode,
        AgentId::Codex,
        AgentId::Grok,
        AgentId::Dsh,
        AgentId::Cursor,
        AgentId::Opencode,
        AgentId::Pi,
        AgentId::Omp,
        AgentId::Kiro,
        AgentId::Kimi,
        AgentId::Gemini,
        AgentId::Copilot,
        AgentId::Antigravity,
        AgentId::Qoder,
        AgentId::Hermes,
        AgentId::Openclaw,
        AgentId::Codebuddy,
        AgentId::Workbuddy,
        AgentId::Zcode,
        AgentId::Devin,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentId::ClaudeCode => "claude-code",
            AgentId::Codex => "codex",
            AgentId::Qoder => "qoder",
            AgentId::Copilot => "copilot",
            AgentId::Cursor => "cursor",
            AgentId::Opencode => "opencode",
            AgentId::Kiro => "kiro",
            AgentId::Gemini => "gemini",
            AgentId::Pi => "pi",
            AgentId::Omp => "omp",
            AgentId::Grok => "grok",
            AgentId::Kimi => "kimi",
            AgentId::Antigravity => "antigravity",
            AgentId::Dsh => "dsh",
            AgentId::Hermes => "hermes",
            AgentId::Openclaw => "openclaw",
            AgentId::Codebuddy => "codebuddy",
            AgentId::Workbuddy => "workbuddy",
            AgentId::Zcode => "zcode",
            AgentId::Devin => "devin",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(AgentId::ClaudeCode),
            "codex" => Some(AgentId::Codex),
            "qoder" => Some(AgentId::Qoder),
            "copilot" => Some(AgentId::Copilot),
            "cursor" => Some(AgentId::Cursor),
            "opencode" => Some(AgentId::Opencode),
            "kiro" => Some(AgentId::Kiro),
            "gemini" => Some(AgentId::Gemini),
            "pi" => Some(AgentId::Pi),
            "omp" => Some(AgentId::Omp),
            "grok" => Some(AgentId::Grok),
            "kimi" => Some(AgentId::Kimi),
            "antigravity" => Some(AgentId::Antigravity),
            "dsh" => Some(AgentId::Dsh),
            "hermes" => Some(AgentId::Hermes),
            "openclaw" => Some(AgentId::Openclaw),
            "codebuddy" => Some(AgentId::Codebuddy),
            "workbuddy" => Some(AgentId::Workbuddy),
            "zcode" => Some(AgentId::Zcode),
            "devin" => Some(AgentId::Devin),
            _ => None,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AgentId::ClaudeCode => "Claude Code",
            AgentId::Codex => "Codex",
            AgentId::Qoder => "Qoder CLI",
            AgentId::Copilot => "Copilot CLI",
            AgentId::Cursor => "Cursor",
            AgentId::Opencode => "OpenCode",
            AgentId::Kiro => "Kiro",
            AgentId::Gemini => "Gemini CLI",
            AgentId::Pi => "Pi",
            AgentId::Omp => "Oh My Pi",
            AgentId::Grok => "Grok Build",
            AgentId::Kimi => "Kimi Code",
            AgentId::Antigravity => "Antigravity CLI",
            AgentId::Dsh => "DeepSeek Harness",
            AgentId::Hermes => "Hermes Agent",
            AgentId::Openclaw => "OpenClaw",
            AgentId::Codebuddy => "CodeBuddy",
            AgentId::Workbuddy => "WorkBuddy",
            AgentId::Zcode => "ZCode",
            AgentId::Devin => "Devin",
        }
    }

    /// agent 品牌图标——wake crate `Assets` 内嵌的 PNG 路径(lobe-icons 素材,
    /// 与 kooky 同源)。后缀必须带上(与 SVG 图标同理,漏后缀 = 静默空白)。
    /// Copilot/Cursor/OpenCode/Pi/Grok/Kimi 是单色字形(白色+alpha):深色模式
    /// 用白色版,浅色模式用 `-light`(深墨 #2B2A26)版——等效 kooky 的染色;
    /// Qoder 是白/黑字形配绿色品牌色,同样按模式切图;Hermes(Nous 的少女字形)
    /// 浅色用墨色透明底字形,**深色不能用白色实心字形**——细节密、16px 下糊成一团白
    /// (用户 2026-09-03 反馈),深色改用官方形态「白色圆角底 + 墨色画」的贴片;
    /// 其余彩色品牌(Claude/Codex/Gemini/Kiro/Omp/Antigravity/OpenClaw 的红龙虾)
    /// 保持原色,两模式通用。
    pub fn brand_icon(&self, dark: bool) -> &'static str {
        match self {
            AgentId::ClaudeCode => "brands/claude-code.png",
            AgentId::Codex => "brands/codex.png",
            AgentId::Qoder => {
                if dark {
                    "brands/qoder.png"
                } else {
                    "brands/qoder-light.png"
                }
            }
            AgentId::Copilot => {
                if dark {
                    "brands/copilot.png"
                } else {
                    "brands/copilot-light.png"
                }
            }
            AgentId::Cursor => {
                if dark {
                    "brands/cursor.png"
                } else {
                    "brands/cursor-light.png"
                }
            }
            AgentId::Opencode => {
                if dark {
                    "brands/opencode.png"
                } else {
                    "brands/opencode-light.png"
                }
            }
            AgentId::Kiro => "brands/kiro.png",
            AgentId::Gemini => "brands/gemini.png",
            AgentId::Pi => {
                if dark {
                    "brands/pi.png"
                } else {
                    "brands/pi-light.png"
                }
            }
            AgentId::Omp => "brands/omp.png",
            AgentId::Grok => {
                if dark {
                    "brands/grok.png"
                } else {
                    "brands/grok-light.png"
                }
            }
            AgentId::Kimi => {
                if dark {
                    "brands/kimi.png"
                } else {
                    "brands/kimi-light.png"
                }
            }
            AgentId::Antigravity => "brands/antigravity.png",
            AgentId::Dsh => "brands/deepseek.png",
            AgentId::Hermes => {
                if dark {
                    "brands/hermes.png"
                } else {
                    "brands/hermes-light.png"
                }
            }
            AgentId::Openclaw => "brands/openclaw.png",
            AgentId::Codebuddy => "brands/codebuddy.png",
            AgentId::Workbuddy => "brands/workbuddy.png",
            // 官方 app 图标(黑底白 Z),两模式通用
            AgentId::Zcode => "brands/zcode.png",
            // 官方 app 图标(蓝图网格白字),两模式通用
            AgentId::Devin => "brands/devin.png",
        }
    }
}

/// 会话元数据 —— 列表页/索引库统一模型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// 全局唯一键: 本地 `{agent}:{native_id}`,远程 `{agent}:{host}:{native_id}`。
    /// agent 恒为首段(scanner 的易主检测按 `split(':').next()` 取 agent);
    /// host 段只由 adapters::remote 的装饰器在解析出口插入,adapter 本体
    /// 永远生产本地格式
    pub key: String,
    /// 原生会话 id(resume 用;不含 host 段)
    pub id: String,
    /// 远程 host 名(Settings 里配置的 SSH 目标);空 = 本地会话。
    /// 只由 RemoteAdapter 装饰器填充,各家 adapter 构造时一律留空
    #[serde(default)]
    pub host: String,
    pub agent: AgentId,
    pub title: String,
    pub project_path: String,
    pub project_name: String,
    pub file_path: String,
    /// epoch ms
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
    pub size_bytes: i64,
    pub git_branch: Option<String>,
    pub model: Option<String>,
    pub tokens_used: Option<i64>,
    pub archived: bool,
    pub source: Option<String>,
    // app 自有状态(user_data 表)
    pub favorite: bool,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    pub input_preview: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub is_error: bool,
    pub sidechain_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageKind {
    Text,
    Meta,
    CompactSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    /// 人读标题(导出与 wake-mcp 的消息头共用;`as_str` 是落库/传输用的小写)
    pub fn title(&self) -> &'static str {
        match self {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

/// 会话正文里携带的一张图片。这里保存 PNG/JPEG 等原始编码字节，
/// 不是解码后的像素；adapter 只在详情解析路径填充，索引路径仅保留占位符。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub media_type: String,
    pub bytes: Vec<u8>,
    /// 图片在消息纯文本中的字节偏移。多个图片可共享同一偏移，表示连续图片块。
    #[serde(default)]
    pub text_offset: usize,
}

/// 详情页统一消息模型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptMessage {
    /// 会话内稳定序号(搜索定位锚点)
    pub seq: i64,
    pub role: Role,
    pub kind: MessageKind,
    pub text: String,
    pub truncated: bool,
    pub tool_calls: Vec<ToolCallView>,
    pub thinking: Option<String>,
    /// epoch ms
    pub timestamp: Option<i64>,
    pub model: Option<String>,
    /// 随消息出现的图片，保持源内容块的顺序。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidechainInfo {
    pub id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub tool_use_id: Option<String>,
}

impl SidechainInfo {
    /// 边车给的"是谁、干什么":`Explore: find the watcher code`;两项都没有时为空串。
    /// 导出的 `## ⑂ Subagent:` 标题与 MCP 的 subagent 事实行共用,别各拼一份
    pub fn label(&self) -> String {
        [self.agent_type.as_deref(), self.description.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": ")
    }
}

#[derive(Debug, Clone)]
pub struct ParsedTranscript {
    pub meta: SessionMeta,
    pub mainline: Vec<TranscriptMessage>,
    pub sidechains: Vec<SidechainInfo>,
    pub unknown_line_count: u32,
}

/// FTS 索引单元(从解析后消息派生,seq 与详情页一致)
#[derive(Debug, Clone)]
pub struct IndexUnit {
    pub seq: i64,
    pub sidechain_id: Option<String>,
    pub role: Role,
    pub timestamp: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct SessionFileRef {
    pub agent: AgentId,
    pub native_id: String,
    pub file_path: String,
    pub mtime_ms: i64,
    pub size: i64,
}

impl SessionFileRef {
    /// 从库内 meta 重建(打开详情/导出等 UI 入口共用)。SQLite 型会话的
    /// file_path 是虚拟路径(`<db>#<id>`),stat 失败时回退库内时间与大小。
    pub fn from_meta(meta: &SessionMeta) -> Self {
        let stat = std::fs::metadata(&meta.file_path).ok();
        Self {
            agent: meta.agent,
            native_id: meta.id.clone(),
            file_path: meta.file_path.clone(),
            mtime_ms: stat
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(meta.updated_at),
            size: stat.map(|m| m.len() as i64).unwrap_or(meta.size_bytes),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub meta: SessionMeta,
    pub units: Vec<IndexUnit>,
    pub unknown_line_count: u32,
    /// agent 在这个会话里查 Wake 的每一次调用(MCP 的 wake_* 工具、shell 里的
    /// wake-cli / wake-mcp),落 wake_lookups 表。与 units 同一处派生
    /// (`ParsedSession::derive`):自指回声过滤把这些调用从索引里跳过了,不记就没有仪表
    pub wake_lookups: Vec<WakeLookup>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub agents: Vec<AgentId>,
    pub favorite_only: bool,
    pub include_archived: bool,
    /// 只返回会话树的可见根节点。父节点不存在或被当前归档口径隐藏时，
    /// 子节点会提升为根，避免形成无法从列表进入的孤儿会话。
    pub roots_only: bool,
    pub title_query: Option<String>,
    pub sort: SortKey,
    /// false = 降序(默认,新到旧/多到少)
    pub ascending: bool,
    pub limit: i64,
    pub offset: i64,
    /// 只要 updated_at ≥ 此刻(epoch ms)的会话;None = 不限(wake-mcp 的 since)
    pub updated_since: Option<i64>,
    /// 项目路径并集;空 = 不限。UI 的单选项目给一个元素,wake-mcp 的 monorepo
    /// 匹配给多个——只此一个字段,别再加"单个项目"的孪生。
    /// 新增筛选字段要同时教会 db.rs `push_session_filters` 与 workbench 的两面
    /// 镜子(`same_session_query` / `session_matches_filter`)
    pub project_paths: Vec<String>,
    /// true = 排序不把置顶提前(wake-mcp 的"最近会话"要的是真按时间;GUI 列表
    /// 保持置顶优先,默认 false)
    pub ignore_pins: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Updated,
    Created,
    Messages,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub path: String,
    pub name: String,
    pub session_count: i64,
    pub last_active: i64,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session: SessionMeta,
    pub seq: i64,
    pub sidechain_id: Option<String>,
    pub role: String,
    pub snippet: String,
    pub timestamp: Option<i64>,
}

/// 全文搜索的筛选面(`Store::search_with`)。`Store::search(q, agents, project,
/// limit)` 是它的便捷形态,UI 用;wake-mcp 需要多项目并集与时间下界
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub agents: Vec<AgentId>,
    /// 项目路径并集;空 = 不限
    pub project_paths: Vec<String>,
    /// 只要 updated_at ≥ 此刻(epoch ms)的会话
    pub updated_since: Option<i64>,
    /// 命中行上限(消息级);0 = 默认 60
    pub limit: i64,
}

/// Insights 页统计快照(`Store::insights` 一次算好)。口径与主 UI 一致:
/// 全部 `archived = 0`;"prompts" = 主线用户消息(role=user 且非 sidechain)。
/// daily 是**全时段**活跃日谱(仅含有时间戳的行,升序)——热力图窗口、
/// streak、busiest day 都由它派生,不另设第二份日数据。
#[derive(Debug, Clone, Default)]
pub struct InsightsData {
    /// 快照的"今天"(查询时传入的本地日):streak 与热力图末列共用它,
    /// 渲染层不再各自读时钟——跨午夜也不会互相错位
    pub as_of: chrono::NaiveDate,
    pub sessions: i64,
    pub prompts: i64,
    /// SUM(tokens_used);多数 adapter 无 token 数据,0 = 不展示
    pub tokens: i64,
    pub project_count: i64,
    /// 最早会话 created_at(ms);0 = 库内无有效时间
    pub first_ts: i64,
    /// 截至 as_of 的连续活跃天数(今天尚无活动时从昨天起算,GitHub 惯例)
    pub current_streak: i64,
    pub longest_streak: i64,
    pub daily: Vec<(chrono::NaiveDate, i64)>,
    /// 本地时段分布(0–23 时)
    pub hourly: [i64; 24],
    /// 星期分布,周一起始(与热力图同序)
    pub weekday: [i64; 7],
    /// 月份分布(1–12 月聚合到 12 桶,跨年叠加)
    pub monthly: [i64; 12],
    /// 每家 agent 的三个度量,按会话数降序;UI 切换度量时自行重排
    pub agents: Vec<UsageTally>,
    /// 全量项目——不截断,top-N 由 UI 按当前度量排序后取(SQL 里加 LIMIT
    /// 会让 Prompts/Tokens 榜漏掉会话数排不进前列的项)
    pub projects: Vec<UsageTally>,
    /// 全量模型,同上
    pub models: Vec<UsageTally>,
    /// 按会话创建日的 (日, 会话数) 序列(升序,仅有效日期、不含未来日)。
    /// "Last 7 days" 周对比由它与 daily 派生。tokens 不进这里:`tokens_used`
    /// 是会话终身累计量,没有时间维度,按创建日切窗会把十天前开的长会话的
    /// 全部用量记到上一窗(2026-09-03 Codex review)
    pub daily_sessions: Vec<(chrono::NaiveDate, i64)>,
    /// 各 agent 近 53 周(周一起始,末项 = as_of 所在周)的每周 prompts;
    /// 与热力图同一时间窗,按总量降序。**不按模型出趋势**:messages 表没有
    /// 逐条 model,会话级 `model` 是末态,按它归因会把整段历史改写成最后
    /// 用的那个模型(同一 review)
    pub trend_agents: Vec<TrendSeries>,
    /// 近 7 天各家 agent 查 Wake 的次数,按接入渠道分列(总数降序,只含 >0 的家)。
    /// 窗与 Last 7 days 同(as_of 收尾的 7 天),按每次调用所在消息的时刻归窗,
    /// 缺时间戳的退回会话 updated_at
    pub wake_lookups_7d: Vec<LookupTally>,
}

/// 时间窗内的度量(Last 7 days 与其前 7 天各一份)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowStats {
    pub sessions: i64,
    pub prompts: i64,
    pub active_days: i64,
}

/// 趋势图的一条序列:`weekly` 长 TREND_WEEKS,末项是 as_of 所在周,向前逐周
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendSeries {
    pub name: String,
    pub weekly: Vec<i64>,
}

impl TrendSeries {
    pub fn total(&self) -> i64 {
        self.weekly.iter().sum()
    }
}

/// 趋势图与热力图共用的周数:52 整周 + 本周
pub const TREND_WEEKS: usize = 53;

impl InsightsData {
    /// 活跃天数 = daily 长度(派生不另存,免第二个写入点失步)
    pub fn active_days(&self) -> i64 {
        self.daily.len() as i64
    }

    pub fn busiest_day(&self) -> Option<(chrono::NaiveDate, i64)> {
        self.daily.iter().max_by_key(|(_, n)| *n).copied()
    }

    /// 以 `ending` 收尾的 7 天闭区间内的度量。会话按创建日归属,prompts 按
    /// 消息日;active_days 数的是有 prompt 的日子(与热力图同口径)。两个日序列
    /// 都升序,二分定位后只读窗内的行(每帧调用,别线性扫全史)
    pub fn week_ending(&self, ending: chrono::NaiveDate) -> WindowStats {
        let start = week_window_start(ending);
        let mut w = WindowStats::default();
        let from = self.daily.partition_point(|(d, _)| *d < start);
        for (_, n) in self.daily[from..].iter().take_while(|(d, _)| *d <= ending) {
            w.prompts += n;
            w.active_days += 1;
        }
        let from = self.daily_sessions.partition_point(|(d, _)| *d < start);
        for (_, sessions) in self.daily_sessions[from..]
            .iter()
            .take_while(|(d, _)| *d <= ending)
        {
            w.sessions += sessions;
        }
        w
    }

    /// 最近 7 天与其前 7 天
    pub fn last_week_pair(&self) -> (WindowStats, WindowStats) {
        (
            self.week_ending(self.as_of),
            self.week_ending(self.as_of - chrono::Days::new(7)),
        )
    }

    /// 趋势图/热力图窗口的首个周一(末列 = as_of 所在周)
    pub fn trend_start(&self) -> chrono::NaiveDate {
        week_start(self.as_of) - chrono::Days::new((TREND_WEEKS as u64 - 1) * 7)
    }
}

/// 周起始 = 周一。Insights 所有"按周"的口径(趋势分桶、热力图列、窗口起点)
/// 只此一处
pub fn week_start(day: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike as _;
    day - chrono::Days::new(day.weekday().num_days_from_monday() as u64)
}

/// Last 7 days 窗的首日(`ending` 收尾的 7 天闭区间)。按日比较的 `week_ending` 与
/// 按时刻比较的 `week_window_start_ms` 同一个定义,窗口只在这里定一次
pub fn week_window_start(ending: chrono::NaiveDate) -> chrono::NaiveDate {
    ending - chrono::Days::new(6)
}

/// 同一窗的起点换成 epoch ms(本地零点):按调用时刻归窗的 SQL 用
pub fn week_window_start_ms(ending: chrono::NaiveDate) -> i64 {
    week_window_start(ending)
        .and_hms_opt(0, 0, 0)
        .and_then(local_ms)
        .unwrap_or(0)
}

/// 本地时间 → epoch ms;夏令时空档里不存在的时刻取最早的解释,None = 无解。
/// since 解析(services/context)与 Insights 的窗起点共用
pub fn local_ms(ndt: chrono::NaiveDateTime) -> Option<i64> {
    use chrono::TimeZone as _;
    chrono::Local
        .from_local_datetime(&ndt)
        .earliest()
        .map(|dt| dt.timestamp_millis())
}

/// 日 → 趋势窗口内的周下标(末项 = as_of 所在周);窗外/未来为 None。
/// 与 `InsightsData::trend_start` 是一对互逆:`trend_start + ix×7 天` 回到该周周一
pub fn trend_week_index(as_of: chrono::NaiveDate, day: chrono::NaiveDate) -> Option<usize> {
    let weeks_back = (week_start(as_of) - week_start(day)).num_days() / 7;
    (0..TREND_WEEKS as i64)
        .contains(&weeks_back)
        .then(|| TREND_WEEKS - 1 - weeks_back as usize)
}

/// Insights 榜单的一行(agent/项目/模型共用)。tokens = 0 表示该组不报
/// token(而非用了 0),UI 按此语义隐藏而不是画空条
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTally {
    pub name: String,
    pub sessions: i64,
    pub prompts: i64,
    pub tokens: i64,
}

/// agent 查 Wake 的一次调用(解析时从工具调用里认出,落 wake_lookups 表)。
/// `timestamp` 是所在消息的时间,缺则 None——归窗时退回会话的 updated_at
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeLookup {
    pub seq: i64,
    pub timestamp: Option<i64>,
    pub channel: LookupChannel,
    /// MCP 工具名(wake_search 等)或命令行二进制名(wake-cli / wake-mcp);两边都
    /// 出自常量表,所以是 &'static
    pub tool: &'static str,
}

/// 调用走的是哪条接入:MCP 的 wake_* 工具,还是 shell 里的 wake-cli / wake-mcp。
/// MCP 那一路按工具名认、天然精确;CLI 那一路按命令文本认,开发 Wake 自己的机器上
/// 会混进写源码、构建这类命令(2026-09-17 实测),两路分开列才看得清
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupChannel {
    Mcp,
    Cli,
}

impl LookupChannel {
    /// 落库/传输用的小写名
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Cli => "cli",
        }
    }
}

/// Insights "Agents asking Wake" 的一行:近 7 天该家 agent 经两条接入各查了几次
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupTally {
    pub name: String,
    pub mcp: i64,
    pub cli: i64,
}

impl LookupTally {
    pub fn total(&self) -> i64 {
        self.mcp + self.cli
    }
}

/// 搜索 snippet 高亮哨兵(UI 层替换为高亮样式)
pub const HL_OPEN: char = '\u{e000}';
pub const HL_CLOSE: char = '\u{e001}';

/// 单条消息正文入库/传输上限
pub const MAX_MSG_TEXT: usize = 32 * 1024;
/// 单张图片的 base64 文本上限（约 12 MiB 原始图片）。
pub const MAX_IMAGE_B64: usize = 16 * 1024 * 1024;
/// 本地引用图片的读取上限，与内联图片解码后的量级保持一致。
pub const MAX_IMAGE_BYTES: u64 = 12 * 1024 * 1024;
/// tool 输入/输出、thinking 上限
pub const MAX_TOOL_IO: usize = 16 * 1024;
pub const MAX_TITLE: usize = 80;

/// agent 自己写下的记忆文档(Claude Code 的 auto-memory、Codex 的 memories)。Wake
/// 只读:列出、搜索、阅读,不编辑、不同步、不删除——写别家数据破铁律,各家格式
/// 与语义也不同。落 memories 表,扫描收尾时由各家 `list_memories` 按 (agent, host)
/// 整组替换
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDoc {
    /// `{agent}:{path}`,远程 `{agent}:{host}:{path}`——与会话 key 同一构造点
    /// (`session_key`),远程装饰器插 host 的逻辑因此一份通吃;agent 恒为首段
    pub key: String,
    pub agent: AgentId,
    /// 远程 host 名;空 = 本地
    pub host: String,
    pub scope: MemoryScope,
    /// 归属的项目路径。adapter 一般不填(留空),读库时按 `session_key` 连 sessions
    /// 表解析——项目归属随索引走、永远是新的,不在写入时定格;adapter 自己知道
    /// 项目时可以直接填,填了优先。用户级恒空
    pub project_path: String,
    pub project_name: String,
    /// 归属锚点:线程级记忆是它所属的会话,Claude 项目级记忆是该项目目录里最新的
    /// 一条会话(同目录的会话同一个 cwd);用户级为空。只有线程级才在界面上标成
    /// "session memory",项目级的锚点只用来解析项目
    pub session_key: String,
    /// 文件路径;SQLite 型是 `<db>#<id>` 虚拟路径(sqlite_ro::virtual_path)
    pub path: String,
    pub title: String,
    /// epoch ms
    pub updated_at: i64,
    pub size_bytes: i64,
    /// 来自哪个来源(`MemorySource::id`):Settings 的计数、停用后的出库、读失败时的
    /// 冻结都按它
    pub source: String,
    /// 正文原样(含 frontmatter);文件型阅读时先读磁盘,这份是索引与兜底
    pub body: String,
}

/// Memory 页侧栏的导航计数:总数、用户级、按 agent、按解析出的项目(用户级记忆计入
/// 总数、`user` 与 agent,不属于任何项目;没归属的记在 path 为空的那一项)
#[derive(Debug, Clone, Default)]
pub struct MemoryCounts {
    pub total: i64,
    /// 用户级(侧栏的 User memory 行)
    pub user: i64,
    pub agents: Vec<(AgentId, i64)>,
    pub projects: Vec<MemoryProject>,
}

/// 一个项目名下的记忆文件数(path 为空 = 没归属的那一组)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryProject {
    pub path: String,
    pub name: String,
    pub count: i64,
    /// 组内最近一次更新,epoch ms
    pub updated_at: i64,
}

/// 记忆文档归哪一组:用户级一组、没归属一组、其余按项目——GUI 的分组/侧栏/页头与
/// MCP 的组头都从这里派生,判据(scope、project_path 空不空)只写一处
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryGroup<'a> {
    User,
    Unknown,
    Project { path: &'a str, name: &'a str },
}

impl MemoryDoc {
    pub fn group(&self) -> MemoryGroup<'_> {
        if self.scope == MemoryScope::User {
            MemoryGroup::User
        } else if self.project_path.is_empty() {
            MemoryGroup::Unknown
        } else {
            MemoryGroup::Project {
                path: &self.project_path,
                name: &self.project_name,
            }
        }
    }
}

/// 记忆的归属层级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// 跨项目的用户级(Codex 的 memories/*.md)
    User,
    /// 某个项目的(Claude Code 的 projects/<dir>/memory/)
    Project,
    /// 某条会话的(Codex 的 stage1_outputs 逐线程摘要)
    Thread,
}

impl MemoryScope {
    /// 落库/传输用的小写名
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Thread => "thread",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "thread" => Some(Self::Thread),
            _ => None,
        }
    }

    /// 人读标签(MCP 输出用;UI 另过 t()),后面接 " memory":User memory / Project
    /// memory / Session memory——沿用 Claude Code 文档的说法(用户 2026-09-21 定)
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Thread => "session",
        }
    }
}

/// 记忆来源的形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySourceKind {
    /// 按项目目录的记忆树:`<dir>/<项目目录>/memory/*.md`(Claude auto-memory,ZCode 同形)
    ProjectTree,
    /// 一个目录直属的 `*.<ext>`(Codex 的 memories、`~/.codex/rules/*.rules`)
    Dir { ext: &'static str },
    /// 单个文件(`~/.claude/CLAUDE.md`、`~/.codex/AGENTS.md`、`~/.gemini/GEMINI.md`)
    File,
    /// 用户添加的路径:是目录就读直属 `*.md`,是文件就读它自己——**读的时候才看形状**
    /// (构造不摸磁盘,与别的来源"不看存不存在"同规矩;曾在构造时按 `is_file()` 定成
    /// Dir / File,2026-09-22 /simplify)
    Custom,
    /// Codex memories_1.sqlite 的逐线程摘要
    ThreadDb,
    /// 每个已索引的本地项目根下的一个文件:`<project>/<rel>`
    ProjectFile,
    /// 每个已索引的本地项目根下的一个目录:`<project>/<rel>/*.<ext>`
    ProjectDir { ext: &'static str },
}

/// 记忆的一个来源 = Settings → Memory locations 的一行。各家 adapter 按自己的根报默认
/// 来源(不看存不存在),项目模式由 `adapters::project_instruction_sources` 按 agent
/// 给;用户可停用默认来源、添加自定义的目录或文件(store 记偏离,scanner 每轮按
/// 配置裁决后交给 `list_memories`)。不区分"agent 记的"与"用户写的指令"——曾有过
/// origin 字段与 instructions 徽章,文件名本身已说明身份,用户 2026-09-21 定拿掉
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub agent: AgentId,
    pub kind: MemorySourceKind,
    /// 目录 / 文件 / 库的绝对路径;项目模式是相对项目根的路径(`CLAUDE.md`、`.cursor/rules`)
    pub path: std::path::PathBuf,
}

impl MemorySource {
    pub fn is_pattern(&self) -> bool {
        matches!(
            self.kind,
            MemorySourceKind::ProjectFile | MemorySourceKind::ProjectDir { .. }
        )
    }

    /// 层级由形态决定:全局目录 / 文件 / 自定义路径是用户级(对每个项目都成立),记忆树
    /// 与项目模式是项目级,线程库是会话级——曾是独立字段,每个构造点填的都是 kind 蕴含
    /// 的值(2026-09-22 /simplify)
    pub fn scope(&self) -> MemoryScope {
        match self.kind {
            MemorySourceKind::Dir { .. } | MemorySourceKind::File | MemorySourceKind::Custom => {
                MemoryScope::User
            }
            MemorySourceKind::ProjectTree
            | MemorySourceKind::ProjectFile
            | MemorySourceKind::ProjectDir { .. } => MemoryScope::Project,
            MemorySourceKind::ThreadDb => MemoryScope::Thread,
        }
    }

    /// 稳定标识:停用记录、memories.source 列、按来源冻结与计数都用它。项目模式带
    /// `<project>/` 前缀,具体路径就是路径本身
    pub fn id(&self) -> String {
        let path = self.path.to_string_lossy();
        if self.is_pattern() {
            format!("<project>/{path}")
        } else {
            path.to_string()
        }
    }

    /// 用户添加的来源(Settings → Memory locations 的 Add location):目录或文件,读时才看
    pub fn custom(agent: AgentId, path: std::path::PathBuf) -> Self {
        Self {
            agent,
            kind: MemorySourceKind::Custom,
            path,
        }
    }
}

/// 用户记忆(对每个项目都成立)在一次筛选里怎么算——它是唯一带特殊语义的层级
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserMemories {
    /// 照列,项目筛选下也放行:MCP 的口径,问一个项目的记忆连用户记忆一起给
    #[default]
    Alongside,
    /// 不列:GUI 的项目行只算项目记忆(用户 2026-09-22 定)
    Excluded,
    /// 只列用户记忆:GUI 侧栏的 User memory 行
    Only,
}

/// 记忆列表/搜索的筛选面(Memory 页与 wake_list_memories / wake_search 共用)
#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    pub agents: Vec<AgentId>,
    /// 项目路径并集;空串不匹配任何东西。用户级记忆对每个项目都成立,`user` 默认
    /// (Alongside)下任何项目筛选都列出它
    pub project_paths: Vec<String>,
    /// 只要没归属(项目解析不出)的那一组——GUI 侧栏的 Unknown project 行;与
    /// `project_paths` 一起时取并集。显式一位,不拿空串当暗号
    pub unattributed: bool,
    /// 用户级记忆进不进这次筛选
    pub user: UserMemories,
    /// 只要这个时刻(epoch ms)之后更新过的;None = 不限。wake_search 的 `since` 也约束
    /// 记忆命中——否则 "No session matches … in the last 24 hours" 后面跟着几个月前的
    /// 笔记(2026-09-22 review)
    pub updated_since: Option<i64>,
    /// 0 = 不限
    pub limit: i64,
}

impl MemoryFilter {
    /// 有没有任何项目维度的筛选
    pub fn scopes_projects(&self) -> bool {
        !self.project_paths.is_empty() || self.unattributed
    }
}

/// 记忆搜索的一条命中(snippet 带 HL_OPEN/HL_CLOSE 高亮哨兵,与会话命中同款)
#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub doc: MemoryDoc,
    pub snippet: String,
}

/// 无标题会话的占位标题(quickMeta 守卫与 adapters 共用,防半/全角漂移)
pub const UNTITLED: &str = "Untitled";

/// 会话 key 的唯一构造点:本地 `{agent}:{native_id}`,远程
/// `{agent}:{host}:{native_id}`。scanner 的墓碑查询、watcher 的幸存者反查、
/// 远程装饰器的 key 改写都走这里——"远程 key 长什么样"只此一处知识。
/// (二十家 adapter 的本地两段构造保留各自 `format!`,它们从不涉及 host。)
pub fn session_key(agent: AgentId, host: &str, native_id: &str) -> String {
    if host.is_empty() {
        format!("{}:{native_id}", agent.as_str())
    } else {
        format!("{}:{host}:{native_id}", agent.as_str())
    }
}

/// session_key 的逆:key 属于该 (agent, host) 实例时返回 native_id,否则
/// None。watcher 的幸存者反查用它按实例过滤,零分配。
pub fn strip_key_prefix<'a>(key: &'a str, agent: AgentId, host: &str) -> Option<&'a str> {
    let rest = key.strip_prefix(agent.as_str())?.strip_prefix(':')?;
    if host.is_empty() {
        // 本地两段 key;含 ':' 的余段是别的 host 的三段 key,不归本实例。
        // (native_id 自身带 ':' 的 agent 当前不存在,UUID/时间戳系文件名)
        (!rest.contains(':')).then_some(rest)
    } else {
        rest.strip_prefix(host)?.strip_prefix(':')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_roundtrip() {
        let local = session_key(AgentId::ClaudeCode, "", "u1");
        assert_eq!(local, "claude-code:u1");
        assert_eq!(
            strip_key_prefix(&local, AgentId::ClaudeCode, ""),
            Some("u1")
        );

        let remote = session_key(AgentId::ClaudeCode, "devbox", "u1");
        assert_eq!(remote, "claude-code:devbox:u1");
        assert_eq!(
            strip_key_prefix(&remote, AgentId::ClaudeCode, "devbox"),
            Some("u1")
        );
    }

    #[test]
    fn strip_key_prefix_rejects_wrong_instance() {
        // 本地实例不认远程 key(余段带 host 冒号)
        assert_eq!(
            strip_key_prefix("claude-code:devbox:u1", AgentId::ClaudeCode, ""),
            None
        );
        // 远程实例不认本地 key,也不认别家 host——含 host 是另一 host 前缀的
        assert_eq!(
            strip_key_prefix("claude-code:u1", AgentId::ClaudeCode, "devbox"),
            None
        );
        assert_eq!(
            strip_key_prefix("claude-code:devbox2:u1", AgentId::ClaudeCode, "devbox"),
            None
        );
        // agent 段不匹配
        assert_eq!(strip_key_prefix("codex:u1", AgentId::ClaudeCode, ""), None);
    }
}
