//! 界面缩放(issue #51):⌘+ / ⌘− / ⌘0、显示菜单与 Settings → Appearance → Zoom。
//!
//! gpui 没有浏览器那种整窗缩放(`Window::set_scale_factor` 只在 test-support 里),
//! Wake 的尺寸又几乎全是显式像素,所以缩放落在两处,倍数同一个:
//! - Wake 自己的尺寸一律经 `ui::zpx` / `ui::Zpx` 取值,乘上这里的倍数;
//! - gpui-component 的组件按 rem 排版,rem 由 Root 每帧按 `theme.font_size` 设,
//!   `theme::apply_wake_theme` 把它(连同等宽字号与两档圆角)乘上同一个倍数。
//!
//! 100% 档两边都是乘 1.0,与缩放落地前逐像素相同。档位是进程全局的(主窗与设置窗
//! 一起变),落在 prefs 的 `zoom`:开窗前就要读,与 appearance 同理。

use std::sync::atomic::{AtomicU16, Ordering};

use gpui::{actions, point, px, App, KeyBinding, Pixels, Point, Window};
use gpui_component::{notification::Notification, WindowExt as _};

use crate::ui::{zpx, LEAD_AXIS};

actions!(wake_app, [ZoomIn, ZoomOut, ActualSize]);

/// 可选档位(百分比)。只放大不缩小
pub const LEVELS: [u16; 4] = [100, 110, 125, 150];
pub const DEFAULT: u16 = 100;
const PREF: &str = "zoom";

/// 快捷键:main.rs 的键位(`key_bindings`)与设置页的提示(`key_hints`)同源。`+` 与 `=`
/// 都绑:美式键盘上 ⌘+ 实际按的是 ⌘⇧=,别的布局 + 是单独一键;提示显示第一个
const ZOOM_IN_KEYS: [&str; 2] = ["secondary-+", "secondary-="];
const ZOOM_OUT_KEY: &str = "secondary--";
const ACTUAL_SIZE_KEY: &str = "secondary-0";

/// 两扇窗 traffic light 在 100% 档的顶距。开窗时与换档后挪位共用这一个数
pub const MAIN_LIGHTS_TOP: f32 = 15.;
pub const SETTINGS_LIGHTS_TOP: f32 = 11.;
/// macOS traffic light 红灯的直径一半与按钮框高的一半(实测 13.5 / 14)
const LIGHT_HALF_W: f32 = 6.75;
const LIGHT_HALF_H: f32 = 7.;

static PERCENT: AtomicU16 = AtomicU16::new(DEFAULT);

pub fn percent() -> u16 {
    PERCENT.load(Ordering::Relaxed)
}

/// 当前倍数(1.0 = 100%)
pub fn factor() -> f32 {
    f32::from(percent()) / 100.
}

/// 档位的显示形制("125%"),设置页下拉的按钮与菜单项共用
pub fn label(level: u16) -> String {
    format!("{level}%")
}

/// 读落盘的档位。必须在 `theme::sync_appearance` 与开任何窗之前
pub fn init() {
    if let Some(level) = crate::prefs::read(PREF).as_deref().and_then(parse) {
        PERCENT.store(level, Ordering::Relaxed);
    }
}

/// 只认档位表里的值:手改的偏好文件与去掉了的档位都退回默认
fn parse(text: &str) -> Option<u16> {
    text.trim()
        .parse()
        .ok()
        .filter(|level| LEVELS.contains(level))
}

/// 相邻档位,到头给 None
fn step(from: u16, up: bool) -> Option<u16> {
    let ix = LEVELS.iter().position(|&level| level == from)?;
    if up {
        LEVELS.get(ix + 1).copied()
    } else {
        ix.checked_sub(1).map(|ix| LEVELS[ix])
    }
}

/// 换档:落盘 → 改倍数 → 重套主题(rem / 等宽字号 / 圆角)→ 重绘全部窗口;同档早退。
/// 落盘失败不换档(与 appearance、语言一致),通知经 `with_active_window` 推迟后贴到当前
/// 窗口——全局 action 监听器是在派发它的那扇窗口的 update 期间被调用的,那扇窗正被借出
pub fn change(level: u16, cx: &mut App) {
    if level == percent() {
        return;
    }
    if let Err(error) = crate::prefs::write(PREF, level.to_string().as_bytes()) {
        let message = crate::tf!("Couldn't save zoom: {}", error);
        crate::with_active_window(cx, move |window, cx| {
            window.push_notification(Notification::error(message), cx)
        });
        return;
    }
    PERCENT.store(level, Ordering::Relaxed);
    crate::theme::apply_wake_theme(cx);
    cx.refresh_windows();
}

