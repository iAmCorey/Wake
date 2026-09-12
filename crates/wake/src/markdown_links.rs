//! macOS 会话正文的文件链接。路径归属来自会话,不能从本机同名文件推断。
use std::path::{Path, PathBuf};

use gpui::{ClickEvent, MouseButton};
use gpui_component::{notification::Notification, text::TextView, WindowExt as _};
use percent_encoding::percent_decode_str;
use url::Url;

use crate::i18n::t;

pub(crate) fn with_session_links(view: TextView, host: &str, project_path: &str) -> TextView {
    // 本轮只接管 macOS;其余平台保留组件默认行为,仍参与编译检查。
    if !cfg!(target_os = "macos") {
        return view;
    }
    let remote = !host.is_empty();
    let project_path = project_path.to_owned();
    view.on_link_click(move |href, event, window, cx| {
        // 组件设置 handler 后会连右键一起转发,须保留默认激活规则。
        let activate = match event {
            ClickEvent::Mouse(click) => {
                matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
            }
            ClickEvent::Keyboard(_) => true,
            ClickEvent::Touch(click) => !click.long_press,
        };
        if !activate {
            return;
        }
        match resolve_link(href, remote, &project_path, dirs::home_dir) {
            LinkTarget::External => cx.open_url(href),
            LinkTarget::File(path) => {
                if let Some(path) = path.to_str() {
                    wake_core::services::terminal::open_in_file_manager(path);
                } else {
                    window.push_notification(
                        Notification::info(t("The linked path is unavailable on this Mac.")),
                        cx,
                    );
                }
            }
            LinkTarget::Remote => window.push_notification(
                Notification::info(t(
                    "Remote sessions are read-only — files stay on the remote host",
                )),
                cx,
            ),
            LinkTarget::Unavailable => window.push_notification(
                Notification::info(t("The linked path is unavailable on this Mac.")),
                cx,
            ),
            LinkTarget::Ignore => {}
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum LinkTarget {
    External,
    File(PathBuf),
    Remote,
    Unavailable,
    Ignore,
}

fn resolve_link(
    href: &str,
    remote: bool,
    project_path: &str,
    home_dir: impl FnOnce() -> Option<PathBuf>,
) -> LinkTarget {
    if href.is_empty() || href.starts_with('#') {
        return LinkTarget::Ignore;
    }
    // 盘符须先于 scheme 识别,否则 C:/... 会被交给 URL 打开器。
    let windows = windows_path(href);
    let scheme = (!windows)
        .then(|| url_scheme(href))
        .flatten()
        .filter(|scheme| {
            // A bare filename such as main.rs:11 also matches URL scheme syntax.
            // Prefer file semantics for dotted names followed only by a line number,
            // before the remote guard and without consulting the local filesystem.
            !(scheme.contains('.') && without_line_number(href) == Some(*scheme))
        });
    if scheme.is_some_and(|scheme| !scheme.eq_ignore_ascii_case("file")) {
        return LinkTarget::External;
    }
    // rsync 只镜像会话数据。远程正文路径不能展开本机 HOME,也不能探测本机。
    if remote {
        return LinkTarget::Remote;
    }
    if windows || href.chars().any(char::is_control) {
        return LinkTarget::Unavailable;
    }
    if scheme.is_some() {
        let Some(path) = file_uri_path(href) else {
            return LinkTarget::Unavailable;
        };
        if path.exists() {
            return LinkTarget::File(path);
        }
        return without_line_number(href)
            .and_then(file_uri_path)
            .filter(|path| path.is_file())
            .map_or(LinkTarget::Unavailable, LinkTarget::File);
    }

    let home = home_dir();
    if let Some(path) = existing_local_path(href, project_path, home.as_deref()) {
        return LinkTarget::File(path);
    }
    without_line_number(href)
        .and_then(|href| existing_local_path(href, project_path, home.as_deref()))
        .filter(|path| path.is_file())
        .map_or(LinkTarget::Unavailable, LinkTarget::File)
}

fn existing_local_path(href: &str, project_path: &str, home: Option<&Path>) -> Option<PathBuf> {
    // 裸路径先按原字面查找,保留真实文件名里的 %, #, ? 等字符。
    if let Some(path) = local_path(href, project_path, home).filter(|p| p.exists()) {
        return Some(path);
    }
    // Markdown href 也可能编码了空格/中文。只解码一次,不把失败的文件路径
    // 回退为 open_url,也不猜测缺失的 file URI authority。
    if href.contains('%') {
        if let Ok(decoded) = percent_decode_str(href).decode_utf8() {
            return local_path(&decoded, project_path, home).filter(|p| p.exists());
        }
    }
    None
}

fn without_line_number(href: &str) -> Option<&str> {
    // 完整文件名不存在时才尝试 :行号。先于百分号解码,避免把文件名里的
    // %3A 误当定位符;Finder 只选中文件,不负责跳转到该行。
    let (path, line) = href.rsplit_once(':')?;
    (!path.is_empty()
        && line.bytes().all(|byte| byte.is_ascii_digit())
        && line.parse::<usize>().is_ok_and(|line| line > 0))
    .then_some(path)
}

fn url_scheme(href: &str) -> Option<&str> {
    let (scheme, _) = href.split_once(':')?;
    let mut bytes = scheme.bytes();
    (bytes.next()?.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
    .then_some(scheme)
}

fn windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with("\\\\")
        || (bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes.len() == 2
                || bytes[2] == b'\\'
                || (bytes[2] == b'/' && bytes.get(3) != Some(&b'/'))))
}

fn file_uri_path(href: &str) -> Option<PathBuf> {
    // file:relative 和反斜杠纠正不在本轮范围,不让宽松 URL 解析猜成本机路径。
    if !href.split_once(':')?.1.starts_with('/') || href.contains('\\') {
        return None;
    }
    let url = Url::parse(href).ok()?;
    if url.host_str().is_some_and(|host| host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.to_file_path().ok()?;
    let text = path.to_str()?;
    if !path.is_absolute() || text.starts_with("//") || windows_path(text.trim_start_matches('/')) {
        return None;
    }
    Some(path)
}

fn local_path(href: &str, project_path: &str, home: Option<&Path>) -> Option<PathBuf> {
    if href.is_empty()
        || href.starts_with(['#', '?'])
        || href.starts_with("//")
        || windows_path(href)
        || href.chars().any(char::is_control)
    {
        return None;
    }
    let path = if href == "~" {
        home?.to_path_buf()
    } else if let Some(relative) = href.strip_prefix("~/") {
        home?.join(relative.trim_start_matches('/'))
    } else if href.starts_with('~') {
        return None;
    } else if Path::new(href).is_absolute() {
        PathBuf::from(href)
    } else {
        let project = Path::new(project_path);
        if !project.is_absolute() || !project.is_dir() {
            return None;
        }
        project.join(href)
    };
    path.is_absolute().then_some(path)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use gpui::AppContext as _;
    use std::fs;

    #[test]
    fn local_files_directories_and_encoded_links() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("OpenSurge for Mac");
        let file = project.join("internal/controlapi/mihomo_recovery.go");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "package controlapi\n").unwrap();
        let project_text = project.to_str().unwrap();
        let file_text = file.to_str().unwrap();
        let file_url = Url::from_file_path(&file).unwrap();
        for href in [
            file_text.to_owned(),
            file_text.replace(' ', "%20"),
            file_url.to_string(),
            format!("file:{}", file_url.path()),
            format!("FILE://localhost{}", file_url.path()),
            "internal/controlapi/mihomo_recovery.go".into(),
            "./internal/controlapi/mihomo_recovery.go".into(),
            "../OpenSurge for Mac/internal/controlapi/mihomo_recovery.go".into(),
            "~/OpenSurge for Mac/internal/controlapi/mihomo_recovery.go".into(),
        ] {
            let target = resolve_link(&href, false, project_text, || Some(temp.path().into()));
            let LinkTarget::File(path) = target else {
                panic!("{href:?}: {target:?}");
            };
            assert_eq!(path.canonicalize().unwrap(), file.canonicalize().unwrap());
        }
        assert_eq!(
            resolve_link(project_text, false, "", || None),
            LinkTarget::File(project)
        );
    }

    #[test]
    fn line_number_links_resolve_to_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("Wake with spaces");
        fs::create_dir_all(project.join("src")).unwrap();
        for (name, line) in [("markdown_links.rs", 11), ("workbench.rs", 7955)] {
            let file = project.join("src").join(name);
            fs::write(&file, "").unwrap();
            for href in [
                format!("{}:{line}", file.display()),
                format!("{}:{line}", file.to_str().unwrap().replace(' ', "%20")),
                format!("{}:{line}", Url::from_file_path(&file).unwrap()),
                format!("src/{name}:{line}"),
                format!("./src/{name}:{line}"),
                format!("~/Wake with spaces/src/{name}:{line}"),
            ] {
                let target = resolve_link(&href, false, project.to_str().unwrap(), || {
                    Some(temp.path().into())
                });
                let LinkTarget::File(path) = target else {
                    panic!("{href:?}: {target:?}");
                };
                assert_eq!(path.canonicalize().unwrap(), file.canonicalize().unwrap());
            }
        }
    }

    #[test]
    fn bare_filename_line_links_use_the_session_project() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().to_str().unwrap();
        let file = temp.path().join("main.rs");
        fs::write(&file, "").unwrap();
        assert_eq!(
            resolve_link("main.rs:11", false, project, || None),
            LinkTarget::File(file)
        );
        // A literal filename containing the suffix still takes precedence.
        let literal = temp.path().join("main.rs:11");
        fs::write(&literal, "").unwrap();
        assert_eq!(
            resolve_link("main.rs:11", false, project, || None),
            LinkTarget::File(literal)
        );
        for (href, project) in [("missing.rs:11", project), ("main.rs:11", "")] {
            assert_eq!(
                resolve_link(href, false, project, || None),
                LinkTarget::Unavailable
            );
        }
    }

    #[test]
    fn line_numbers_preserve_literal_names_and_require_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("source file.rs");
        let literal = temp.path().join("source file.rs:11");
        // 即使去掉行号后两种拼法都存在,完整的解码文件名也优先。
        for path in [&file, &literal, &temp.path().join("source%20file.rs")] {
            fs::write(path, "").unwrap();
        }
        for href in [
            literal.to_str().unwrap().to_owned(),
            literal.to_str().unwrap().replace(' ', "%20"),
            Url::from_file_path(&literal).unwrap().to_string(),
        ] {
            assert_eq!(
                resolve_link(&href, false, "", || None),
                LinkTarget::File(literal.clone())
            );
        }
        for suffix in [":0", ":+11", ":-11", ":line11", "%3A12", ":11#fragment"] {
            for base in [
                file.to_str().unwrap().to_owned(),
                Url::from_file_path(&file).unwrap().to_string(),
            ] {
                let href = format!("{base}{suffix}");
                assert_eq!(
                    resolve_link(&href, false, "", || None),
                    LinkTarget::Unavailable,
                    "{href}"
                );
            }
        }
        assert_eq!(
            resolve_link(&format!("{}:11", temp.path().display()), false, "", || None),
            LinkTarget::Unavailable
        );
    }

    #[test]
    fn filenames_are_not_reinterpreted_or_decoded_twice() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "中文 #1%.md",
            "literal%20space.md",
            "literal space.md",
            "name:42",
            "name#L42",
        ] {
            let path = temp.path().join(name);
            fs::write(&path, "").unwrap();
            for href in [
                path.to_str().unwrap().to_owned(),
                Url::from_file_path(&path).unwrap().to_string(),
            ] {
                assert_eq!(
                    resolve_link(&href, false, "", || None),
                    LinkTarget::File(path.clone())
                );
            }
        }
        fs::write(temp.path().join("once decoded.md"), "").unwrap();
        assert_eq!(
            resolve_link(
                "once%2520decoded.md",
                false,
                temp.path().to_str().unwrap(),
                || None
            ),
            LinkTarget::Unavailable
        );
    }

    #[test]
    fn remote_paths_never_resolve_against_this_mac() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("exists.md");
        fs::write(&file, "").unwrap();
        let with_line = format!("{}:11", file.display());
        for href in [
            file.to_str().unwrap(),
            &with_line,
            "~/exists.md",
            "~/exists.md:11",
            "exists.md",
            "exists.md:11",
            "./exists.md:11",
            "file:///tmp/exists.md",
            "file:///tmp/exists.md:11",
            "C:/work/a.rs",
        ] {
            assert_eq!(
                resolve_link(href, true, temp.path().to_str().unwrap(), || panic!(
                    "remote path requested local HOME"
                )),
                LinkTarget::Remote
            );
        }
    }

    #[test]
    fn invalid_or_unavailable_paths_never_become_external_urls() {
        let temp = tempfile::tempdir().unwrap();
        for href in [
            "/wake-link-test-missing/a.rs",
            "/wake-link-test-missing/a.rs:11",
            "missing.rs",
            "file:relative.rs",
            "file://Users/example/a.rs",
            "file://server/share/a.rs",
            "file://server/share/a.rs:11",
            "file:///C:/work/a.rs",
            "C:/work/a.rs",
            r"C:\work\a.rs",
            r"\\server\share\a.rs",
            "//server/share/a.rs",
            "file:///tmp/a.rs?x=1",
            "file:///tmp/a.rs#L42",
            "file:///tmp/a%00.rs",
            "~someone/a.rs",
        ] {
            assert_eq!(
                resolve_link(href, false, temp.path().to_str().unwrap(), || None),
                LinkTarget::Unavailable,
                "{href}"
            );
        }
        // 即使当前工作目录中确有 Cargo.toml,也不能在会话基准缺席时使用 cwd。
        assert_eq!(
            resolve_link("Cargo.toml", false, "", || None),
            LinkTarget::Unavailable
        );
        assert_eq!(
            resolve_link("Cargo.toml", false, ".", || None),
            LinkTarget::Unavailable
        );
    }

    #[test]
    fn external_schemes_and_anchors_keep_their_own_meaning() {
        for remote in [false, true] {
            for href in [
                "https://example.com/a%20b?q=1#x",
                "http://example.com",
                "mailto:a@example.com",
                "vscode://file/tmp/a.rs",
                "vscode://file/tmp/a.rs:11",
                "https://example.com/a.rs:11",
                "custom+app:value",
                "custom+app:11",
                "org.example://open/main.rs:11",
                "x://example.com",
            ] {
                assert_eq!(
                    resolve_link(href, remote, "", || panic!("URL requested local HOME")),
                    LinkTarget::External
                );
            }
            for href in ["", "#section"] {
                assert_eq!(resolve_link(href, remote, "", || None), LinkTarget::Ignore);
            }
        }
    }

    struct LinkView {
        href: String,
        host: &'static str,
    }

    impl gpui::Render for LinkView {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div().w(gpui::px(360.)).child(with_session_links(
                TextView::markdown("test-link", format!("[example link](<{}>)", self.href))
                    .selectable(true),
                self.host,
                "",
            ))
        }
    }

    #[gpui::test]
    fn missing_file_click_is_handled_in_wake(cx: &mut gpui::TestAppContext) {
        use gpui::{point, px, Modifiers};
        use gpui_component::Root;
        cx.update(gpui_component::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| LinkView {
                href: "/wake-link-test-missing/OpenSurge for Mac/a.go".into(),
                host: "",
            });
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let notifications = root.read_with(cx, |root, _| root.notification.clone());
        cx.simulate_mouse_down(
            point(px(15.), px(12.)),
            MouseButton::Right,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(15.), px(12.)),
            MouseButton::Right,
            Modifiers::default(),
        );
        assert!(notifications.read_with(cx, |list, _| list.notifications().is_empty()));
        assert_eq!(cx.opened_url(), None);
        cx.simulate_click(point(px(15.), px(12.)), Modifiers::default());
        assert_eq!(
            notifications.read_with(cx, |list, _| list.notifications().len()),
            1
        );
        assert_eq!(cx.opened_url(), None);
    }

    #[gpui::test]
    #[allow(deprecated)]
    fn external_links_preserve_selection_and_middle_click(cx: &mut gpui::TestAppContext) {
        use gpui::{point, px, Modifiers};
        use gpui_component::Root;
        let href = "https://example.com/a%20b?q=1#anchor";
        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| LinkView {
                href: href.into(),
                host: "remote-host",
            });
            Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_down(
            point(px(5.), px(12.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(65.), px(12.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(65.), px(12.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(cx.opened_url(), None);
        assert!(!cx.update(|window, cx| window.selected_text(cx)).is_empty());
        // 点空白处取消选区后,中键仍按组件原来的规则打开外链。
        cx.simulate_click(point(px(300.), px(100.)), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_down(
            point(px(15.), px(12.)),
            MouseButton::Middle,
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(15.), px(12.)),
            MouseButton::Middle,
            Modifiers::default(),
        );
        assert_eq!(cx.opened_url().as_deref(), Some(href));
    }
}
