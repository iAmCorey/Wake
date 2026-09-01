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
}

impl AgentId {
    /// 全部十四家,**枚举声明序**(= Ord = 用户钉的侧栏展示序;面板成组、
    /// 表单下拉共用同一顺序)。曾误抄 create_adapters 的构造序,下拉与侧栏
    /// 排序当场对不上——契约测试现在卡它与 Ord 一致
    pub const ALL: [AgentId; 14] = [
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
        }
    }

    /// agent 品牌图标——wake crate `Assets` 内嵌的 PNG 路径(lobe-icons 素材,
    /// 与 kooky 同源)。后缀必须带上(与 SVG 图标同理,漏后缀 = 静默空白)。
    /// Copilot/Cursor/OpenCode/Pi/Grok/Kimi 是单色字形(白色+alpha):深色模式
    /// 用白色版,浅色模式用 `-light`(深墨 #2B2A26)版——等效 kooky 的染色;
    /// Qoder 是白/黑字形配绿色品牌色,同样按模式切图；其余彩色品牌
    /// (Claude/Codex/Gemini/Kiro/Omp/Antigravity)保持原色,两模式通用。
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
        }
    }
}

/// 会话元数据 —— 列表页/索引库统一模型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// 全局唯一键: `{agent}:{native_id}`
    pub key: String,
    /// 原生会话 id(resume 用)
    pub id: String,
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
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidechainInfo {
    pub id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub tool_use_id: Option<String>,
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
}

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub agents: Vec<AgentId>,
    pub project_path: Option<String>,
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
}

impl InsightsData {
    /// 活跃天数 = daily 长度(派生不另存,免第二个写入点失步)
    pub fn active_days(&self) -> i64 {
        self.daily.len() as i64
    }

    pub fn busiest_day(&self) -> Option<(chrono::NaiveDate, i64)> {
        self.daily.iter().max_by_key(|(_, n)| *n).copied()
    }
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

/// 搜索 snippet 高亮哨兵(UI 层替换为高亮样式)
pub const HL_OPEN: char = '\u{e000}';
pub const HL_CLOSE: char = '\u{e001}';

/// 单条消息正文入库/传输上限
pub const MAX_MSG_TEXT: usize = 32 * 1024;
/// tool 输入/输出、thinking 上限
pub const MAX_TOOL_IO: usize = 16 * 1024;
pub const MAX_TITLE: usize = 80;

/// 无标题会话的占位标题(quickMeta 守卫与 adapters 共用,防半/全角漂移)
pub const UNTITLED: &str = "Untitled";
