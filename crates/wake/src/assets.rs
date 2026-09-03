use anyhow::Result;
use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

/// 内嵌 lucide 图标(ISC License)。GPUI 无 SF Symbols,以 lucide 作为图标系统。
pub struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        fn lookup(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $name, ".svg") =>
                    Some(include_bytes!(concat!("../assets/icons/", $name, ".svg"))),)*
                _ => None,
            }
        }
        #[allow(dead_code)]
        pub const ICON_NAMES: &[&str] = &[$($name),*];
    };
}

/// Agent 品牌图标(彩色 PNG,640×640 带 alpha)。经 `img()` 渲染,
/// 不走 Icon——品牌色必须原样保留,不能被 text_color 着色。
/// 文件名即 `AgentId::as_str()`,见 `AgentId::brand_icon()`。
macro_rules! brands {
    ($($name:literal),* $(,)?) => {
        fn lookup_brand(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("brands/", $name, ".png") =>
                    Some(include_bytes!(concat!("../assets/brands/", $name, ".png"))),)*
                _ => None,
            }
        }
    };
}

brands!(
    "claude-code",
    "codex",
    "qoder",
    "qoder-light",
    "copilot",
    "copilot-light",
    "cursor",
    "cursor-light",
    "opencode",
    "opencode-light",
    "kiro",
    "gemini",
    "pi",
    "pi-light",
    "omp",
    "grok",
    "grok-light",
    "kimi",
    "kimi-light",
    "antigravity",
    "deepseek",
    "hermes",
    "hermes-light",
    "openclaw",
);

fn lookup_product(path: &str) -> Option<&'static [u8]> {
    match path {
        "brands/wake.svg" => Some(include_bytes!("../assets/icon.svg")),
        _ => None,
    }
}

icons!(
    // gpui-component TitleBar 的 Linux 窗口控制按钮(IconName::Window* 按这
    // 四个路径取图;缺了按钮就渲染成隐形热区——2026-08-24 Codex review)
    "window-minimize",
    "window-maximize",
    "window-restore",
    "window-close",
    "star",
    "pin",
    "download",
    "folder",
    "calendar",
    "trash-2",
    "search",
    "arrow-up-down",
    "check",
    "copy",
    "pin-filled",
    "star-filled",
    "terminal",
    "message-square",
    "layers",
    "close",
    "git-branch",
    "refresh-cw",
    "inbox",
    "chevron-down",
    "chevron-left",
    "chevron-right",
    "loader-circle",
    "loader",
    "circle-x",
    "file-text",
    "more-horizontal",
    "hard-drive",
    "server",
    "database",
    "settings",
    "plus",
    "info",
    "chart-column",
);

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(lookup(path)
            .or_else(|| lookup_brand(path))
            .or_else(|| lookup_product(path))
            .map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
