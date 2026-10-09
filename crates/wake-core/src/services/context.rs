//! agent 取上下文用的纯函数(wake-mcp 工具层调用;无 UI、无 IO)。
//! 项目参数的三级匹配与 `since` 解析都在这里,单测卡住语义——工具层只管拼装。

use crate::adapters::path_owns;
use crate::models::{local_ms, ProjectInfo};
use chrono::{NaiveDate, NaiveDateTime};

/// 把 agent 给的 project 参数解析成索引里的项目路径集合(交给
/// `SessionFilter::project_paths` / `SearchFilter::project_paths`)。
///
/// 绝对路径三级:①完全相等;②该路径是某项目路径的后代(agent 的 cwd 在
/// 子目录里)→ 取最长的那个祖先;③项目路径是该路径的后代(monorepo 根、或
/// `~/Github` 这类上层目录)→ 并集。边界判定用 `path_owns`,`wake-old` 不算
/// `wake` 的后代。非绝对路径按项目名匹配(大小写不敏感;`project_name` 就是
/// 各 adapter 经 `project_name_of` 取的路径尾段,不必再自己切一遍)。
/// 返回空 = 没匹配上,调用方据此报"未知项目",**不要退化成不过滤**。
pub fn resolve_project_paths(arg: &str, projects: &[ProjectInfo]) -> Vec<String> {
    let home = crate::adapters::home_dir().map(|h| h.to_string_lossy().into_owned());
    resolve_in(arg, projects, home.as_deref())
}

/// `resolve_project_paths` 的本体,家目录由调用方给(单测不碰环境)
fn resolve_in(arg: &str, projects: &[ProjectInfo], home: Option<&str>) -> Vec<String> {
    let arg = crate::adapters::expand_tilde(arg.trim());
    let wanted = strip_trailing_sep(&arg);
    if wanted.is_empty() {
        return Vec::new();
    }
    if looks_like_path(wanted) {
        if let Some(p) = projects
            .iter()
            .find(|p| strip_trailing_sep(&p.path) == wanted)
        {
            return vec![p.path.clone()];
        }
        // 最长的祖先,但文件系统根与家目录不算:从 Dock 起的进程 cwd 是 "/"、在家目录随手
        // 开的 agent 落在 "~",它们是一切路径的祖先——拿来当"这个目录所在的项目",在还没有
        // 会话的新项目里一问就是一堆不相干的会话(SessionStart 钩子注入的正是这个,2026-10-09)
        let mut best: Option<&ProjectInfo> = None;
        for p in projects
            .iter()
            .filter(|p| !p.path.is_empty() && !catch_all(&p.path, home))
        {
            if path_owns(&p.path, wanted) && best.is_none_or(|b| p.path.len() > b.path.len()) {
                best = Some(p);
            }
        }
        if let Some(b) = best {
            return vec![b.path.clone()];
        }
        let mut inside: Vec<String> = projects
            .iter()
            .filter(|p| !p.path.is_empty() && path_owns(wanted, &p.path))
            .map(|p| p.path.clone())
            .collect();
        inside.sort();
        inside.dedup();
        return inside;
    }
    let lower = wanted.to_lowercase();
    let mut matched: Vec<String> = projects
        .iter()
        .filter(|p| !p.path.is_empty() && p.name.to_lowercase() == lower)
        .map(|p| p.path.clone())
        .collect();
    matched.sort();
    matched.dedup();
    matched
}

/// 一切路径的祖先、当不了"所在项目"的目录:文件系统根(`/`、`C:\`)与家目录
fn catch_all(path: &str, home: Option<&str>) -> bool {
    let p = strip_trailing_sep(path);
    // 盘符根两种写法都认:Unix 上 `\` 不是分隔符,`C:\` 剥不掉尾巴(镜像来的 Windows 路径)
    let drive_root = after_drive(p).is_some_and(|rest| matches!(rest, "" | "\\" | "/"));
    let root = p.chars().all(std::path::is_separator) || drive_root;
    root || home.is_some_and(|h| strip_trailing_sep(h) == p)
}

