//! Wake 界面尺度规范——六档字阶。
//! 规则:所有 UI 字号必须引用本模块常量,禁止裸 px 数字与 rem 工具类
//! (text_sm 等按 rem=14px 折算会产生 12.25px 这类幽灵值)。
//! 颜色只用三级:foreground(主文字)/muted_foreground(辅助)/primary(强调)。
use gpui::{px, Hsla, Pixels, Styled};

/// 上下文大标题(中栏头部)——semibold。
/// Windows Fluent 标题略收紧，给三栏布局留出更多阅读空间。
pub const FONT_TITLE: Pixels = if cfg!(target_os = "windows") {
    px(20.)
} else {
    px(22.)
};
/// 区块标题(详情页会话标题)——semibold
pub const FONT_HEADING: Pixels = px(16.);
/// 界面正文(导航行/列表标题/按钮/输入);列表标题用 medium
pub const FONT_BODY: Pixels = px(14.);
/// 辅助说明(列表副行/元信息/占位/空态提示/侧栏子级行)
pub const FONT_CAPTION: Pixels = px(12.);
/// 标签(分组头/计数/快捷键徽标/状态栏);组头 semibold + 大写
pub const FONT_LABEL: Pixels = px(11.);

// ---- 对话区附档(详情逐消息渲染,经用户逐轮校准,与六档并存)----

/// 用户气泡正文(比 FONT_BODY 收半档,气泡内更紧凑)
pub const FONT_MSG_USER: Pixels = px(13.5);
/// 助手平铺正文
pub const FONT_MSG_BODY: Pixels = px(13.);
/// thinking 摘要行(斜体)
pub const FONT_MSG_THINKING: Pixels = px(11.5);

// ---------------- 间距与结构 ----------------
// 间距刻度:4px 网格。新代码一律引用常量或显式 px();
// 注意 gpui 的 rem 间距类有幽灵值(rem=14px 下 p_2p5=8.75px、p_3=10.5px,
// 均不在网格上)——对齐敏感处禁止使用,存量逐步迁移。

pub const SPACE_XS: Pixels = px(4.);
pub const SPACE_SM: Pixels = px(8.);
pub const SPACE_MD: Pixels = px(12.);
pub const SPACE_LG: Pixels = px(16.);
pub const SPACE_XL: Pixels = px(20.);
pub const SPACE_XXL: Pixels = px(24.);

/// Windows 侧栏采用更宽的导航栏，方便鼠标和触控命中；macOS 保持原布局。
pub const SIDEBAR_WIDTH: Pixels = if cfg!(target_os = "windows") {
    px(248.)
} else {
    px(224.)
};
/// 会话列表宽度：Windows 上为标题、筛选和双行摘要预留更稳定的空间。
pub const LIST_WIDTH: Pixels = if cfg!(target_os = "windows") {
    px(360.)
} else {
    px(336.)
};

/// 侧栏容器水平内边距(行的 hover/选中背景左右各留这么多)。
pub const SIDEBAR_EDGE: Pixels = if cfg!(target_os = "windows") {
    px(12.)
} else {
    px(10.)
};

// ---- 侧栏中轴:LEAD_AXIS = 26.75 ----
// traffic light 实测左缘 20、直径 13.5,中心即 26.75。侧栏每一行的行首元素
// (导航图标、品牌图、文件夹图标、组头首字母)的**中心**都压在这条竖线上。
// 注意:中心对齐与左缘对齐是同一个自由度,只能满足一个——选了中心,
// 图标左缘就会落在 17.75,比红灯左缘还靠左 2.25,这是预期而非错位。
// 三个内边距是同一条轴推出来的,改任何一个都必须重算另外两个。

/// 行首槽位:取最大前导元素(品牌图 18px)的尺寸;槽位内**居中**,
/// 于是 14/15px 的小图标中心也落在轴上。
pub const LEAD_BOX: Pixels = px(18.);
/// 行左内边距 = 26.75(轴) − 9(槽位半宽) − 10(SIDEBAR_EDGE,容器那一半)
pub const LEAD_INSET: Pixels = if cfg!(target_os = "windows") {
    px(8.)
} else {
    px(7.75)
};
/// 分组项(agent/项目)相对轴的右缩进,表达从属。
/// 压轴的只有主导航行与组头;分组项一律偏这么多。
pub const SUB_INDENT: Pixels = px(12.);
/// 组头左内边距:让首字母字形中心落在轴上。组头用 FONT_BODY 常规字重,
/// 实测 A 宽 8.25、P 宽 7.25,两者字形中心恰好重合,一个值同时命中两个组头。
/// **字号或字重一变这个数就失效**——它是从字形宽度反推的,不是间距刻度
/// (semibold 时实测需 12.0/12.25,两个组头还对不上同一个值)。
/// 12.125 实测落位 −0.125;调到 12.25 反而变成 +0.375——2x 屏光栅化步长是
/// 0.5px,两者落进不同物理像素,别再往小数点后调了。
pub const GROUP_HEAD_INSET: Pixels = if cfg!(target_os = "windows") {
    px(12.)
} else {
    px(12.125)
};
/// 侧栏标题 "Wake" 的左内边距:同样让首字母 W 的字形中心落在轴上。
/// 实测 16px semibold 的 W 宽 14.25、左承距 0.5,文字左缘需 19.65;
/// 减去容器的 SIDEBAR_EDGE 得 9.15,取 9.0(2x 屏下 0.5px 以下的差会被光栅化吃掉)。
pub const TITLE_INSET: Pixels = if cfg!(target_os = "windows") {
    px(8.)
} else {
    px(9.)
};
/// 侧栏主导航行高(固定区:All Sessions/Starred;搜索框同高)
pub const ROW_HEIGHT: Pixels = if cfg!(target_os = "windows") {
    px(36.)
} else {
    px(32.)
};
/// 侧栏子级行高(分组展开项:agent/项目)——比主导航低一级,
/// 配 FONT_CAPTION 形成侧栏纵向层级(macOS 原生侧栏惯例)
pub const ROW_HEIGHT_SUB: Pixels = if cfg!(target_os = "windows") {
    px(30.)
} else {
    px(26.)
};

/// Windows 控件使用 32px 的标准命中高度；macOS 保持紧凑的工具栏。
pub const TOOLBAR_HEIGHT: Pixels = if cfg!(target_os = "windows") {
    px(32.)
} else {
    px(28.)
};

/// 跨平台键盘提示，避免把 macOS 的 ⌘ 符号泄漏到 Windows 界面。
pub const SEARCH_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "⌘K"
} else {
    "Ctrl+K"
};

// ---------------- 交互态文字 ----------------

/// gpui 陷阱兜底:`.hover()`/`.active()` 闭包的 text 样式对 base 是**整体替换**
/// (`Style.text` 无 `#[refineable]`,refine 直接覆盖)——闭包里只改文字色会把
/// base 的字号一并丢掉、回退窗口默认 14px。交互闭包内改文字色一律走本方法,
/// 把该元素 base 的字号原样重申;禁止在 hover/active 里裸调 `text_color`
/// (图标-only 元素除外:图标尺寸走 `with_size`,不受 text 替换影响)。
pub trait TextColored: Styled + Sized {
    fn text_colored(self, color: Hsla, size: Pixels) -> Self {
        self.text_color(color).text_size(size)
    }
}

impl<T: Styled + Sized> TextColored for T {}
