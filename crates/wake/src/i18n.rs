//! Wake 的多语言层。
//!
//! **key 就是英文原文**:漏翻译天然回退英文(查不到即返回 key 自身),改文案
//! 不必同步改 key,代码里读到的还是那句话。代价是同一英文在两处需要不同译文时
//! 会撞——真撞上再给其中一处加消歧后缀,别提前发明符号 key 体系。
//!
//! `t()` 返回 `&'static str` 而非 String/SharedString:调用点是 `.child("Settings")`
//! → `.child(t("Settings"))` 的**原地替换**,所有既有签名(`label: &'static str`
//! 这类)一字不改。代价是译文串要 leak——语言表整表 leak,切一次语言泄漏一份
//! (几十 KB),而切语言是罕见操作(`set_language` 另有同档位早退)。
//!
//! ## 加一门语言
//!
//! 两条路,都不需要动这个文件之外的逻辑:
//! - **提 PR**:`crates/wake/locales/<BCP47>.json`,再在下面 `BUNDLED` 加一行。
//! - **不重编译**:把同样的 JSON 丢进 `<config dir>/wake/locales/<BCP47>.json`,
//!   重启即出现在 Settings 的语言列表里;同 tag 的外部文件整体顶掉内嵌那份。
//!
//! JSON 是平铺的 `{"英文原文": "译文"}`,另有一个 `"$name"` 给这门语言的自称。
//! **英文自己也是一个语言包**(`locales/en.json`,只有 `$name`)——这样"一门语言
//! 就是一个 JSON 文件"没有例外,`available()` 不必给英文开后门,用户也能靠
//! `<config dir>/wake/locales/en.json` 改写英文原文。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::RwLock;

use gpui::App;

/// 内嵌语言包:(BCP 47 tag, 这门语言的自称, JSON 正文)。社区加语言在这里
/// 登记一行,文件放 `crates/wake/locales/`。
///
/// 自称写在这里而不是只放在 JSON 里,是为了让**发现**不必解析:启动时
/// `available()` 要列出每门语言的名字,若从 JSON 里取就得把每个包都完整
/// parse 一遍再全部丢掉(只为一个字段),随社区包增多线性变慢。JSON 里的
/// `$name` 仍然保留且必须一致(测试卡住),外部包就靠它。
const BUNDLED: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../locales/en.json")),
    ("zh-Hans", "简体中文", include_str!("../locales/zh-Hans.json")),
];

/// 语言包里 native name 的保留 key(`$` 前缀不会与任何 UI 文案撞)
const NAME_KEY: &str = "$name";

/// prefs 文件名。内容是 tag,或 `system` = 跟随系统
const PREF: &str = "language";

/// 一门可选语言:tag 是 BCP 47(`zh-Hans`),name 是它的自称(`简体中文`)
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Locale {
    pub tag: &'static str,
    pub name: &'static str,
}

type Table = HashMap<&'static str, &'static str>;

/// 当前生效的语言:tag 与它的译文表恒同生同死,所以是一个 static 而不是两个。
/// None = 英文原文——**空表也归到这里**(英文包本身、以及坏掉的包),于是
/// `t()` 在英文下连哈希都不查
static ACTIVE: RwLock<Option<(&'static str, &'static Table)>> = RwLock::new(None);

