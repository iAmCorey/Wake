//! 主窗口:句柄登记,以及几何的记忆与恢复(哪块屏、屏内位置、全屏/最大化)。
//!
//! 坐标约定(2026-09-02 双屏反馈定稿):macOS 上 gpui 的 `window.bounds()` 与开窗
//! 的 `window_bounds` 都是**所在屏幕内的相对坐标**,屏幕由 `WindowOptions.display_id`
//! 选定、留空落主屏;`PlatformDisplay::bounds()`/`visible_bounds()` 同样以屏幕
//! 自身为原点。Windows/X11 上这几个 API 给的则是虚拟桌面的**全局坐标**,显示器
//! 原点非零。所以落盘的是(屏幕 uuid,减去该屏原点后的相对 bounds):macOS 上
//! 减零等于无操作,Windows 上显示器换了主屏或挪了位置时 uuid 仍命中、相对位置
//! 照样恢复(Codex review)。开任何窗都要把目标屏的 display_id 一起传——只算
//! 坐标不传屏幕,就是"设置窗跑到主屏"那个 bug。
//!
//! 两个平台差异(2026-09-02 Codex review):①落盘取 `inner_window_bounds()` 而非
//! `window_bounds()`——Wayland/X11 客户端装饰下后者含 gpui-component 加的 20px 阴影
//! inset,原样开窗会被 `set_client_inset` 再加一层,每次重启长 40px(Zed #22301 同病);
//! macOS/Windows 两者相同。②macOS 后端从不报 `WindowBounds::Maximized`,Zoom 后仍是
//! `Windowed` + 铺满可见区的 frame,要用 `is_maximized()` 补判,并且最大化/全屏时
//! 保住上一次 windowed 的 bounds 落盘,恢复时先按它开窗再放大/全屏(gpui 开窗后自动
//! 调 zoom/toggle_fullscreen),退出时才回得到原尺寸。
use crate::prefs;
use gpui::*;
use gpui_component::Root;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 首次启动的主窗尺寸
const MAIN_SIZE: Size<Pixels> = size(px(1180.), px(760.));

/// 主窗句柄。无窗时的菜单兜底与 Window → Main Window 据此找主窗,从属窗口
/// 据此取主窗几何
pub struct MainWindow(pub WindowHandle<Root>);
impl Global for MainWindow {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
enum Mode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SavedWindow {
    /// 屏内相对坐标;最大化/全屏时存的是进入前的 windowed bounds,恢复时先按它
    /// 开窗再放大/切全屏,退出也回到原位
    bounds: Bounds<Pixels>,
    #[serde(default)]
    mode: Mode,
    /// `PlatformDisplay::uuid()`,跨重启稳定;None = 拿不到,恢复时落主屏
    #[serde(default)]
    display: Option<String>,
}

/// 进程内快照与落盘状态。关窗后 Dock/菜单重开、退出时落盘都读快照,不依赖
/// 节流写盘是否已经落地
#[derive(Default)]
struct MainGeometry {
    /// 已提交的快照;`bounds` 是 windowed 尺寸,最大化/全屏期间保持不变
    saved: SavedWindow,
    /// windowed 模式下最近一次事件的 frame,等节流任务到期、模式仍是 windowed 才提交成
    /// `saved.bounds`。Zoom/全屏的入场动画每帧都按 windowed 报上来(尺寸还没铺满,
    /// `is_maximized()` 尚为假),直接提交会把"差一点铺满"的中间帧当成恢复尺寸;
    /// 延迟到到期时它已被最终的 Maximized/Fullscreen 盖掉(Codex review)
    live: Option<Bounds<Pixels>>,
    /// 节流落盘任务:在队 ⇔ 快照有未落盘的变化;在队就不再起新的,触发时读最新快照
    save_task: Option<Task<()>>,
}
impl Global for MainGeometry {}

impl MainGeometry {
    /// 待提交的 windowed frame 并进快照后的样子
    fn snapshot(&self) -> SavedWindow {
        let mut saved = self.saved.clone();
        if saved.mode == Mode::Windowed {
            if let Some(live) = self.live {
                saved.bounds = live;
            }
        }
        saved
    }

