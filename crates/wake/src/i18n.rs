//! Wake 自有界面文案。
//!
//! gpui-component 负责通用控件的 locale；这里负责 Wake 的业务界面文案，
//! 让语言切换不需要在渲染代码里散落大量平台/语言判断。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Chinese,
}

impl Language {
    pub fn from_env() -> Self {
        match std::env::var("WAKE_LANG").as_deref() {
            Ok("zh") | Ok("zh-CN") | Ok("中文") => Self::Chinese,
            _ => Self::English,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh-CN",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }

    /// 语言按钮显示的是“切换到哪种语言”，而不是当前语言。
    pub fn switch_label(self) -> &'static str {
        match self {
            Self::English => "中文",
            Self::Chinese => "English",
        }
    }

    pub fn text(self, key: TextKey) -> &'static str {
        match self {
            Self::English => key.english(),
            Self::Chinese => key.chinese(),
        }
    }

    pub fn tools_summary(self, count: usize, names: &str) -> String {
        match self {
            Self::English => format!("{} tools · {}", count, names),
            Self::Chinese => format!("{} 个工具 · {}", count, names),
        }
    }

    pub fn failed_count(self, count: usize) -> String {
        match self {
            Self::English => format!("{count} failed"),
            Self::Chinese => format!("{count} 个失败"),
        }
    }
}

#[derive(Clone, Copy)]
pub enum TextKey {
    ToggleLanguage,
    SearchSessions,
    AllSessions,
    Starred,
    Agents,
    Projects,
    LiveUpdatesOff,
    Now,
    Refreshing,
    RefreshFailed,
    NoMatchingSessions,
    TryDifferentFilters,
    DateUpdated,
    DateCreated,
    MessageCount,
    SortSessions,
    Descending,
    Ascending,
    NoSessionSelected,
    PickSession,
    ExportMarkdown,
    RevealInExplorer,
    RevealInFinder,
    CopySessionId,
    MoveToRecycleBin,
    MoveToTrash,
    OpenSessionIn,
    Unstar,
    Star,
    Unpin,
    Pin,
    UnknownProject,
    Messages,
    Sessions,
    Tokens,
    Created,
    Updated,
    LoadingSession,
    ContextCompacted,
    Thinking,
    SearchEverything,
    SearchFullConversation,
    SearchMatchesHint,
    NoResultsFor,
    TryDifferentQuery,
    ShortQueryFallback,
    ScopeAllSessions,
    Navigate,
    Open,
    EscClose,
    LookingForSessions,
    RefreshingSessions,
    SessionsRefreshed,
    OpenedInTerminal,
    ResumeFailed,
    ExportedTo,
    ExportFailed,
    SessionMovedToRecycleBin,
    SessionMovedToTrash,
    DeleteFailed,
    DeleteConfirmTitle,
    DeleteConfirmRecycleBin,
    DeleteConfirmTrash,
    CodexRecords,
}