/// 是"路径"而非"项目名"的判据。不能只看 `Path::is_absolute`:Windows 上它对
/// `/Users/…` 返回 false,而索引里的远程镜像会话(以及 CI 上的 fixture)全是
/// POSIX 路径;反过来 Unix 上也认 `C:\…` 形态,一个盘符串不可能是项目名
fn looks_like_path(s: &str) -> bool {
    let drive = after_drive(s).is_some_and(|rest| rest.starts_with(['\\', '/']));
    std::path::Path::new(s).is_absolute() || s.starts_with(std::path::is_separator) || drive
}

/// 盘符(`C:`)之后的部分;不以盘符开头给 None
fn after_drive(s: &str) -> Option<&str> {
    match s.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => Some(&s[2..]),
        _ => None,
    }
}

fn strip_trailing_sep(s: &str) -> &str {
    let trimmed = s.trim_end_matches(std::path::is_separator);
    // 文件系统根("/")剥光了会变空串,保留原样
    if trimmed.is_empty() && !s.is_empty() {
        s
    } else {
        trimmed
    }
}

/// `since` 参数 → epoch ms。接受:相对量 `30m` / `24h` / `7d` / `2w`(以
/// `now_ms` 为基准);RFC 3339(`2026-09-01T00:00:00Z`);无时区的日期时间
/// (`2026-09-01 09:30`、`2026-09-01T09:30:00`,按本地时区);裸日期
/// (`2026-09-01`,本地当天 00:00)。解析不了返回 None,调用方报参数错误
pub fn parse_since(arg: &str, now_ms: i64) -> Option<i64> {
    let s = arg.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(ms) = parse_relative(s) {
        return Some(now_ms - ms);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return local_ms(ndt);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_ms(d.and_hms_opt(0, 0, 0)?);
    }
    None
}

