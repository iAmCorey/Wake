//! `wake://` 链接(#82)。Wake 写出去的引用(MCP 与 wake-cli 输出的 `wake://session/<key>#<seq>`、
//! `wake://memory/<key>`)与 GUI 打开的深链是同一套格式:写法(`session_ref` / `memory_ref`)与
//! 读法(`WakeRef`)放在一起,两边一起改。
//!
//! 另有 Windows / Linux 用的收件箱:系统把链接作为命令行参数交给新起的 Wake 进程,已经有一个
//! Wake 在跑时,新进程把链接写进收件箱目录就退出,运行中的那个监听着这个目录。macOS 由系统
//! 直接交给运行中的 app,用不着它

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::adapters::parse_utils::percent_decode;
use crate::db::now_ms;

pub fn session_ref(key: &str, seq: i64) -> String {
    format!("wake://session/{}#{seq}", escape_key(key))
}

pub fn memory_ref(key: &str) -> String {
    format!("wake://memory/{}", escape_key(key))
}

/// 引用里的 key 只编码三样:`%`(读的时候会解码,真实路径里的 `a%20b` 不编码就读回成 `a b`)、
/// 空格与控制字符(终端与 Markdown 在那里把链接截断)。别的字符原样:引用也是给人和模型读的,
/// 中文路径编成 `%E4…` 就没法看了;读法原样的与编码过的都认
fn escape_key(key: &str) -> Cow<'_, str> {
    let escaped = |c: char| c == '%' || c == ' ' || c.is_ascii_control();
    if !key.contains(escaped) {
        return Cow::Borrowed(key);
    }
    let mut out = String::with_capacity(key.len() + 8);
    for c in key.chars() {
        if escaped(c) {
            let _ = write!(out, "%{:02X}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 一条会话(可带消息序号)或一份记忆文件的引用。记忆 key 自身可含 `#`(SQLite 型虚拟路径
/// `<db>#<id>`),所以先认 scheme 再拆 seq
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeRef {
    Session { key: String, seq: Option<i64> },
    Memory { key: String },
}

/// 深链最长多少字节:正常的 key 不过几百字节,网页能拼出任意长的链接
const MAX_LINK_BYTES: usize = 4096;

impl WakeRef {
    /// `wake_get_session` 的 `key` 参数认的三种形态:裸会话 key、`wake://session/<key>#<seq>`
    /// (seq 成为默认起点)、`wake://memory/<key>`(记忆文件,另一条读法)。两种引用与 GUI 的深链
    /// 同一个读法(`reference`);裸 key 原样,不解码
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim();
        Self::reference(s).unwrap_or_else(|| {
            let (key, seq) = split_seq(s);
            Self::Session {
                key: key.to_string(),
                seq,
            }
        })
    }

    /// GUI 打开的深链:只认两种引用,裸 key、别的 scheme、空 key 都不算
    pub fn from_link(url: &str) -> Option<Self> {
        let url = url.trim();
        if url.len() > MAX_LINK_BYTES {
            return None;
        }
        let link = Self::reference(url)?;
        let (Self::Session { key, .. } | Self::Memory { key }) = &link;
        (!key.trim().is_empty()).then_some(link)
    }

    /// `wake://session/…` 与 `wake://memory/…`(scheme 不分大小写)。拆完结构再百分号解码:写引用
    /// 时编码过 `%` 与空白(`escape_key`),别的工具与浏览器还会编码别的字符(`:` → `%3A`);会话
    /// key 里编码过的 `%23` 不能被当成消息序号拆掉
    fn reference(s: &str) -> Option<Self> {
        let rest = s
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("wake://"))
            .map(|_| &s[7..])?;
        Some(match rest.split_once('/')? {
            ("session", rest) => {
                let (key, seq) = split_seq(rest);
                Self::Session {
                    key: percent_decode(key),
                    seq,
                }
            }
            ("memory", key) => Self::Memory {
                key: percent_decode(key.trim()),
            },
            _ => return None,
        })
    }
}

/// 会话引用末尾的 `#<seq>`:`#` 后面是数字才算序号,否则整串都是 key
fn split_seq(s: &str) -> (&str, Option<i64>) {
    match s.rsplit_once('#') {
        Some((key, seq)) => match seq.parse() {
            Ok(seq) => (key, Some(seq)),
            Err(_) => (s, None),
        },
        None => (s, None),
    }
}

/// 命令行参数里的 wake:// 链接(Windows / Linux 由系统这样交给 Wake;别的参数不管)
pub fn links_in_args(args: impl IntoIterator<Item = OsString>) -> Vec<String> {
    args.into_iter()
        .filter_map(|arg| arg.into_string().ok())
        .filter(|arg| WakeRef::from_link(arg).is_some())
        .collect()
}

/// 收件箱放在索引库旁边(`<库目录>/links`):同一个库只会有一个 Wake 在用,转给它的链接
/// 也就跟着库走
pub fn inbox_dir(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join("links")
}

/// 链接在收件箱里放多久还算数(毫秒):转发那一刻运行中的 Wake 恰好退出了,下次启动不该
/// 冒出一条旧链接
const INBOX_MAX_AGE_MS: i64 = 60_000;

/// 同一毫秒里放进去的几条链接靠它排先后
static POSTED: AtomicU32 = AtomicU32::new(0);

/// 把一条链接放进收件箱:先写临时文件再改名,读的一方看不到写了一半的。文件名以写下的
/// 毫秒数开头(补零到同宽),按名字排就是先后
fn post(inbox: &Path, url: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(inbox)?;
    let n = POSTED.fetch_add(1, Ordering::Relaxed);
    let name = format!("{:015}-{}-{n:04}", now_ms(), std::process::id());
    let tmp = inbox.join(format!("{name}.tmp"));
    std::fs::write(&tmp, url)?;
    std::fs::rename(&tmp, inbox.join(format!("{name}.url")))
}

/// 取走收件箱里的链接(读完即删),按放进来的先后;放了太久的丢掉、不交出去。两处同时取
/// (启动时先取一遍、监听线程又取)也只交一次:删成功的那一方才算拿到
pub fn take(inbox: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".url").then(|| (name, entry.path()))
        })
        .collect();
    files.sort();
    let now = now_ms();
    files
        .into_iter()
        .filter_map(|(name, path)| {
            let url = std::fs::read_to_string(&path).ok();
            std::fs::remove_file(&path).ok()?;
            let posted: i64 = name.split('-').next()?.parse().ok()?;
            (now - posted <= INBOX_MAX_AGE_MS).then_some(url).flatten()
        })
        .collect()
}

