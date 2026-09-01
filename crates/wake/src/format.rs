use chrono::{DateTime, Datelike, Duration, Local, TimeZone};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// 按终端显示单元宽度截断文本，并在确有截断时补一个省略号。
///
/// CJK 宽字符按 2 个单元计算，并以 Unicode 字素簇为不可拆分单位；这比按
/// code point 范围猜测宽度准确，也不会切断 ZWJ/肤色修饰 Emoji。
pub fn clip_display(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }

    let ellipsis = "…";
    let ellipsis_width = ellipsis.width();
    if max_width < ellipsis_width {
        return String::new();
    }

    let content_width = max_width - ellipsis_width;
    let mut clipped = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let width = grapheme.width();
        if used + width > content_width {
            break;
        }
        clipped.push_str(grapheme);
        used += width;
    }
    clipped.push_str(ellipsis);
    clipped
}

/// 会话时间的渐进式展示：越近越强调新鲜度，越远越强调日期锚点。
pub fn smart_time(ts: i64) -> String {
    smart_time_from(ts, &Local::now())
}

fn smart_time_from(ts: i64, now: &DateTime<Local>) -> String {
    if ts <= 0 {
        return String::new();
    }

    let Some(dt) = Local.timestamp_millis_opt(ts).single() else {
        return String::new();
    };
    let diff = now.timestamp_millis() - ts;
    const MIN: i64 = 60_000;
    if (0..MIN).contains(&diff) {
        "Just now".to_string()
    } else if (MIN..60 * MIN).contains(&diff) {
        format!("{} min ago", diff / MIN)
    } else if dt.date_naive() == now.date_naive() {
        dt.format("%-I:%M %p").to_string()
    } else if dt.date_naive() == now.date_naive() - Duration::days(1) {
        "Yesterday".to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %-d").to_string()
    } else {
        dt.format("%b %-d, %Y").to_string()
    }
}

/// 详情页时间信息：保留到秒，避免相对时间丢失会话的精确时间上下文。
pub fn abs_date(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// epoch ms → "Mar 2026"(Insights 副标题);无效或 ≤0 给空串。
/// ts→字符串一律住本模块,渲染层不直接碰 chrono
pub fn month_year(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.format("%b %Y").to_string())
        .unwrap_or_default()
}

/// 千分位分组(Insights 大数字用):1234567 → "1,234,567"
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

pub fn fmt_tokens(n: Option<i64>) -> String {
    match n {
        None | Some(0) => String::new(),
        Some(n) if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

/// 进程内不变的 HOME,缓存住:数据源路径折叠与手输展开(expand_tilde)
/// 共用同一份。
fn cached_home() -> Option<&'static str> {
    static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        // 与 adapter 侧同一个 HOME(wake_core::adapters::home_dir 的
        // WAKE_HOME 开关):两边不一致时,改道过的数据根不会折成 `~`
        std::env::var_os("WAKE_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(dirs::home_dir)
            .map(|h| h.to_string_lossy().to_string())
            .filter(|h| !h.is_empty())
    })
    .as_deref()
}

/// 绝对路径 → `~/…` 形式,**不折叠中段**。数据源面板要如实给出完整路径。
pub fn tilde_path(p: &str) -> String {
    // 边界必须落在分隔符上:裸 starts_with 会把 HOME 的同名前缀兄弟目录
    // 也折叠掉(HOME=/Users/al 时 /Users/al-data → "~-data",一个并不存在
    // 的 HOME 相对路径)。自定义 CODEX_HOME / XDG_DATA_HOME 让这种根变得可能。
    // 分隔符判定走 std::path::is_separator:Windows 上 `\` 与 `/` 都算
    match cached_home() {
        Some(h) => match p.strip_prefix(h) {
            Some(rest) if rest.is_empty() || rest.starts_with(std::path::is_separator) => {
                format!("~{rest}")
            }
            _ => p.to_string(),
        },
        None => p.to_string(),
    }
}

/// 手输路径的 `~` 前缀展开(tilde_path 的逆;仅前缀,边界同样落在分隔符上
/// ——Windows 用户手输 `~\foo` 同样认)
pub fn expand_tilde(p: &str) -> String {
    match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(std::path::is_separator) => {
            match cached_home() {
                Some(h) => format!("{h}{rest}"),
                None => p.to_string(),
            }
        }
        _ => p.to_string(),
    }
}

/// 首行截断预览
pub fn one_line(s: &str, max_chars: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() > max_chars {
        let mut t: String = chars[..max_chars].iter().collect();
        t.push('…');
        t
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("test date should be unambiguous in the local timezone")
    }

    #[test]
    fn smart_time_uses_progressive_precision() {
        let now = local_time(2026, 8, 26, 18, 0);

        assert_eq!(
            smart_time_from((now - Duration::seconds(30)).timestamp_millis(), &now),
            "Just now"
        );
        assert_eq!(
            smart_time_from((now - Duration::minutes(23)).timestamp_millis(), &now),
            "23 min ago"
        );
        assert_eq!(
            smart_time_from(local_time(2026, 8, 26, 15, 42).timestamp_millis(), &now),
            "3:42 PM"
        );
        assert_eq!(
            smart_time_from(local_time(2026, 8, 25, 12, 0).timestamp_millis(), &now),
            "Yesterday"
        );
        assert_eq!(
            smart_time_from(local_time(2026, 8, 21, 12, 0).timestamp_millis(), &now),
            "Aug 21"
        );
        assert_eq!(
            smart_time_from(local_time(2025, 8, 21, 12, 0).timestamp_millis(), &now),
            "Aug 21, 2025"
        );
    }

    #[test]
    fn clip_display_counts_cjk_and_latin_widths() {
        assert_eq!(clip_display("Wake 会话", 9), "Wake 会话");
        assert_eq!(clip_display("Wake 会话标题", 9), "Wake 会…");
        assert_eq!(clip_display("超长会话标题", 7), "超长会…");
    }

    #[test]
    fn clip_display_respects_zero_width_marks_and_tiny_limits() {
        let combined = "e\u{301}clair";
        assert_eq!(clip_display(combined, 4), "e\u{301}cl…");
        assert_eq!(clip_display("abc", 1), "…");
        assert_eq!(clip_display("abc", 0), "");
    }

    #[test]
    fn clip_display_never_splits_emoji_grapheme_clusters() {
        let family = "👩‍👩‍👧‍👦";
        assert_eq!(
            clip_display(&format!("{family}xy"), 3),
            format!("{family}…")
        );

        let toned = "👍🏽";
        assert_eq!(clip_display(&format!("{toned}xy"), 3), format!("{toned}…"));
    }
}