fn parse_relative(s: &str) -> Option<i64> {
    // 按最后一个**字符**切,不是最后一个字节:`7天` 的末字节落在多字节字符
    // 中间,split_at 会 panic 把整个 server 带走
    let unit = s.chars().last()?;
    let unit_ms = match unit {
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 7 * 86_400_000,
        _ => return None,
    };
    let num = &s[..s.len() - unit.len_utf8()];
    let n: i64 = num.parse().ok().filter(|n| *n >= 0)?;
    n.checked_mul(unit_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone as _};

    fn project(path: &str, name: &str) -> ProjectInfo {
        ProjectInfo {
            path: path.to_string(),
            name: name.to_string(),
            session_count: 1,
            last_active: 0,
        }
    }

    fn fixtures() -> Vec<ProjectInfo> {
        vec![
            project("/Users/t/Github/wake", "wake"),
            project("/Users/t/Github/wake/crates/wake-core", "wake-core"),
            project("/Users/t/Github/wake-old", "wake-old"),
            project("/Users/t/Work/api", "api"),
            project("", ""),
        ]
    }

    #[test]
    fn exact_match_wins_and_ignores_trailing_separator() {
        let p = fixtures();
        assert_eq!(
            resolve_project_paths("/Users/t/Github/wake/", &p),
            vec!["/Users/t/Github/wake".to_string()]
        );
    }

    #[test]
    fn cwd_inside_project_picks_longest_ancestor() {
        let p = fixtures();
        assert_eq!(
            resolve_project_paths("/Users/t/Github/wake/src/db", &p),
            vec!["/Users/t/Github/wake".to_string()]
        );
        assert_eq!(
            resolve_project_paths("/Users/t/Github/wake/crates/wake-core/src", &p),
            vec!["/Users/t/Github/wake/crates/wake-core".to_string()]
        );
    }

    #[test]
    fn ancestor_dir_unions_projects_below_it() {
        let p = fixtures();
        assert_eq!(
            resolve_project_paths("/Users/t/Github", &p),
            vec![
                "/Users/t/Github/wake".to_string(),
                "/Users/t/Github/wake-old".to_string(),
                "/Users/t/Github/wake/crates/wake-core".to_string(),
            ]
        );
    }

    #[test]
    fn sibling_with_shared_prefix_is_not_a_descendant() {
        let p = vec![project("/Users/t/Github/wake", "wake")];
        assert!(resolve_project_paths("/Users/t/Github/wake-old", &p).is_empty());
        assert!(resolve_project_paths("/nope", &p).is_empty());
        assert!(resolve_project_paths("   ", &p).is_empty());
    }

    /// 根与家目录是一切路径的祖先,不当"所在项目":还没有会话的新项目问不出它们的会话;
    /// 真正的上级项目照常,人就站在家目录里时也照常是它
    #[test]
    fn root_and_home_are_nobodys_enclosing_project() {
        let mut p = fixtures();
        p.push(project("/", ""));
        p.push(project("/Users/t", "t"));
        let home = Some("/Users/t/");
        assert!(resolve_in("/Users/t/Github/brand-new", &p, home).is_empty());
        assert!(resolve_in("/opt/elsewhere", &p, home).is_empty());
        assert_eq!(
            resolve_in("/Users/t/Github/wake/src", &p, home),
            vec!["/Users/t/Github/wake".to_string()]
        );
        assert_eq!(
            resolve_in("/Users/t", &p, home),
            vec!["/Users/t".to_string()]
        );
        assert_eq!(resolve_in("/", &p, home), vec!["/".to_string()]);
        assert!(catch_all(r"C:\", None));
        assert!(!catch_all("/Users/t/Github", home));
    }

    #[test]
    fn drive_letter_paths_are_paths_on_every_platform() {
        let p = vec![project(r"C:\Users\t\Github\wake", "wake")];
        assert_eq!(
            resolve_project_paths(r"C:\Users\t\Github\wake", &p),
            vec![r"C:\Users\t\Github\wake".to_string()]
        );
        // Unix 上 `\` 不是分隔符,后代匹配只在 Windows 成立;但盘符串在任何
        // 平台都不该被当成项目名去比
        assert!(resolve_project_paths(r"D:\elsewhere", &p).is_empty());
        #[cfg(windows)]
        assert_eq!(
            resolve_project_paths(r"C:\Users\t\Github\wake\src", &p),
            vec![r"C:\Users\t\Github\wake".to_string()]
        );
    }

    #[test]
    fn bare_name_matches_project_name_case_insensitively() {
        let p = fixtures();
        assert_eq!(
            resolve_project_paths("Wake", &p),
            vec!["/Users/t/Github/wake".to_string()]
        );
        assert!(resolve_project_paths("billing", &p).is_empty());
    }

    #[test]
    fn since_relative_and_absolute_forms() {
        let now = 1_800_000_000_000;
        assert_eq!(parse_since("7d", now), Some(now - 7 * 86_400_000));
        assert_eq!(parse_since("90m", now), Some(now - 90 * 60_000));
        assert_eq!(parse_since("2w", now), Some(now - 14 * 86_400_000));
        assert_eq!(
            parse_since("2026-09-01T00:00:00Z", now),
            Some(1_788_220_800_000)
        );
        let midnight = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2026, 9, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap()
            .timestamp_millis();
        assert_eq!(parse_since("2026-09-01", now), Some(midnight));
        assert_eq!(parse_since("2026-09-01 00:00", now), Some(midnight));
        assert_eq!(parse_since("yesterday", now), None);
        assert_eq!(parse_since("-3d", now), None);
        assert_eq!(parse_since("", now), None);
        // 非 ASCII 收尾只能是 None,绝不能 panic
        assert_eq!(parse_since("7天", now), None);
        assert_eq!(parse_since("昨天", now), None);
        assert_eq!(parse_since("天", now), None);
    }
}