/// 收件箱里放进了新链接就调 `on_change`(调用方随后 `take`)。监听活到进程结束,句柄不交出去。
/// 只认 `.url` 的新建与改名过来的事件:Linux 的 notify 连"目录被打开"也报,`take` 读目录
/// 本身就会触发,全收的话监听与读取互相触发、一个核一直转
pub fn watch(inbox: &Path, on_change: impl Fn() + Send + 'static) -> anyhow::Result<()> {
    use notify::event::{EventKind, ModifyKind, RenameMode};
    use notify::Watcher;
    std::fs::create_dir_all(inbox)?;
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        let arrived = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Name(
                    RenameMode::To | RenameMode::Both | RenameMode::Any
                ))
        ) && event
            .paths
            .iter()
            .any(|path| path.extension().is_some_and(|ext| ext == "url"));
        if arrived || event.need_rescan() {
            on_change();
        }
    })?;
    watcher.watch(inbox, notify::RecursiveMode::NonRecursive)?;
    std::mem::forget(watcher);
    Ok(())
}

/// 已经有一个 Wake 窗口用着 `db` 这个库(`IndexLock::app_running`):把 `links` 放进它的
/// 收件箱,返回 true。Windows 上先把前台权让出去——链接是用户点开的,前台权在这个进程手里,
/// 让给那个 Wake 它才能把窗口拉到最前
pub fn forward_to_running(db: &Path, links: &[String]) -> bool {
    if links.is_empty() || !crate::db::IndexLock::app_running(db) {
        return false;
    }
    #[cfg(windows)]
    windows::allow_foreground();
    let inbox = inbox_dir(db);
    links.iter().all(|url| post(&inbox, url).is_ok())
}

#[cfg(windows)]
pub use windows::register_scheme;