/// 装表的次数。UI 里凡是**缓存过译文**的地方(列表的日期分组标签、输入框
/// 的 placeholder)都要记下取用时的代次,变了就重算——`refresh_windows()`
/// 只触发重绘,不会让已经存进 entity 的字符串跟着变
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 当前语言代次,见 `GENERATION`
pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// 翻译一句 UI 文案。查不到就是英文原文——**永远不会显示 key 名**
pub fn t(key: &'static str) -> &'static str {
    match *ACTIVE.read().expect("i18n table") {
        Some((_, table)) => table.get(key).copied().unwrap_or(key),
        None => key,
    }
}

/// 带参数的文案。占位符 `{}` 按顺序取参、`{0}`/`{1}` 按下标取参
/// ——译文语序与英文不同时用下标形式重排,不必迁就英文的参数顺序。
/// 认不出的花括号内容(`{foo}`)原样输出。
pub fn tf(key: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    let template = t(key);
    let mut out = String::with_capacity(template.len() + args.len() * 8);
    let mut next = 0usize;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // 没有闭合花括号:剩下的全是普通文本
            out.push_str(&rest[open..]);
            return out;
        };
        let slot = &after[..close];
        let picked = if slot.is_empty() {
            let ix = next;
            next += 1;
            args.get(ix)
        } else if let Ok(ix) = slot.parse::<usize>() {
            args.get(ix)
        } else {
            None
        };
        match picked {
            // 直接写进 out:`arg.to_string()` 会为每个参数多分配一个临时 String
            Some(arg) => {
                let _ = write!(out, "{arg}");
            }
            // 越界或非占位:原样保留,别把 UI 变成空白
            None => {
                out.push('{');
                out.push_str(slot);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// 英语的单复数:`n == 1` 取 one 支,否则取 other 支。两支都是完整句子
/// (`"{} session"` / `"{} sessions"`),中文两条译成同一句即可
pub fn tp(one: &'static str, other: &'static str, n: i64, args: &[&dyn std::fmt::Display]) -> String {
    tf(if n == 1 { one } else { other }, args)
}

/// `tf` 的调用糖:`tf!("{} messages", n)`
#[macro_export]
macro_rules! tf {
    ($key:literal $(, $arg:expr)* $(,)?) => {
        $crate::i18n::tf($key, &[$(&$arg),*])
    };
}

/// `tp` 的调用糖:`tp!("{} session", "{} sessions", n)`。
/// **计数即第一个参数**——它总要出现在句子里,分开写就是把同一个值传两遍。
/// 译文要把它挪到别处用 `{0}`(见 `tf` 的下标占位)
#[macro_export]
macro_rules! tp {
    ($one:literal, $other:literal, $n:expr $(, $arg:expr)* $(,)?) => {{
        let n = $n;
        $crate::i18n::tp($one, $other, n as i64, &[&n $(, &$arg)*])
    }};
}

// ---------------- 语言包发现与加载 ----------------

/// 用户语言包目录 `<config dir>/wake/locales/`
fn user_dir() -> Option<std::path::PathBuf> {
    crate::prefs::dir().map(|dir| dir.join("locales"))
}

/// 一门语言的原始 JSON 文本:用户目录优先(同 tag 整体顶掉内嵌那份,
/// 允许用户在本机覆盖官方翻译),否则用内嵌的
fn source_for(tag: &str) -> Option<String> {
    if let Some(dir) = user_dir() {
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{tag}.json"))) {
            return Some(text);
        }
    }
    BUNDLED
        .iter()
        .find(|(bundled, ..)| *bundled == tag)
        .map(|(.., text)| (*text).to_string())
}

/// 解析一个语言包。**坏包给空表而不是消失**:JSON 语法错、顶层不是对象、
/// 值不是字符串——这些都退化成"这门语言的这些句子还是英文",而不是让整门
/// 语言从选择器里蒸发(那种失败是静默的,用户只会看见语言列表少了一项)
fn parse(text: &str) -> HashMap<String, String> {
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str(text) else {
        return HashMap::new();
    };
    object
        .into_iter()
        .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
        .collect()
}

/// 语言列表要活到进程结束,tag 与 native name 都 leak 成 'static
fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// 可选语言 = 内嵌 ∪ 用户目录里的 `*.json`(同 tag 由用户目录顶掉),按 tag
/// 字典序。
///
/// **只扫一次**:tag 与 native name 都要 leak 成 'static,而语言选择器在
/// Settings 的 render 路径上(每帧调),不缓存就是每帧漏几十字节。代价是
/// 运行中往 locales/ 丢的新语言包要重启才出现——README 就是这么写的
pub fn available() -> &'static [Locale] {
    static CACHE: std::sync::OnceLock<Vec<Locale>> = std::sync::OnceLock::new();
    CACHE.get_or_init(scan_available)
}

fn scan_available() -> Vec<Locale> {
    let mut out: Vec<Locale> = BUNDLED
        .iter()
        .map(|(tag, name, _)| Locale { tag, name })
        .collect();
    // 用户目录:同 tag 覆盖内嵌那份的自称,新 tag 追加。只有这一支要解析
    // JSON,而且只为取 `$name`(内嵌包的自称在 BUNDLED 里,零解析)
    let entries = user_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten();
    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let (Some(stem), Ok(text)) = (
            path.file_stem().and_then(|s| s.to_str()),
            std::fs::read_to_string(&path),
        ) else {
            continue;
        };
        let name = parse(&text)
            .remove(NAME_KEY)
            .map_or_else(|| leak(stem.to_string()), leak);
        match out.iter_mut().find(|locale| locale.tag == stem) {
            Some(existing) => existing.name = name,
            None => out.push(Locale {
                tag: leak(stem.to_string()),
                name,
            }),
        }
    }
    out.sort_by_key(|locale| locale.tag);
    out
}