impl TextKey {
    fn english(self) -> &'static str {
        match self {
            Self::ToggleLanguage => "Switch language",
            Self::SearchSessions => "Search sessions",
            Self::AllSessions => "All Sessions",
            Self::Starred => "Starred",
            Self::Agents => "Agents",
            Self::Projects => "Projects",
            Self::LiveUpdatesOff => "Live updates off",
            Self::Now => "now",
            Self::Refreshing => "Refreshing…",
            Self::RefreshFailed => "Refresh failed",
            Self::NoMatchingSessions => "No matching sessions",
            Self::TryDifferentFilters => "Try different filters or clear the query",
            Self::DateUpdated => "Date updated",
            Self::DateCreated => "Date created",
            Self::MessageCount => "Message count",
            Self::SortSessions => "Sort sessions",
            Self::Descending => "Descending",
            Self::Ascending => "Ascending",
            Self::NoSessionSelected => "No session selected",
            Self::PickSession => "Pick one from the list, or press",
            Self::ExportMarkdown => "Export as Markdown",
            Self::RevealInExplorer => "Reveal in Explorer",
            Self::RevealInFinder => "Reveal in Finder",
            Self::CopySessionId => "Copy Session ID",
            Self::MoveToRecycleBin => "Move to Recycle Bin",
            Self::MoveToTrash => "Move to Trash",
            Self::OpenSessionIn => "Open this session in…",
            Self::Unstar => "Unstar",
            Self::Star => "Star",
            Self::Unpin => "Unpin",
            Self::Pin => "Pin",
            Self::UnknownProject => "Unknown project",
            Self::Messages => "messages",
            Self::Sessions => "sessions",
            Self::Tokens => "tokens",
            Self::Created => "Created",
            Self::Updated => "Updated",
            Self::LoadingSession => "Loading session…",
            Self::ContextCompacted => "Context compacted",
            Self::Thinking => "Thinking",
            Self::SearchEverything => "Search everything — prose or code",
            Self::SearchFullConversation => "Search full conversation text",
            Self::SearchMatchesHint => "Matches natural language and code, like \"useEffect(\".",
            Self::NoResultsFor => "No results for",
            Self::TryDifferentQuery => "Try a different or shorter query.",
            Self::ShortQueryFallback => {
                "Short query — using fallback search. Longer keywords are faster."
            }
            Self::ScopeAllSessions => "Scope: all sessions",
            Self::Navigate => "navigate",
            Self::Open => "open",
            Self::EscClose => "esc close",
            Self::LookingForSessions => "Looking for sessions…",
            Self::RefreshingSessions => "Refreshing sessions",
            Self::SessionsRefreshed => "Sessions refreshed",
            Self::OpenedInTerminal => "Opened in terminal",
            Self::ResumeFailed => "Resume failed",
            Self::ExportedTo => "Exported to",
            Self::ExportFailed => "Export failed",
            Self::SessionMovedToRecycleBin => "Session moved to Recycle Bin",
            Self::SessionMovedToTrash => "Session moved to Trash",
            Self::DeleteFailed => "Delete failed",
            Self::DeleteConfirmTitle => "Delete this session?",
            Self::DeleteConfirmRecycleBin => {
                "The session file will be moved to the Recycle Bin. You can restore it anytime:"
            }
            Self::DeleteConfirmTrash => {
                "The session file will be moved to Trash. You can restore it anytime:"
            }
            Self::CodexRecords => {
                "Only the local file is removed — Codex's own records stay intact."
            }
        }
    }

    fn chinese(self) -> &'static str {
        match self {
            Self::ToggleLanguage => "切换语言",
            Self::SearchSessions => "搜索会话",
            Self::AllSessions => "全部会话",
            Self::Starred => "已收藏",
            Self::Agents => "智能体",
            Self::Projects => "项目",
            Self::LiveUpdatesOff => "实时更新已关闭",
            Self::Now => "刚刚",
            Self::Refreshing => "正在刷新…",
            Self::RefreshFailed => "刷新失败",
            Self::NoMatchingSessions => "没有匹配的会话",
            Self::TryDifferentFilters => "请更换筛选条件或清空查询",
            Self::DateUpdated => "更新时间",
            Self::DateCreated => "创建时间",
            Self::MessageCount => "消息数",
            Self::SortSessions => "排序会话",
            Self::Descending => "降序",
            Self::Ascending => "升序",
            Self::NoSessionSelected => "未选择会话",
            Self::PickSession => "请从列表中选择，或按",
            Self::ExportMarkdown => "导出为 Markdown",
            Self::RevealInExplorer => "在文件资源管理器中显示",
            Self::RevealInFinder => "在 Finder 中显示",
            Self::CopySessionId => "复制会话 ID",
            Self::MoveToRecycleBin => "移到回收站",
            Self::MoveToTrash => "移到废纸篓",
            Self::OpenSessionIn => "打开会话…",
            Self::Unstar => "取消收藏",
            Self::Star => "收藏",
            Self::Unpin => "取消置顶",
            Self::Pin => "置顶",
            Self::UnknownProject => "未知项目",
            Self::Messages => "条消息",
            Self::Sessions => "个会话",
            Self::Tokens => "tokens",
            Self::Created => "创建于",
            Self::Updated => "更新于",
            Self::LoadingSession => "正在加载会话…",
            Self::ContextCompacted => "上下文已压缩",
            Self::Thinking => "思考",
            Self::SearchEverything => "搜索全部内容——文字或代码",
            Self::SearchFullConversation => "搜索完整对话内容",
            Self::SearchMatchesHint => "支持自然语言和代码，例如“useEffect(”。",
            Self::NoResultsFor => "没有找到结果：",
            Self::TryDifferentQuery => "请尝试更换关键词或缩短查询。",
            Self::ShortQueryFallback => "查询较短，正在使用备用搜索；更长的关键词更快。",
            Self::ScopeAllSessions => "范围：全部会话",
            Self::Navigate => "导航",
            Self::Open => "打开",
            Self::EscClose => "Esc 关闭",
            Self::LookingForSessions => "正在查找会话…",
            Self::RefreshingSessions => "正在刷新会话",
            Self::SessionsRefreshed => "会话已刷新",
            Self::OpenedInTerminal => "已在终端打开",
            Self::ResumeFailed => "恢复失败",
            Self::ExportedTo => "已导出到",
            Self::ExportFailed => "导出失败",
            Self::SessionMovedToRecycleBin => "会话已移到回收站",
            Self::SessionMovedToTrash => "会话已移到废纸篓",
            Self::DeleteFailed => "删除失败",
            Self::DeleteConfirmTitle => "确定删除此会话？",
            Self::DeleteConfirmRecycleBin => "会话文件将移到回收站，之后可以随时恢复：",
            Self::DeleteConfirmTrash => "会话文件将移到废纸篓，之后可以随时恢复：",
            Self::CodexRecords => "只会删除本地文件，Codex 自己的记录不会受影响。",
        }
    }
}
