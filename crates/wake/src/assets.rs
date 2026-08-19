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
    "copilot",
    "copilot-light",
    "cursor",
    "cursor-light",
    "opencode",
    "opencode-light",
    "kiro",
    "gemini",
    "grok-build",
    "grok-build-light",
);

icons!(
    "star",
    "pin",
    "download",
    "folder",
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
    "x",
    "git-branch",
    "refresh-cw",
    "inbox",
    "chevron-down",
    "chevron-right",
    "loader-circle",
    "loader",
    "circle-x",
    "file-text",
    "more-horizontal",
);

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(lookup(path).or_else(|| lookup_brand(path)).map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
