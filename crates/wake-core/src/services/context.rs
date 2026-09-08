//! agent 取上下文用的纯函数(wake-mcp 工具层调用;无 UI、无 IO)。
//! 项目参数的三级匹配与 `since` 解析都在这里,单测卡住语义——工具层只管拼装。

use crate::adapters::path_owns;
use crate::models::ProjectInfo;
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone as _};

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
        let mut best: Option<&ProjectInfo> = None;
        for p in projects.iter().filter(|p| !p.path.is_empty()) {
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

/// 是"路径"而非"项目名"的判据。不能只看 `Path::is_absolute`:Windows 上它对
/// `/Users/…` 返回 false,而索引里的远程镜像会话(以及 CI 上的 fixture)全是
/// POSIX 路径;反过来 Unix 上也认 `C:\…` 形态,一个盘符串不可能是项目名
fn looks_like_path(s: &str) -> bool {
    let bytes = s.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    std::path::Path::new(s).is_absolute() || s.starts_with(std::path::is_separator) || drive
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

fn local_ms(ndt: NaiveDateTime) -> Option<i64> {
    Local
        .from_local_datetime(&ndt)
        .earliest()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

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