// ---------------- 系统语言匹配 ----------------

/// `zh-CN` 这类只带地区的 tag 推断书写系统——中文用户的系统 locale 绝大多数是
/// `zh-Hans-CN`/`zh-CN`/`zh-TW`,不补这一步就会把 zh-TW 匹配给简体包。
/// **入参必须已是小写**(调用方都已 lowercase,别在这里再转一次)。
///
/// 只认中文是有意的:安全属性是"zh-TW 绝不落进简体包",而更一般的规则
/// (带 script 子标签的包不参与主码回退)会连 `zh-CN` → 简体一起打掉,
/// 那是最常见的中文系统。地区→书写系统本就是 CLDR 数据,推不出来。
/// 第二门有简繁之分的语言(`sr-Cyrl`/`sr-Latn`)出现时,把这份知识挪进语言包
/// (`"$match": ["zh-Hans", "zh-CN", "zh-SG"]`),`match_system` 就不必认识任何语言了
fn script_hint(lower: &str) -> Option<&'static str> {
    let (lang, region) = lower.split_once('-')?;
    if lang != "zh" {
        return None;
    }
    match region.split('-').next_back().unwrap_or(region) {
        "cn" | "sg" | "my" | "hans" => Some("hans"),
        "tw" | "hk" | "mo" | "hant" => Some("hant"),
        _ => None,
    }
}

fn primary(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// 把系统 locale 匹配到一门已装语言。没有匹配 = 英文
fn match_system(locale: &str, choices: &[Locale]) -> Option<&'static str> {
    let want = locale.to_ascii_lowercase();
    let lower = |choice: &Locale| choice.tag.to_ascii_lowercase();
    // ① 完全相等。**必须自成一轮**:choices 按 tag 排序,`en` 排在 `en-GB`
    //    前面,和前缀判据混在同一轮里就会让装了 en-GB 的人永远匹配到 en
    //    (pt / pt-BR 同理)
    if let Some(choice) = choices.iter().find(|choice| lower(choice) == want) {
        return Some(choice.tag);
    }
    // ② 语言包 tag 是系统 locale 的前缀(zh-Hans ⊂ zh-Hans-CN);多个都匹配
    //    时取最长的,即最具体的那个
    if let Some(choice) = choices
        .iter()
        .filter(|choice| want.starts_with(&format!("{}-", lower(choice))))
        .max_by_key(|choice| choice.tag.len())
    {
        return Some(choice.tag);
    }
    // ③ 主语言码相同;中文另按书写系统裁一刀,免得 zh-TW 落进简体包
    let hint = script_hint(&want);
    let mut fallback = None;
    for choice in choices {
        let tag = lower(choice);
        if primary(&tag) != primary(&want) {
            continue;
        }
        match hint {
            Some(script) if tag.contains(script) => return Some(choice.tag),
            Some(_) => continue,
            None => fallback = fallback.or(Some(choice.tag)),
        }
    }
    fallback
}

