use chrono::{Local, TimeZone};

use crate::i18n::{Language, TextKey};

pub fn relative_time(ts: i64, language: Language) -> String {
    if ts <= 0 {
        return String::new();
    }
    let now = chrono::Utc::now().timestamp_millis();
    let diff = now - ts;
    const MIN: i64 = 60_000;
    if diff < MIN {
        language.text(TextKey::Now).to_string()
    } else if diff < 60 * MIN {
        format!("{}m", diff / MIN)
    } else if diff < 24 * 60 * MIN {
        format!("{}h", diff / (60 * MIN))
    } else if diff < 7 * 24 * 60 * MIN {
        format!("{}d", diff / (24 * 60 * MIN))
    } else {
        Local
            .timestamp_millis_opt(ts)
            .single()
            .map(|d| d.format("%m-%d").to_string())
            .unwrap_or_default()
    }
}

/// 绝对时间(详情页时间行):yyyy-MM-dd HH:mm:ss
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

pub fn fmt_tokens(n: Option<i64>) -> String {
    match n {
        None | Some(0) => String::new(),
        Some(n) if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

/// 会话文件路径的展示形态(详情页路径行):SQLite 虚拟路径剥 `#<id>`、
/// HOME 缩成 `~`、深路径折叠中段(根目录 + … + 文件名)。
/// 仅用于展示——Reveal in Finder/Explorer 仍传原始完整路径。
pub fn display_file_path(path: &str) -> String {
    // 虚拟路径 <db>#<id>:id 不是路径的一部分,展示到库文件为止
    let p = path
        .rsplit_once('#')
        .filter(|(db, _)| db.ends_with(".db"))
        .map(|(db, _)| db)
        .unwrap_or(path);
    let normalized = p.replace('\\', "/");
    let home = wake_core::home_dir().to_string_lossy().replace('\\', "/");
    let tilde = if !home.is_empty() && normalized.starts_with(home.as_str()) {
        format!("~{}", &normalized[home.len()..])
    } else {
        normalized
    };
    let parts: Vec<&str> = tilde.split('/').collect();
    match (parts.first(), parts.get(1), parts.last()) {
        // 超过 根/次级/…/文件 四段的深路径折叠中段
        (Some(root), Some(second), Some(file)) if parts.len() > 4 => {
            format!("{root}/{second}/…/{file}")
        }
        _ => tilde,
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