/// 三个缩放 action 挂成**全局**监听器:不管焦点在哪(弹窗里、下拉菜单刚关、两扇窗
/// 哪扇在前,甚至焦点丢了)都得生效——挂在窗口根上的话,焦点一旦不在根的子树里
/// 快捷键与菜单就一起失灵,而缩放档位一高、版式一乱,用户最需要的恰恰是 ⌘0 能回来
/// (2026-10-08 实测踩到)。代价是菜单项到头不置灰(gpui 判可用性时全局监听器恒算
/// 可用),到头时就是什么也不做
pub fn register(cx: &mut App) {
    cx.on_action(|_: &ZoomIn, cx| {
        if let Some(level) = step(percent(), true) {
            change(level, cx)
        }
    });
    cx.on_action(|_: &ZoomOut, cx| {
        if let Some(level) = step(percent(), false) {
            change(level, cx)
        }
    });
    cx.on_action(|_: &ActualSize, cx| change(DEFAULT, cx));
}

/// main.rs 键位表里的三个缩放键(全局,不限 key context)
pub fn key_bindings() -> impl Iterator<Item = KeyBinding> {
    ZOOM_IN_KEYS
        .into_iter()
        .map(|key| KeyBinding::new(key, ZoomIn, None))
        .chain([
            KeyBinding::new(ZOOM_OUT_KEY, ZoomOut, None),
            KeyBinding::new(ACTUAL_SIZE_KEY, ActualSize, None),
        ])
}

/// 三个快捷键的显示形制("⌘+ / ⌘- / ⌘0"、"Ctrl++ / …")。设置页的说明用它:Linux /
/// Windows 没有菜单栏,那里是唯一能看到快捷键的地方
pub fn key_hints() -> &'static str {
    use std::sync::OnceLock;
    static HINTS: OnceLock<String> = OnceLock::new();
    HINTS.get_or_init(|| {
        [ZOOM_IN_KEYS[0], ZOOM_OUT_KEY, ACTUAL_SIZE_KEY]
            .map(crate::ui::key_hint)
            .join(" / ")
    })
}

/// 换过档没有:`seen` 是调用方上次处理时的档位,变了就记下新档位并返回 true。缓存了按
/// 旧倍数量出的尺寸的地方(变高列表的行高、traffic light 坐标)在 render 里每帧问一次
pub fn changed(seen: &mut u16) -> bool {
    let now = percent();
    if *seen == now {
        return false;
    }
    *seen = now;
    true
}

/// macOS traffic light 的位置。`top` 是 100% 档的顶距(`MAIN_LIGHTS_TOP` /
/// `SETTINGS_LIGHTS_TOP`)。灯本身不随缩放变大,但界面是整体放大的:侧栏中轴与顶部
/// 净空都按倍数放大——把灯挪过去,红灯中心仍压在中轴上、仍在净空里垂直居中。
/// 100% 档恰好给回 (20, top)
pub fn traffic_lights(top: f32) -> Point<Pixels> {
    point(
        LEAD_AXIS.get() - px(LIGHT_HALF_W),
        zpx(top + LIGHT_HALF_H) - px(LIGHT_HALF_H),
    )
}

/// 换档后把已开窗口的 traffic light 挪到新位置。只有 macOS 有这个 API(gpui 在别的
/// 平台不编译它,`cfg!` 表达式也会被类型检查,所以必须是属性)
pub fn move_traffic_lights(top: f32, window: &mut Window) {
    #[cfg(target_os = "macos")]
    window.set_traffic_light_position(traffic_lights(top));
    #[cfg(not(target_os = "macos"))]
    let _ = (top, window);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_walk_the_level_table_and_stop_at_the_ends() {
        assert_eq!(step(100, true), Some(110));
        assert_eq!(step(125, false), Some(110));
        assert_eq!(step(150, true), None);
        assert_eq!(step(100, false), None);
        assert_eq!(step(123, true), None);
    }

    #[test]
    fn saved_levels_must_be_on_the_table() {
        assert_eq!(parse("125\n"), Some(125));
        assert_eq!(parse("175"), None);
        assert_eq!(parse("90"), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn traffic_lights_stay_where_they_were_at_100_percent() {
        assert_eq!(factor(), 1.);
        assert_eq!(traffic_lights(MAIN_LIGHTS_TOP), point(px(20.), px(15.)));
        assert_eq!(traffic_lights(SETTINGS_LIGHTS_TOP), point(px(20.), px(11.)));
    }

    #[test]
    fn a_seen_level_reports_each_change_once() {
        let mut seen = 0;
        assert!(changed(&mut seen));
        assert_eq!(seen, percent());
        assert!(!changed(&mut seen));
    }
}