/// gpui-component 自带组件(日期选择器、颜色选择器等)的语言。它的 tag 体系
/// 与我们的语言包不同名(en / zh-CN / zh-HK / zh-TW / it),按主语言码加书写
/// 系统映射;没有对应的就退英文——组件文案是英文、Wake 自己的文案仍是译文,
/// 比整个界面回退英文强
fn component_locale(tag: Option<&str>) -> &'static str {
    let Some(tag) = tag else { return "en" };
    let lower = tag.to_ascii_lowercase();
    match primary(&lower) {
        "zh" if script_hint(&lower) == Some("hant") => "zh-TW",
        "zh" => "zh-CN",
        "it" => "it",
        _ => "en",
    }
}

// ---------------- 偏好读写与应用 ----------------

/// 用户选定的语言;None = 跟随系统。**显式选的英文是 `Some(en)`**,与跟随
/// 系统区别开:系统语言是中文时两者结果不同。落盘的 `system` 不等于任何
/// tag,所以不必单独判
pub fn preference() -> Option<Locale> {
    let saved = crate::prefs::read(PREF)?;
    available().iter().copied().find(|l| l.tag == saved)
}

/// 当前实际生效的 tag(None = 英文原文)
fn active() -> Option<&'static str> {
    ACTIVE.read().expect("i18n active").map(|(tag, _)| tag)
}

/// `%` 开头的 key 是 chrono 模板不是散文。译文写坏了不是"显示得难看"——
/// `dt.format(坏模板).to_string()` 会 panic(实测 `%Q`:Display 返回 Err,
/// `to_string` 直接炸),而外部语言包是用户随手丢进 locales/ 的文件,没有
/// 任何一步会先看它一眼。丢掉这一条即回退英文模板
fn usable(key: &str, value: &str) -> bool {
    use chrono::format::{Item, StrftimeItems};
    !key.starts_with('%') || !StrftimeItems::new(value).any(|item| item == Item::Error)
}