    fn commit(&mut self) {
        self.saved = self.snapshot();
        self.live = None;
    }
}

fn split(window_bounds: WindowBounds) -> (Bounds<Pixels>, Mode) {
    match window_bounds {
        WindowBounds::Windowed(b) => (b, Mode::Windowed),
        WindowBounds::Maximized(b) => (b, Mode::Maximized),
        WindowBounds::Fullscreen(b) => (b, Mode::Fullscreen),
    }
}

/// 挂到主窗上:移动/缩放/全屏都更新快照并节流落盘。观察器按窗口登记,只挂主窗
pub fn track(window: &mut Window, cx: &mut Context<Root>) {
    // 开窗即记一次:启动后不挪窗直接 Zoom、或以 Maximized 启动由 gpui 自动 zoom 时,
    // 最终那次回调才有"进 Zoom 前的尺寸"可保
    observe(window, cx);
    cx.observe_window_bounds(window, |_, window, cx| observe(window, cx))
        .detach();
}

fn observe(window: &Window, cx: &mut App) {
    // inner_window_bounds / is_maximized 的取舍见模块头
    let (platform_bounds, platform_mode) = split(window.inner_window_bounds());
    let mode = match platform_mode {
        Mode::Windowed if window.is_maximized() => Mode::Maximized,
        mode => mode,
    };
    let display = window.display(cx);
    let uuid = display
        .as_ref()
        .and_then(|d| d.uuid().ok())
        .map(|u| u.to_string());
    // 落盘的是相对所在屏幕原点的坐标(见模块头)
    let origin = display.map(|d| d.bounds().origin).unwrap_or_default();
    let platform_bounds = platform_bounds - origin;
    if !cx.has_global::<MainGeometry>() {
        // 首次(开窗时 track 主动调的那次):直接成为快照,不排落盘——磁盘上就是刚恢复
        // 出来的那份
        cx.set_global(MainGeometry {
            saved: SavedWindow {
                bounds: platform_bounds,
                mode,
                display: uuid,
            },
            ..Default::default()
        });
        return;
    }
    let geometry = cx.global_mut::<MainGeometry>();
    // 激活、换屏通知也会触发观察器,拖动时更是每帧一次:几何没变就什么都不做。
    // 最大化/全屏时各平台给的 bounds 不一(macOS Zoom 给的是铺满后的 frame,X11 给的
    // 是当前 bounds),一律不看,只认模式与屏幕
    let unchanged = mode == geometry.saved.mode
        && uuid == geometry.saved.display
        && match mode {
            Mode::Windowed => geometry
                .live
                .map_or(geometry.saved.bounds == platform_bounds, |live| {
                    live == platform_bounds
                }),
            _ => true,
        };
    if unchanged {
        return;
    }
    geometry.saved.mode = mode;
    geometry.saved.display = uuid;
    if mode == Mode::Windowed {
        geometry.live = Some(platform_bounds);
    }
    schedule_save(cx);
}

/// 一次移动只排一个任务:500ms 后写当时最新的快照,期间的事件只改快照
fn schedule_save(cx: &mut App) {
    if cx.global::<MainGeometry>().save_task.is_some() {
        return;
    }
    let task = cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        cx.update(flush);
    });
    cx.global_mut::<MainGeometry>().save_task = Some(task);
}

/// 有排队中的落盘任务才写(排队 ⇔ 快照有未落盘的变化):先提交待定的 windowed frame,
/// 写完即出队。节流任务与退出前(on_app_quit)都走这里
pub fn flush(cx: &mut App) {
    if !cx.has_global::<MainGeometry>() {
        return;
    }
    let geometry = cx.global_mut::<MainGeometry>();
    if geometry.save_task.take().is_none() {
        return;
    }
    geometry.commit();
    if let Err(e) = save(&geometry.saved) {
        eprintln!("failed to save window geometry: {e}");
    }
}

fn load() -> Option<SavedWindow> {
    let text = std::fs::read_to_string(prefs::path("window.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(saved: &SavedWindow) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?;
    prefs::write(&prefs::path("window.json"), &bytes)
}

/// 把窗口夹进屏幕范围:尺寸先按屏幕封顶,再把原点限制在屏内
fn fit_in(mut bounds: Bounds<Pixels>, screen: Bounds<Pixels>) -> Bounds<Pixels> {
    bounds.size = bounds.size.min(&screen.size);
    let max_origin = point(
        screen.origin.x + screen.size.width - bounds.size.width,
        screen.origin.y + screen.size.height - bounds.size.height,
    );
    bounds.origin = bounds.origin.clamp(&screen.origin, &max_origin);
    bounds
}

/// 开主窗的参数:进程内快照(本次会话刚关掉的位置)> 上次落盘 > 主屏居中。
/// 返回的 bounds 是所选屏幕内的相对坐标,必须与 display_id 一起交给 open_window。
/// 按 uuid 找不回屏幕(显示器拔了)退回主屏;一律夹进可见区,换过分辨率也不会
/// 开到屏外或压住 Dock
pub fn restore(cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let fallback = || (WindowBounds::centered(MAIN_SIZE, cx), None);
    let saved = cx.try_global::<MainGeometry>().map(MainGeometry::snapshot);
    let Some(saved) = saved.or_else(load) else {
        return fallback();
    };
    let display = saved
        .display
        .as_deref()
        .and_then(|uuid| {
            cx.displays()
                .into_iter()
                .find(|d| d.uuid().is_ok_and(|u| u.to_string() == uuid))
        })
        .or_else(|| cx.primary_display());
    let Some(display) = display else {
        return fallback();
    };
    // 相对坐标加回当前这块屏的原点,再夹进它的可见区
    let bounds = fit_in(
        saved.bounds + display.bounds().origin,
        display.visible_bounds(),
    );
    let window_bounds = match saved.mode {
        Mode::Windowed => WindowBounds::Windowed(bounds),
        Mode::Maximized => WindowBounds::Maximized(bounds),
        Mode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };
    (window_bounds, Some(display.id()))
}

/// 从属窗口(Settings)的开窗参数:开在主窗所在屏幕、居中压在主窗上、夹进
/// 可见区。主窗不在(理论上只有开窗失败)退回主屏居中。要在没有窗口被借出时
/// 调用(defer 里)
pub fn centered_over_main(size: Size<Pixels>, cx: &mut App) -> (Bounds<Pixels>, Option<DisplayId>) {
    let anchor = cx
        .try_global::<MainWindow>()
        .map(|main| main.0)
        .and_then(|handle| {
            handle
                .update(cx, |_, window, cx| (window.bounds(), window.display(cx)))
                .ok()
        });
    match anchor {
        Some((main, display)) => {
            let mut bounds = Bounds::centered_at(main.center(), size);
            if let Some(display) = &display {
                bounds = fit_in(bounds, display.visible_bounds());
            }
            (bounds, display.map(|d| d.id()))
        }
        None => (Bounds::centered(None, size, cx), None),
    }
}