#[cfg(windows)]
mod windows {
    use crate::services::terminal::wide;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, RegSetKeyValueW, RegSetValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    pub(super) fn allow_foreground() {
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
        }
    }

    /// Windows 没有安装器(zip 解开即用):每次启动把当前用户的 `wake:` 协议指向这个 Wake
    /// (HKCU\Software\Classes\wake,不要管理员权限)。已经指向它就不写;放在后台线程里跑
    pub fn register_scheme() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let command = format!("\"{}\" \"%1\"", exe.display());
        if let Err(status) = register(r"Software\Classes\wake", &command) {
            eprintln!("wake: can't register the wake: link handler (error {status})");
        }
    }

    /// HKCU 下 `subkey` 的默认值(没有、读不出给 None)
    fn default_value(subkey: &str) -> Option<String> {
        let subkey = wide(subkey);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != 0 {
            return None;
        }
        let len = (bytes as usize / 2).saturating_sub(1).min(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// 把 HKCU 下的 `root` 登记成 URL 协议、由 `command` 打开:`root` 的默认值 "URL:Wake" 与
    /// 空的 "URL Protocol",`root\shell\open\command` 的默认值是命令行。已经是这样就不写
    fn register(root: &str, command: &str) -> Result<(), u32> {
        let open = format!(r"{root}\shell\open\command");
        if default_value(&open).as_deref() == Some(command) {
            return Ok(());
        }
        set_default(root, "URL:Wake")?;
        set_named(root, "URL Protocol", "")?;
        set_default(&open, command)
    }

    /// RegSetValueW:键(连同中间缺的键)不存在就建,写它的默认值
    fn set_default(subkey: &str, value: &str) -> Result<(), u32> {
        let (subkey, data) = (wide(subkey), wide(value));
        let status = unsafe {
            RegSetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                REG_SZ,
                data.as_ptr(),
                (data.len() * 2) as u32,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(status)
        }
    }

    /// 已有的键上写一个具名的字符串值
    fn set_named(subkey: &str, name: &str, value: &str) -> Result<(), u32> {
        let (subkey, name, data) = (wide(subkey), wide(name), wide(value));
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(status)
        }
    }

    #[cfg(test)]
    mod tests {
        use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};

        /// 登记写在测试自己的键下,读回来核对,再删掉
        #[test]
        fn registers_the_link_handler_and_reads_it_back() {
            let root = format!(r"Software\WakeTest\links-{}", std::process::id());
            let command = r#""C:\Program Files\Wake\Wake.exe" "%1""#;
            super::register(&root, command).unwrap();
            assert_eq!(
                super::default_value(&format!(r"{root}\shell\open\command")).as_deref(),
                Some(command)
            );
            assert_eq!(super::default_value(&root).as_deref(), Some("URL:Wake"));
            // 已经指向它:不再写,照样成功
            super::register(&root, command).unwrap();
            let base = super::wide(r"Software\WakeTest");
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, base.as_ptr()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(key: &str, seq: Option<i64>) -> WakeRef {
        WakeRef::Session {
            key: key.to_string(),
            seq,
        }
    }

    #[test]
    fn key_arg_accepts_wake_refs() {
        assert_eq!(
            WakeRef::parse(&session_ref("claude-code:abc", 12)),
            session("claude-code:abc", Some(12))
        );
        assert_eq!(
            WakeRef::parse("codex:devbox:0195-xyz"),
            session("codex:devbox:0195-xyz", None)
        );
        assert_eq!(
            WakeRef::parse(" claude-code:abc "),
            session("claude-code:abc", None)
        );
        // 裸 key 原样,不解码
        assert_eq!(
            WakeRef::parse("claude-code:a%20b"),
            session("claude-code:a%20b", None)
        );
        // 记忆 key 自身带 `#`(SQLite 型虚拟路径)也不能被当成 seq 拆掉
        let memory_key = "codex:/home/me/.codex/memories_1.sqlite#t-0001";
        assert_eq!(
            WakeRef::parse(&memory_ref(memory_key)),
            WakeRef::Memory {
                key: memory_key.to_string()
            }
        );
    }

    /// Wake 写出去的引用就是能打开的链接;别的工具编码过的 key 解码后再认,编码过的 `#` 是 key
    /// 的一部分、不是消息序号
    #[test]
    fn links_open_wake_refs_and_reject_anything_else() {
        let memory_key = "codex:/home/me/.codex/memories_1.sqlite#t-0001";
        let memory = WakeRef::Memory {
            key: memory_key.to_string(),
        };
        for (url, expected) in [
            (
                session_ref("claude-code:abc", 12),
                session("claude-code:abc", Some(12)),
            ),
            (memory_ref(memory_key), memory),
            (
                "Wake://session/claude-code%3Aa%20b#3".into(),
                session("claude-code:a b", Some(3)),
            ),
            (
                "wake://session/craft:ws_1%2312".into(),
                session("craft:ws_1#12", None),
            ),
            (
                "wake://session/codex:devbox:0195-xyz".into(),
                session("codex:devbox:0195-xyz", None),
            ),
        ] {
            assert_eq!(WakeRef::from_link(&url), Some(expected), "{url}");
        }
        let long = format!("wake://session/claude-code:{}", "a".repeat(MAX_LINK_BYTES));
        for url in [
            "claude-code:abc",
            "https://example.com/session/claude-code:abc",
            "wake://settings/general",
            "wake://session/",
            "wake://memory/%20",
            "wake:session/claude-code:abc",
            &long,
        ] {
            assert_eq!(WakeRef::from_link(url), None, "{url}");
        }
    }

    /// 写出去的引用读回来还是同一个 key,MCP 的读法与深链一样:真实路径里的 `%`、空格、中文、
    /// Windows 路径;别的工具把整个 key 再编码一遍也读得回来
    #[test]
    fn references_round_trip_their_keys() {
        for key in [
            "claude-code:/Users/me/notes%20archive/memory/MEMORY.md",
            "claude-code:/Users/me/My Projects/app/CLAUDE.md",
            "claude-code:devbox:/home/me/项目/CLAUDE.md",
            r"claude-code:C:\Users\John Smith\.claude\CLAUDE.md",
            "codex:/home/me/.codex/memories_1.sqlite#t-0001",
            "openclaw:a\tb",
        ] {
            let memory = memory_ref(key);
            assert!(!memory.contains([' ', '\t']), "{memory}");
            let expected = WakeRef::Memory {
                key: key.to_string(),
            };
            assert_eq!(WakeRef::parse(&memory), expected, "{memory}");
            assert_eq!(WakeRef::from_link(&memory), Some(expected), "{memory}");
            if !key.contains('#') {
                let link = session_ref(key, 7);
                assert_eq!(WakeRef::parse(&link), session(key, Some(7)), "{link}");
                assert_eq!(WakeRef::from_link(&link), Some(session(key, Some(7))));
            }
        }
        assert_eq!(
            WakeRef::from_link(
                "wake://memory/claude-code%3A%2FUsers%2Fme%2FMy%20Projects%2F%E9%A1%B9%E7%9B%AE%2FCLAUDE.md"
            ),
            Some(WakeRef::Memory {
                key: "claude-code:/Users/me/My Projects/项目/CLAUDE.md".to_string()
            })
        );
    }

    #[test]
    fn only_wake_links_are_taken_from_the_command_line() {
        let args = [
            "--verbose",
            "wake://session/claude-code:abc#2",
            "https://x.y",
        ]
        .map(OsString::from);
        assert_eq!(
            links_in_args(args),
            vec!["wake://session/claude-code:abc#2".to_string()]
        );
    }

    #[test]
    fn the_inbox_hands_each_link_over_once_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("links");
        post(&inbox, "wake://session/a:1").unwrap();
        post(&inbox, "wake://session/a:2").unwrap();
        // 没写完的临时文件、别的文件不算;放了太久的丢掉
        std::fs::write(
            inbox.join("000000000000001-1-0000.tmp"),
            "wake://session/a:x",
        )
        .unwrap();
        std::fs::write(inbox.join("notes.txt"), "wake://session/a:y").unwrap();
        std::fs::write(
            inbox.join("000000000000001-1-0001.url"),
            "wake://session/a:old",
        )
        .unwrap();
        assert_eq!(
            take(&inbox),
            vec![
                "wake://session/a:1".to_string(),
                "wake://session/a:2".to_string()
            ]
        );
        assert!(take(&inbox).is_empty());
        assert!(!inbox.join("000000000000001-1-0001.url").exists());
    }
}