/// 偏好 → 实际该装哪门语言。**跟随系统(None)不等于"没有语言"**,而是
/// "由 sys_locale 现算"——把这一步只放在 `init()` 里,运行时从 English 切回
/// 跟随系统就会掉成英文原文,得重启才回得到系统语言
fn resolve(preference: Option<&'static str>) -> Option<&'static str> {
    match preference {
        Some(tag) => Some(tag),
        None => sys_locale::get_locale()
            .as_deref()
            .and_then(|locale| match_system(locale, available())),
    }
}

/// 装载一门语言的译文表;None 卸回英文
fn load(tag: Option<&'static str>) {
    let loaded = tag.and_then(|tag| {
        let table: Table = parse(&source_for(tag)?)
            .into_iter()
            .filter(|(k, v)| k != NAME_KEY && usable(k, v))
            .map(|(k, v)| (leak(k), leak(v)))
            .collect();
        // 空表(英文包本身,或坏掉的包)当没装:`t()` 少一次哈希查询
        (!table.is_empty()).then(|| (tag, &*Box::leak(Box::new(table))))
    });
    *ACTIVE.write().expect("i18n table") = loaded;
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 启动时调用一次。**必须在建任何窗口/菜单之前**——菜单栏文案是 `set_menus`
/// 时求值的,晚一步就是一栏英文菜单配中文界面
pub fn init() {
    // 存过的偏好指向一门已经不在的语言包时,`preference()` 给 None,于是
    // 跟着系统走——与选择器那时显示的 "System" 一致
    load(resolve(preference().map(|locale| locale.tag)));
    gpui_component::set_locale(component_locale(active()));
}

/// 切语言:落盘 + 重装译文表 + 重设组件语言 + **重建菜单栏** + 全窗重绘。
/// 五步缺一不可(菜单栏文案是 `set_menus` 那一刻求值的静态串,不重建就留着
/// 上一门语言),所以这是唯一出口——装表那步不单独对外开放
pub fn set_language(tag: Option<&'static str>, cx: &mut App) -> std::io::Result<()> {
    if tag == preference().map(|locale| locale.tag) {
        // 点的就是当前档位:别再落一次盘(UI 线程上的真实磁盘 I/O),
        // 也别再 leak 一张几十 KB 的表
        return Ok(());
    }
    crate::prefs::write(PREF, tag.unwrap_or("system").as_bytes())?;
    load(resolve(tag));
    gpui_component::set_locale(component_locale(active()));
    cx.set_menus(crate::app_menus());
    cx.refresh_windows();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locales() -> Vec<Locale> {
        vec![
            Locale { tag: "zh-Hans", name: "简体中文" },
            Locale { tag: "zh-Hant", name: "繁體中文" },
            Locale { tag: "ja", name: "日本語" },
        ]
    }

    #[test]
    fn system_locale_matches_by_prefix_and_script() {
        let all = locales();
        assert_eq!(match_system("zh-Hans-CN", &all), Some("zh-Hans"));
        assert_eq!(match_system("zh-CN", &all), Some("zh-Hans"));
        assert_eq!(match_system("zh-TW", &all), Some("zh-Hant"));
        assert_eq!(match_system("ja-JP", &all), Some("ja"));
        assert_eq!(match_system("en-US", &all), None);
        // 只装了简体时,繁体系统宁可回落英文也不给简体
        let hans = vec![all[0]];
        assert_eq!(match_system("zh-TW", &hans), None);
        assert_eq!(match_system("zh-CN", &hans), Some("zh-Hans"));
    }

    #[test]
    fn component_locale_maps_our_tags_onto_gpui_components() {
        // gpui-component 的 locales/ui.yml 只有这五个
        assert_eq!(component_locale(Some("zh-Hans")), "zh-CN");
        assert_eq!(component_locale(Some("zh-Hant")), "zh-TW");
        assert_eq!(component_locale(Some("zh-TW")), "zh-TW");
        assert_eq!(component_locale(Some("it")), "it");
        assert_eq!(component_locale(Some("ja")), "en");
        assert_eq!(component_locale(None), "en");
    }

    /// 通用包与地区包并存时,精确匹配必须赢。`available()` 按 tag 排序,
    /// `en` 恒排在 `en-GB` 前面——两条判据混在一轮里就是 en-GB 永不中选
    #[test]
    fn exact_tag_wins_over_a_shorter_prefix() {
        let all = vec![
            Locale { tag: "en", name: "English" },
            Locale { tag: "en-GB", name: "English (UK)" },
            Locale { tag: "pt", name: "Português" },
            Locale { tag: "pt-BR", name: "Português (Brasil)" },
        ];
        assert_eq!(match_system("en-GB", &all), Some("en-GB"));
        assert_eq!(match_system("en-US", &all), Some("en"));
        assert_eq!(match_system("pt-BR", &all), Some("pt-BR"));
        // 前缀匹配到多个时取最具体的:en-GB-oxendict 属于 en-GB 而不是 en
        assert_eq!(match_system("en-GB-oxendict", &all), Some("en-GB"));
    }

    /// 坏日期模板会让 `dt.format(..).to_string()` panic(Display 返回 Err),
    /// 而外部语言包没有任何一步会先看它一眼——必须在装表时就拦掉
    #[test]
    fn broken_date_patterns_never_reach_the_table() {
        assert!(usable("%b %-d", "%-m月%-d日"));
        assert!(!usable("%b %-d", "%Q"));
        // 散文里的 % 不是日期模板,别误伤
        assert!(usable("{}% vs last week", "较上周 {}%"));
    }

    #[test]
    fn missing_key_falls_back_to_english() {
        assert_eq!(t("Never Translated"), "Never Translated");
    }

    #[test]
    fn placeholders_fill_in_order_and_by_index() {
        assert_eq!(tf("{} of {}", &[&3, &7]), "3 of 7");
        assert_eq!(tf("{1} / {0}", &[&"a", &"b"]), "b / a");
        // 参数不够/不是占位:原样保留,不把界面吃成空白
        assert_eq!(tf("{} and {}", &[&1]), "1 and {}");
        assert_eq!(tf("{name} stays", &[]), "{name} stays");
        assert_eq!(tf("unclosed {", &[]), "unclosed {");
    }

    #[test]
    fn plural_picks_branch_by_count() {
        assert_eq!(tp("{} session", "{} sessions", 1, &[&1]), "1 session");
        assert_eq!(tp("{} session", "{} sessions", 4, &[&4]), "4 sessions");
    }

    #[test]
    fn broken_pack_keeps_the_language_listed() {
        // 坏 JSON 只该让句子退回英文,不该让这门语言从选择器里消失
        assert!(parse("{ not json").is_empty());
        assert!(parse(r#"["array"]"#).is_empty());
        // 非字符串值单独跳过,同一个包里的其余条目照常生效
        let map = parse(r#"{"a": "译", "b": 3}"#);
        assert_eq!(map.get("a").map(String::as_str), Some("译"));
        assert!(!map.contains_key("b"));
    }

    #[test]
    fn bundled_packs_agree_with_their_json() {
        for (tag, name, text) in BUNDLED {
            let map = parse(text);
            assert!(!map.is_empty(), "{tag} is not a flat JSON object");
            // BUNDLED 的自称是为了免解析发现而抄的一份,漂了就是选择器
            // 显示一个名字、用户目录里同一个包显示另一个
            assert_eq!(
                map.get(NAME_KEY).map(String::as_str),
                Some(*name),
                "{tag}: BUNDLED 的自称与 {NAME_KEY} 对不上"
            );
        }
    }

    /// 模板引用到的最高参数下标(顺序 `{}` 也计入),None = 不带参数
    fn max_slot(template: &str) -> Option<usize> {
        let mut next = 0usize;
        let mut max = None;
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else { break };
            let slot = &after[..close];
            let ix = if slot.is_empty() {
                let ix = next;
                next += 1;
                Some(ix)
            } else {
                slot.parse::<usize>().ok()
            };
            if let Some(ix) = ix {
                max = Some(max.map_or(ix, |m: usize| m.max(ix)));
            }
            rest = &after[close + 1..];
        }
        max
    }

    /// 译文引用的参数不能比英文原文提供的多——多出来的那个 `{2}` 不会报错,
    /// 只会原样出现在界面上(校对时最容易漏掉的一类错)
    #[test]
    fn translations_never_reference_missing_arguments() {
        for (tag, _, text) in BUNDLED {
            for (key, value) in parse(text) {
                if key == NAME_KEY {
                    continue;
                }
                let provided = max_slot(&key).map_or(0, |m| m + 1);
                if let Some(used) = max_slot(&value) {
                    assert!(
                        used < provided,
                        "{tag}: 译文 {value:?} 用了第 {used} 个参数,但 {key:?} 只提供 {provided} 个"
                    );
                }
                // 认不出的花括号会原样显示,`tf` 跑一遍就能同时卡住它和落单的 `{`
                let filled = tf_with(&value, provided);
                assert!(
                    !filled.contains('{'),
                    "{tag}: 译文 {value:?} 里有不是占位符的花括号"
                );
            }
        }
    }

    /// 拿 `provided` 个哑参数把模板填满,用于检查有没有填不掉的花括号
    fn tf_with(template: &str, provided: usize) -> String {
        let args: Vec<&dyn std::fmt::Display> = (0..provided).map(|_| &"x" as &dyn std::fmt::Display).collect();
        // tf 走 t() 查表,这里要的是模板本身,故复制那几行的效果:
        // 未装表时 t(key) == key,测试进程从不 load,恒等成立
        tf(Box::leak(template.to_string().into_boxed_str()), &args)
    }

    /// `%` 开头的 key 是 chrono 模板不是散文,译者改的是日期顺序与时制。
    /// `{}` 那套占位符有测试盯着,`%` 指令没有——写错只会渲染出静默的乱码
    #[test]
    fn date_patterns_stay_valid_chrono_formats() {
        use chrono::format::{Item, StrftimeItems};
        for (tag, _, text) in BUNDLED {
            for (key, value) in parse(text) {
                if !key.starts_with('%') {
                    continue;
                }
                for (which, pattern) in [("key", key.as_str()), ("译文", value.as_str())] {
                    assert!(
                        !StrftimeItems::new(pattern).any(|item| item == Item::Error),
                        "{tag}: {which} {pattern:?} 不是合法的 chrono 格式串"
                    );
                }
            }
        }
    }

    /// 语言包里的死 key(英文原文改过、包里那条没跟着改)是静默的:界面
    /// 悄悄回退英文,没人报错。扫一遍源码里的 `t("…")` / `tf!("…"` / `tp!("…"`
    /// 把它们揪出来
    #[test]
    fn packs_have_no_keys_without_a_call_site() {
        const SOURCES: &[&str] = &[
            include_str!("workbench.rs"),
            include_str!("settings.rs"),
            include_str!("main.rs"),
            include_str!("format.rs"),
        ];
        for (tag, _, text) in BUNDLED {
            for key in parse(text).into_keys() {
                if key == NAME_KEY || assembled_elsewhere(&key) || from_wake_core(&key) {
                    continue;
                }
                // 源码里是字面量形式,反斜杠与引号都带着转义
                let literal = format!("\"{}\"", key.replace('\\', r"\\").replace('"', "\\\""));
                assert!(
                    SOURCES.iter().any(|src| src.contains(&literal)),
                    "{tag}: {key:?} 在源码里没有调用点了(英文原文改过?)"
                );
            }
        }
    }

    /// Settings → Connect 把 wake-core 的接入片段在显示边界上过一道 `t()`
    /// (`t(s.hint)` / `t(s.copy_label)`),所以这些 key 的英文原文不在 UI 源码里,
    /// 而在 `setup_snippets` 里。**不要把它们塞进 `assembled_elsewhere`**——
    /// wake-core 不参与 i18n,可以随时改这些字面量,恰恰最需要棘轮看着;
    /// 这里直接问那个 API,改了名就在这里红,而不是变成一条静默的死译文
    fn from_wake_core(key: &str) -> bool {
        wake_core::mcp::setup_snippets(std::path::Path::new("x"))
            .iter()
            .any(|s| s.hint == key || s.copy_label == key)
    }

    /// 文本扫描找不到的 key:ui.rs 由 `concat!` 拼出整句(平台名词只写一次),
    /// 开窗动词由 `t(what)` 间接传入
    fn assembled_elsewhere(key: &str) -> bool {
        const PREFIXES: &[&str] = &[
            "Move to ",
            "Session moved to ",
            "{} sessions moved to ",
            "The session file will be moved to ",
            "Show in ",
            "Reveal in ",
        ];
        PREFIXES.iter().any(|p| key.starts_with(p))
            || matches!(
                key,
                // trash_copy! 的两个宏实参:名词本身与它在正文里的带冠形式
                "Trash" | "Recycle Bin" | "the Recycle Bin"
                // 开窗失败提示里的动词,由 t(what) 间接传入
                | "open" | "reopen"
            )
    }
}
