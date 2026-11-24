//! 跨数据层与 UI 共用的文本小工具。wake crate 的 `format::one_line` 转发到
//! 这里——同一条"折行截断"规则只写一次(曾在两个 crate 各有一份、同名不同义)。
//! 搜索命中的片段与高亮也在这里:会话、记忆(db.rs)与 skill(skills.rs)三处共用

use crate::models::{HL_CLOSE, HL_OPEN};

/// 折成一行:空白压成单空格,超过 `max_chars` 个字符截断并加 `…`
/// (按字符数计,CJK 安全)。够长就停,不先把整段压平——详情页每帧都拿几十 KB 的注入内容
/// 取一行摘要
pub fn one_line(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in s.split_whitespace() {
        let separator = (!out.is_empty()).then_some(' ');
        for c in separator.into_iter().chain(word.chars()) {
            if count == max_chars {
                out.push('…');
                return out;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

/// 英文复数后缀:1 → ""、其余 → "s"
pub fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// 检索用的大小写折叠,逐字符:正文与检索词同一个取法、对齐 FTS 的 trigram——它把
/// ΟΣΟΣ / οσος / οσοσ 当成同一个词。整串 to_lowercase 会把词尾的 Σ 变成 ς(希腊文词尾
/// sigma 规则),逐字符的正文那边是 σ,两边就对不上;ς 也并成 σ
fn fold(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase().map(|c| if c == 'ς' { 'σ' } else { c })
}

/// 整串按 `fold` 折叠。两个折叠后的串之间的子串关系就是高亮认的那些位置(UTF-8 下按字节找
/// 子串等于按字符找),skill 搜索拿它比
pub(crate) fn fold_case(s: &str) -> String {
    s.chars().flat_map(fold).collect()
}

/// 搜索串按空白切成词项:会话、记忆与 skill 三处搜索同一套分词
pub(crate) fn fts_terms(q: &str) -> Vec<&str> {
    q.split_whitespace().collect()
}

/// 命中片段:以最早出现的检索词为锚,前 40 / 后 80 字符开窗,窗口里每个检索词的每次
/// 出现都高亮(相邻、重叠的并成一段)。会话正文、记忆(FTS / LIKE 两条路径)与 skill 的
/// 说明共用这一个;标题不开窗(`highlight_all`)。FTS5 的 snippet() 按词元数开窗,trigram
/// 分词下一个词元是三个字符的滑窗,16 个词元只露出十几个字,看不出命中在说什么
pub(crate) fn make_snippet(text: &str, segs: &[&str]) -> String {
    mark_terms(text, segs, Some((40, 80)))
}

/// 整段原样、每个检索词的每次出现都高亮,不开窗。标题命中用它:标题短,三条标题路径(只有三字
/// 以上的词、另带短词、全是短词)同一个取法,同一个标题不因查询里有没有短词显示得不一样
pub(crate) fn highlight_all(text: &str, segs: &[&str]) -> String {
    mark_terms(text, segs, None)
}

/// 逐字符往后折叠的正文:`chars` 是读过的原文字符,`lower` 是它们折叠后的字符,`origin[i]` 是
/// `lower[i]` 来自 `chars` 的第几个
struct Folding<'a> {
    rest: std::str::Chars<'a>,
    chars: Vec<char>,
    lower: Vec<char>,
    origin: Vec<usize>,
    /// 原文读完了
    done: bool,
}

impl<'a> Folding<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            rest: text.chars(),
            chars: Vec::new(),
            lower: Vec::new(),
            origin: Vec::new(),
            done: false,
        }
    }

    /// 读到原文的前 `n` 个字符(不够就读完)
    fn read_to(&mut self, n: usize) {
        while self.chars.len() < n {
            let Some(c) = self.rest.next() else {
                self.done = true;
                return;
            };
            for lc in fold(c) {
                self.lower.push(lc);
                self.origin.push(self.chars.len());
            }
            self.chars.push(c);
        }
    }

    /// 每个检索词在读过的那一段里的每次出现(各词各自从左往右、不重叠地找),换算成原文的
    /// 字符区间 [start, end),排好序
    fn spans(&self, needles: &[Vec<char>]) -> Vec<(usize, usize)> {
        let lower = &self.lower;
        let mut spans = Vec::new();
        for needle in needles {
            let mut i = 0;
            while i + needle.len() <= lower.len() {
                if lower[i..i + needle.len()] == needle[..] {
                    spans.push((self.origin[i], self.origin[i + needle.len() - 1] + 1));
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
        }
        spans.sort_unstable();
        spans
    }
}

/// `make_snippet` 与 `highlight_all` 的本体,`window` 是锚点前后各留多少字符。
///
/// 全程按**字符**下标走:逐字符小写并记下每个小写字符来自原文第几个字符——小写
/// 不保长(Ω→ω、İ→i̇、K→k 字节数与字符数都会变),拿小写串里的字节偏移去切原文正是
/// 2026-09-21 review 复现的 panic(⌘K 逐字键入 CJK 查询必经此路,后台线程 panic
/// 直接 abort 整个 app)。
///
/// 开窗时只折叠用得着的开头一段:先折一段,最早的命中与窗口末尾都落在里面就停,不够再加倍往后
/// 折。记忆正文里有几十万字的 CLAUDE.md,⌘K 每次输入要给 30 份记忆取片段,整篇折叠一次要
/// 5–12 ms、分配十几 MB;命中靠前的只用折几百个字符
fn mark_terms(text: &str, segs: &[&str], window: Option<(usize, usize)>) -> String {
    let needles: Vec<Vec<char>> = segs
        .iter()
        .map(|seg| seg.chars().flat_map(fold).collect::<Vec<char>>())
        .filter(|needle| !needle.is_empty())
        .collect();
    let longest = needles.iter().map(Vec::len).max().unwrap_or(0);
    let mut folding = Folding::new(text);
    let mut want = window.map_or(usize::MAX, |(before, after)| (before + after).max(256));
    let spans = loop {
        folding.read_to(want);
        let spans = folding.spans(&needles);
        let Some((_, after)) = window else {
            break spans;
        };
        // 窗口末尾之后再多折一个最长词:开头在窗口里、跨过末尾的命中才找得全;再多一个字符,才知道
        // 后面还有没有(省略号)。比最早那个开头还早的命中,也一定在这一段里收尾
        let need = spans
            .first()
            .map_or(usize::MAX, |&(_, end)| end + after + longest + 1);
        if folding.done || folding.chars.len() >= need {
            break spans;
        }
        want = need.max(want.saturating_mul(2));
    };
    let chars = &folding.chars;
    let Some(&(anchor_start, anchor_end)) = spans.first() else {
        return match window {
            Some((before, after)) => chars.iter().take(before + after).collect(),
            None => text.to_string(),
        };
    };
    let (start, end) = match window {
        Some((before, after)) => (
            anchor_start.saturating_sub(before),
            (anchor_end + after).min(chars.len()),
        ),
        None => (0, chars.len()),
    };
    let mut marks: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in &spans {
        if s >= end {
            break;
        }
        let e = e.min(end);
        match marks.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => marks.push((s, e)),
        }
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut cursor = start;
    for (s, e) in marks {
        out.extend(&chars[cursor..s]);
        out.push(HL_OPEN);
        out.extend(&chars[s..e]);
        out.push(HL_CLOSE);
        cursor = e;
    }
    out.extend(&chars[cursor..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_flattens_and_clips_by_chars() {
        assert_eq!(one_line("  a\n\n b\tc ", 10), "a b c");
        assert_eq!(one_line("二维码扫描登录", 3), "二维码…");
        assert_eq!(one_line("abc", 3), "abc", "恰好等长不截");
    }

    /// 够长就停的写法与"先整段压平再按字符截"逐字一样:截在词中间、截在词间的空格上、
    /// 恰好写满、空串、只有空白、上限为零
    #[test]
    fn one_line_matches_flatten_then_clip() {
        let reference = |s: &str, max: usize| {
            let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
            match flat.char_indices().nth(max) {
                Some((ix, _)) => format!("{}…", &flat[..ix]),
                None => flat,
            }
        };
        let text = " <system-reminder>\n Linked  sessions now:\tdeploy = idle 看下部署成功没有 ";
        for max in 0..=text.chars().count() + 1 {
            for s in [text, "", "   \n\t", "abc def", "二维码 扫描"] {
                assert_eq!(one_line(s, max), reference(s, max), "{s:?} {max}");
            }
        }
    }

    #[test]
    fn plural_suffix() {
        assert_eq!(plural(1), "");
        assert_eq!(plural(0), "s");
        assert_eq!(plural(2), "s");
    }

    /// 把哨兵换成方括号,断言好写好读
    fn shown(s: &str) -> String {
        s.replace(HL_OPEN, "[").replace(HL_CLOSE, "]")
    }

    #[test]
    fn snippet_windows_around_the_first_hit_and_marks_every_term() {
        let text = format!(
            "{}先说二维码,再说 useEffect( 的挂载,最后又提到二维码。{}",
            "前文".repeat(40),
            "后文".repeat(80)
        );
        let s = shown(&make_snippet(&text, &["二维码", "useeffect("]));
        assert!(
            s.starts_with('…') && s.ends_with('…'),
            "长文两头都要省略:{s}"
        );
        assert!(
            s.contains("先说[二维码],再说 [useEffect(] 的挂载,最后又提到[二维码]"),
            "窗口里每个词的每次出现都标上,大小写按原文:{s}"
        );
        // 前 40 后 80 字符的窗口,再加两个省略号
        assert!(s.chars().count() <= 40 + 80 + "二维码".chars().count() + 2 + 6);
        // 标题那种不开窗的:整段原样,每次出现都标
        let all = shown(&highlight_all(&text, &["二维码"]));
        assert_eq!(all.replace(['[', ']'], ""), text);
        assert_eq!(all.matches("[二维码]").count(), 2);
    }

    #[test]
    fn snippet_merges_overlapping_terms_and_falls_back_to_the_start() {
        assert_eq!(shown(&make_snippet("xabcdx", &["abc", "bcd"])), "x[abcd]x");
        let plain = "没有命中的长文本".repeat(30);
        let s = make_snippet(&plain, &["二维码"]);
        assert_eq!(s.chars().count(), 120, "找不到词就给开头 120 字");
        assert!(!s.contains(HL_OPEN));
    }

    #[test]
    fn snippet_survives_case_mappings_that_change_length() {
        // Ω/İ/K 小写后字符数或字节数会变,按原文字符下标切才不会 panic 或错位
        let s = shown(&make_snippet(
            "İstanbul Ωmega KELVIN 二维码",
            &["ωmega", "kelvin"],
        ));
        assert_eq!(s, "İstanbul [Ωmega] [KELVIN] 二维码");
        let s = shown(&make_snippet("İİİ二维码", &["二维码"]));
        assert_eq!(s, "İİİ[二维码]");
    }

    /// 希腊文词尾的 sigma:FTS 的 trigram 把 ΟΣΟΣ / οσος / οσοσ 当成同一个词,片段也得认得出——
    /// 整串小写会把查询词尾的 Σ 变成 ς,逐字符小写的正文那边却是 σ
    #[test]
    fn snippet_folds_greek_final_sigma_like_the_index() {
        let text = format!("{} ΟΣΟΣ τέλος", "α".repeat(200));
        for query in ["ΟΣΟΣ", "οσος", "οσοσ"] {
            let s = shown(&make_snippet(&text, &[query]));
            assert!(s.contains("[ΟΣΟΣ]"), "{query}: {s}");
        }
        assert!(shown(&make_snippet("τέλος οσος", &["ΟΣΟΣ"])).contains("[οσος]"));
    }

    /// 折叠后按子串比,认的就是高亮标得出来的那些
    #[test]
    fn folded_strings_match_where_the_highlight_does() {
        let found = |text: &str, term: &str| fold_case(text).contains(&fold_case(term));
        assert!(found("İstanbul Ωmega KELVIN", "ωMEGA"));
        assert!(found("İstanbul Ωmega \u{212A}ELVIN", "kelvin"));
        assert!(found("τέλος ΟΣΟΣ", "οσος"));
        assert!(found("部署脚本", "署脚"));
        assert!(!found("deploy", "deploys"));
        for (text, term) in [("İstanbul Ωmega", "ωmega"), ("τέλος ΟΣΟΣ", "οσοσ")] {
            assert!(
                shown(&highlight_all(text, &[term])).contains('['),
                "{text} / {term}"
            );
        }
    }

    /// 开窗时只折开头一段的写法,与整篇折叠、找出全部出现再开窗的原写法逐字相同。字母表里
    /// 专挑小写后变长或变形的字符(İ、K 开尔文号、Σ / ς、ß)
    #[test]
    fn windowed_marking_matches_folding_the_whole_text() {
        fn whole_text(text: &str, segs: &[&str], window: Option<(usize, usize)>) -> String {
            let chars: Vec<char> = text.chars().collect();
            let mut lower = Vec::new();
            let mut origin = Vec::new();
            for (i, &c) in chars.iter().enumerate() {
                for lc in fold(c) {
                    lower.push(lc);
                    origin.push(i);
                }
            }
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for seg in segs {
                let needle: Vec<char> = seg.chars().flat_map(fold).collect();
                let mut i = 0;
                while !needle.is_empty() && i + needle.len() <= lower.len() {
                    if lower[i..i + needle.len()] == needle[..] {
                        spans.push((origin[i], origin[i + needle.len() - 1] + 1));
                        i += needle.len();
                    } else {
                        i += 1;
                    }
                }
            }
            spans.sort_unstable();
            let Some(&(anchor_start, anchor_end)) = spans.first() else {
                return match window {
                    Some((before, after)) => chars.iter().take(before + after).collect(),
                    None => text.to_string(),
                };
            };
            let (start, end) = match window {
                Some((before, after)) => (
                    anchor_start.saturating_sub(before),
                    (anchor_end + after).min(chars.len()),
                ),
                None => (0, chars.len()),
            };
            let mut marks: Vec<(usize, usize)> = Vec::new();
            for &(s, e) in &spans {
                if s >= end {
                    break;
                }
                let e = e.min(end);
                match marks.last_mut() {
                    Some(last) if s <= last.1 => last.1 = last.1.max(e),
                    _ => marks.push((s, e)),
                }
            }
            let mut out = String::new();
            if start > 0 {
                out.push('…');
            }
            let mut cursor = start;
            for (s, e) in marks {
                out.extend(&chars[cursor..s]);
                out.push(HL_OPEN);
                out.extend(&chars[s..e]);
                out.push(HL_CLOSE);
                cursor = e;
            }
            out.extend(&chars[cursor..end]);
            if end < chars.len() {
                out.push('…');
            }
            out
        }

        const ALPHABET: [char; 14] = [
            'a', 'b', 'A', 'B', 'İ', 'i', 'Σ', 'σ', 'ς', '\u{212A}', 'k', 'ß', '二', ' ',
        ];
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut below = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..3000 {
            let mut text = String::new();
            for _ in 0..below(900) {
                text.push(ALPHABET[below(ALPHABET.len())]);
            }
            let mut terms: Vec<String> = Vec::new();
            for _ in 0..1 + below(3) {
                let mut term = String::new();
                for _ in 0..1 + below(4) {
                    term.push(ALPHABET[below(ALPHABET.len())]);
                }
                terms.push(term);
            }
            let segs: Vec<&str> = terms.iter().map(String::as_str).collect();
            for window in [Some((40, 80)), Some((0, 3)), Some((5, 0)), None] {
                assert_eq!(
                    mark_terms(&text, &segs, window),
                    whole_text(&text, &segs, window),
                    "{text:?} {segs:?} {window:?}"
                );
            }
        }
    }
}
