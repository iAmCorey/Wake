//! Wake 界面尺度规范——六档字阶。
//! 规则:所有 UI 字号必须引用本模块常量,禁止裸 px 数字与 rem 工具类
//! (text_sm 等按 rem=14px 折算会产生 12.25px 这类幽灵值)。
//! 颜色只用三级:foreground(主文字)/muted_foreground(辅助)/primary(强调)。
use gpui::{px, Hsla, Pixels, Styled};

/// 产品展示名（About）——medium
pub const FONT_DISPLAY: Pixels = px(28.);
/// 上下文大标题(中栏头部)——semibold
pub const FONT_TITLE: Pixels = px(22.);
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

// ---------------- 平台文案 ----------------
// 同一处 UI 在不同平台叫不同名字/键(Finder vs File Explorer vs 文件管理器、
// ⌘ vs Ctrl),文案引用这里的常量,别在 render 里散落字面量。cfg! 表达式让
// 各支在任一平台都过类型检查(名词与 wake-core terminal 的平台实现对应:
// macos=Finder,windows=资源管理器官方英文名 File Explorer)。

pub const SHOW_IN_FM: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else if cfg!(target_os = "windows") {
    "Show in File Explorer"
} else {
    "Show in File Manager"
};
pub const REVEAL_IN_FM: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Reveal in File Explorer"
} else {
    "Reveal in File Manager"
};

// 系统回收站的平台名词(macOS 与 freedesktop 都叫 Trash,Windows 是
// Recycle Bin)与由它派生的三句删除文案。名词只写一次、整句由 concat!
// 拼出:三处分别手写 cfg! 链的话,没有任何东西能拦住它们说不同的词
// ——与 wake-core 各平台 trash_existing 的失败文案也得是同一个词。
//
// 全部是 &'static str 而非调用点 format!:确认框内容活在 **dialog builder
// 闭包**里,对话框开着时每帧重跑,留 format! 在里面就是每帧堆分配。
macro_rules! trash_copy {
    ($noun:literal, $body:literal) => {
        /// 删除确认框主按钮 / 更多菜单项
        pub const MOVE_TO_TRASH: &str = concat!("Move to ", $noun);
        /// 删除成功通知
        pub const SESSION_TRASHED: &str = concat!("Session moved to ", $noun);
        /// 删除确认框正文首句(冠词随名词变,故整句单列)
        pub const TRASH_CONFIRM_BODY: &str = concat!(
            "The session file will be moved to ",
            $body,
            ". You can restore it anytime:"
        );
    };
}
#[cfg(target_os = "windows")]
trash_copy!("Recycle Bin", "the Recycle Bin");
#[cfg(not(target_os = "windows"))]
trash_copy!("Trash", "Trash");

/// ⌘K 面板的唯一绑定串——main.rs 的 bind_keys 与下方徽标同源,改键只动这里
pub const SEARCH_KEYSTROKE: &str = "secondary-k";

/// 搜索快捷键徽标,从真实绑定串派生("⌘K"/"Ctrl+K"——secondary→平台键
/// 与显示形制都由 gpui/Kbd 持有,这里零平台知识)
pub fn search_key_hint() -> &'static str {
    use std::sync::OnceLock;
    static HINT: OnceLock<String> = OnceLock::new();
    HINT.get_or_init(|| {
        let key = gpui::Keystroke::parse(SEARCH_KEYSTROKE).expect("SEARCH_KEYSTROKE parses");
        gpui_component::kbd::Kbd::format(&key)
    })
}

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

/// 侧栏容器水平内边距(行的 hover/选中胶囊左右各留这么多)。
pub const SIDEBAR_EDGE: Pixels = px(10.);

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
pub const LEAD_INSET: Pixels = px(7.75);
/// 分组项(agent/项目)相对轴的右缩进,表达从属。
/// 压轴的只有主导航行与组头;分组项一律偏这么多。
pub const SUB_INDENT: Pixels = px(12.);
/// 组头左内边距:让首字母字形中心落在轴上。组头用 FONT_BODY 常规字重,
/// 实测 A 宽 8.25、P 宽 7.25,两者字形中心恰好重合,一个值同时命中两个组头。
/// **字号或字重一变这个数就失效**——它是从字形宽度反推的,不是间距刻度
/// (semibold 时实测需 12.0/12.25,两个组头还对不上同一个值)。
/// 12.125 实测落位 −0.125;调到 12.25 反而变成 +0.375——2x 屏光栅化步长是
/// 0.5px,两者落进不同物理像素,别再往小数点后调了。
pub const GROUP_HEAD_INSET: Pixels = px(12.125);
/// 侧栏标题 "Wake" 的左内边距:同样让首字母 W 的字形中心落在轴上。
/// 实测 16px semibold 的 W 宽 14.25、左承距 0.5,文字左缘需 19.65;
/// 减去容器的 SIDEBAR_EDGE 得 9.15,取 9.0(2x 屏下 0.5px 以下的差会被光栅化吃掉)。
pub const TITLE_INSET: Pixels = px(9.);
/// 侧栏主导航行高(固定区:All Sessions/Starred;搜索框同高)
pub const ROW_HEIGHT: Pixels = px(32.);
/// 侧栏子级行高(分组展开项:agent/项目)——比主导航低一级,
/// 配 FONT_CAPTION 形成侧栏纵向层级(macOS 原生侧栏惯例)
pub const ROW_HEIGHT_SUB: Pixels = px(26.);

// ---------------- 圆角 ----------------
// 四档见 DESIGN.md:面板 12 = `theme.radius_lg`,列表与侧栏选择 8 = `theme.radius`
// (这两档走主题 token),其余三档无 token,在此定名以免散成魔法数字。
/// 按钮圆角。**用户钉死 6px,勿改**
pub const RADIUS_BUTTON: Pixels = px(6.);
/// 快捷键标签
pub const RADIUS_KBD: Pixels = px(5.);
/// 小胶囊 badge(项目名 / model / source / 各处计数共用)
pub const RADIUS_BADGE: Pixels = px(4.);
/// 数据可视化小色块(热力图、分布柱、图例)，统一保持方格读数感。
pub const RADIUS_CELL: Pixels = px(2.);

// ---------------- 组件度量派生 ----------------

/// gpui-component `Size::Small` 按钮的水平内边距(px_3 @ rem14 = 10.5)。
/// location 表单的"内容轴"对齐从它派生——组件升级或 rem 基准变更先核此值
pub const BUTTON_SM_PX: Pixels = px(10.5);
/// gpui-component `Size::Small` 按钮高度(h_6 = 24),手排胶囊钮取齐用
pub const BUTTON_SM_H: Pixels = px(24.);

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
