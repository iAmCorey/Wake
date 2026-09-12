//! wake-cli 的端到端契约:合成 fixtures 建临时库 → 起真实 bin → 与进程内
//! `mcp::tools::invoke` 同参数的输出**逐字节**比对(MCP server 跑的正是这个
//! 入口)。docs 与将来的 Skill 因此只需描述一份格式。
//!
//! 参照那一侧故意喂**自然形态**的 JSON(`5` / `true` / `["claude"]`),CLI 那
//! 侧喂字符串与逗号串——同一条断言于是顺带卡住 int_arg 认字符串、agents_arg
//! 认逗号串这两处宽容:谁把它们收紧了,当天就红,而不是 CLI 悄悄开始拒数字。
//!
//! WAKE_HOME/HOME 是进程级环境且子进程继承它:fixture home 只建一次
//! (OnceLock)、全文件共用;不碰 home 的用例(缺库 / --help)照常并行。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};
use wake_core::adapters::{create_adapter_roster_for, create_adapters_for, AgentAdapter};
use wake_core::cli;
use wake_core::db::Store;
use wake_core::mcp::tools::{self, TranscriptCache};
use wake_core::models::SessionFilter;
use wake_core::scanner::{run_scan, NullEvents};

mod common;

const CLAUDE_KEY: &str = "claude-code:11111111-aaaa-bbbb-cccc-000000000001";
const CLAUDE_PROJECT: &str = "/Users/tester/Github/wakefx";
/// 绝对日期,不用 7d 这类相对量:fs::copy 在 Linux 上不保留 mtime,fixture 在
/// 那边全是"刚刚",相对窗口两平台结果不同(2026-09-08 让 CI 红过一次)
const ALWAYS: &str = "2000-01-01";
const NEVER: &str = "2999-01-01";

struct TestEnv {
    db: PathBuf,
    _home: tempfile::TempDir, // TempDir 必须被持有,否则整进程期间被清掉
}

static ENV: OnceLock<TestEnv> = OnceLock::new();

fn env() -> &'static TestEnv {
    ENV.get_or_init(|| {
        let tmp = tempfile::Builder::new()
            .prefix("wake-cli-e2e-")
            .tempdir()
            .unwrap();
        let home = tmp.path().join("home");
        common::stage_dir_fixtures(&home);
        common::stage_sidecars(&home);
        common::isolate_home(&home); // 必须在任何 adapter 构造之前
        let db = tmp.path().join("wake.db");
        {
            let store = Arc::new(Store::open(&db).unwrap());
            let roster = create_adapter_roster_for(&store);
            run_scan(&roster.active, &store, &NullEvents, true).expect("scan ok");
            let (_, total) = store
                .list_sessions(&SessionFilter {
                    limit: 1,
                    ..Default::default()
                })
                .unwrap();
            assert!(
                total > 10,
                "fixture home should index many sessions ({total})"
            );
        } // 块尾关写连接:最后一个连接 checkpoint,-wal/-shm 消失,之后
          // open_read_only 与字节快照才稳定
        TestEnv { db, _home: tmp }
    })
}

/// 参照侧:与 bin 同一条打开路径、同一份 roster
fn store_and_roster() -> (Store, Vec<Box<dyn AgentAdapter>>) {
    let store = Store::open_read_only(&env().db).expect("read-only open");
    let adapters = create_adapters_for(&store);
    (store, adapters)
}

fn call(
    store: &Store,
    adapters: &[Box<dyn AgentAdapter>],
    tool: &str,
    args: &Value,
) -> Result<String, tools::ToolError> {
    tools::invoke(store, adapters, &TranscriptCache::default(), tool, args)
}

/// 唯一的 spawn 实现。不要 .env_clear():子进程的 roster 全靠继承
/// WAKE_HOME/HOME
fn cli_raw(args: &[&str]) -> (String, String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_wake-cli"))
        .args(args)
        .output()
        .expect("spawn wake-cli");
    (
        String::from_utf8(out.stdout).expect("stdout is utf-8"),
        String::from_utf8(out.stderr).expect("stderr is utf-8"),
        out.status.code(),
    )
}

/// 冲着 fixture 库跑
fn cli_run(args: &[&str]) -> (String, String, Option<i32>) {
    let db = env().db.to_str().expect("tmp path is utf-8");
    cli_raw(&[&["--db", db], args].concat())
}

/// argv 跑出来的东西必须与同参数的工具文本逐字节相同
fn same(
    (store, adapters): &(Store, Vec<Box<dyn AgentAdapter>>),
    argv: &[&str],
    tool: &str,
    natural: Value,
) {
    let text = call(store, adapters, tool, &natural).expect("tool ok");
    // CLI 最多补一个换行:search/sessions/projects 收在 index_note 上、无换行,
    // get_session 自带。用算出来的期望值精确相等,不要 trim_end——那会把工具
    // 文本自带的尾换行一起吃掉、把差异藏起来
    let want = if text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    };
    let (stdout, stderr, code) = cli_run(argv);
    assert_eq!(code, Some(0), "argv {argv:?} 应当成功: {stderr}");
    assert_eq!(stderr, "", "argv {argv:?} 跑通时 stderr 必须一个字节都没有");
    assert_eq!(stdout, want, "argv {argv:?} 必须原样吐工具文本");
}

/// 每条 argv 与它对应的自然形态参数。测试 2 会走这张表核对旗标覆盖率
fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn cases() -> Vec<(Vec<String>, &'static str, Value)> {
    let reference = format!("wake://session/{CLAUDE_KEY}#3");
    vec![
        (
            argv(&["search", "二维码", "--limit", "3"]),
            tools::SEARCH,
            json!({"query":"二维码","limit":3}),
        ),
        (
            argv(&[
                "search",
                "二维码",
                "--project",
                CLAUDE_PROJECT,
                "--agent",
                "claude",
                "--since",
                ALWAYS,
                "--limit",
                "2",
            ]),
            tools::SEARCH,
            json!({"query":"二维码","project":CLAUDE_PROJECT,"agents":["claude"],"since":ALWAYS,"limit":2}),
        ),
        (
            argv(&["sessions", "--limit", "5"]),
            tools::LIST_SESSIONS,
            json!({"limit":5}),
        ),
        (
            argv(&[
                "sessions",
                "--project",
                CLAUDE_PROJECT,
                "--agent",
                "claude,codex",
                "--starred",
                "--since",
                ALWAYS,
                "--limit",
                "3",
            ]),
            tools::LIST_SESSIONS,
            json!({"project":CLAUDE_PROJECT,"agents":["claude","codex"],"starred":true,
                   "since":ALWAYS,"limit":3}),
        ),
        (
            argv(&[
                "show",
                CLAUDE_KEY,
                "--from",
                "1",
                "--messages",
                "2",
                "--chars",
                "5000",
                "--message-chars",
                "500",
                "--tools",
                "--thinking",
            ]),
            tools::GET_SESSION,
            json!({"key":CLAUDE_KEY,"from_seq":1,"max_messages":2,"max_chars":5000,
                   "max_message_chars":500,"include_tools":true,"include_thinking":true}),
        ),
        (
            argv(&["show", &reference]),
            tools::GET_SESSION,
            json!({ "key": reference }),
        ),
        (
            argv(&["projects", "--since", ALWAYS, "--limit", "10"]),
            tools::LIST_PROJECTS,
            json!({"since":ALWAYS,"limit":10}),
        ),
    ]
}

#[test]
fn every_command_prints_the_tool_text_verbatim() {
    let reference = store_and_roster();
    for (args, tool, natural) in cases() {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        same(&reference, &args, tool, natural);
    }
}

/// 每个命令、每个旗标都被上面那张表真正跑过一次。与 cli.rs 里的双射单测合起
/// 来,"工具新增参数不可能未经测试地发版"才是真的,不是口号
#[test]
fn every_command_and_flag_is_exercised_end_to_end() {
    let cases = cases();
    for c in cli::COMMANDS {
        let used: Vec<&Vec<String>> = cases
            .iter()
            .filter(|(argv, _, _)| argv.first().map(String::as_str) == Some(c.name))
            .map(|(argv, _, _)| argv)
            .collect();
        assert!(!used.is_empty(), "命令 {} 没有端到端用例", c.name);
        for f in c.flags {
            assert!(
                used.iter().any(|argv| argv.iter().any(|a| a == f.long)),
                "{} 的 {} 没有端到端用例",
                c.name,
                f.long
            );
        }
    }
}

/// 退出码那道决定的全部:argv 敲错与工具认为值不对,对用户是同一件事,都退 2
#[test]
fn bad_arguments_exit_two_and_name_the_problem() {
    for (argv, needle) in [
        (
            vec!["sessions", "--since", "yesterday"],
            "`since` not understood",
        ),
        (vec!["sessions", "--agent", "chatgpt"], "unknown agent"),
        (vec!["sessions", "--limit", "abc"], "must be an integer"),
        (vec!["nope"], "unknown command"),
        (vec!["sessions", "--nope"], "unknown option"),
        (vec!["sessions", "--starred=false"], "takes no value"),
        (vec!["search"], "needs a QUERY"),
        (vec!["show"], "needs a KEY"),
        (
            vec!["search", "x", "--starred"],
            "only valid for `sessions`",
        ),
        (vec!["sessions", "--project", "."], "$PWD"),
        (vec!["sessions", "myproj"], "takes no arguments"),
    ] {
        let (stdout, stderr, code) = cli_run(&argv);
        assert_eq!(code, Some(2), "argv {argv:?} 应当退 2: {stderr}");
        assert!(stdout.is_empty(), "argv {argv:?} 出错时 stdout 必须为空");
        assert!(
            stderr.contains(needle),
            "argv {argv:?} 的 stderr 应当提到 {needle:?},实际是 {stderr}"
        );
        assert!(stderr.starts_with("wake-cli: "), "{stderr}");
    }
    // 缺命令走的是另一条:parse 直接给 "needs a command"
    let (_, stderr, code) = cli_run(&[]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("needs a command"), "{stderr}");
}

/// 读不出来的会话是"跑起来了然后失败",退 1,且文本与 MCP 那侧同源
#[test]
fn unreadable_session_exits_one() {
    let (store, adapters) = store_and_roster();
    let want = match call(
        &store,
        &adapters,
        tools::GET_SESSION,
        &json!({"key":"claude-code:nope"}),
    ) {
        Err(tools::ToolError::Failed(m)) => m,
        other => panic!("坏 key 应当是 ToolError::Failed,实际 {other:?}"),
    };
    let (stdout, stderr, code) = cli_run(&["show", "claude-code:nope"]);
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty());
    assert_eq!(stderr, format!("wake-cli: {want}\n"));
}

/// 空结果不是错误:`wake-cli search x || fallback` 不能因为"这事没讨论过"就触发
#[test]
fn no_results_is_success() {
    for (argv, prefix) in [
        (vec!["search", "zzqqxx-no-such-term"], "No matches for"),
        (
            vec!["sessions", "--project", "/nope/nope"],
            "No indexed project matches",
        ),
        (vec!["sessions", "--since", NEVER], "No sessions"),
        (vec!["projects", "--since", NEVER], "No projects"),
    ] {
        let (stdout, stderr, code) = cli_run(&argv);
        assert_eq!(code, Some(0), "argv {argv:?}: {stderr}");
        assert!(stdout.starts_with(prefix), "argv {argv:?} 输出是 {stdout}");
    }
    let (stdout, _, code) = cli_run(&["show", CLAUDE_KEY, "--from", "9999"]);
    assert_eq!(code, Some(0));
    assert!(
        stdout.contains("No messages at or after seq 9999"),
        "{stdout}"
    );
}

/// 超范围的数字由 int_arg 裁剪,不是 CLI 拒绝——CLI 若自己校验就会与 MCP 分家
#[test]
fn out_of_range_limits_are_clamped_not_rejected() {
    let reference = store_and_roster();
    same(
        &reference,
        &["sessions", "--limit", "999"],
        tools::LIST_SESSIONS,
        json!({"limit":100}),
    );
    same(
        &reference,
        &["search", "二维码", "--limit", "0"],
        tools::SEARCH,
        json!({"query":"二维码","limit":1}),
    );
}

/// 这三条不碰 fixture home,与别的用例并行也安全:--help/--version 根本不开库,
/// 缺库那条在建 roster 之前就退了,而且都显式带 --db,不会触发
/// default_db_path() 的首次迁移副作用
#[test]
fn missing_index_and_help_need_no_fixture_home() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.db");
    let (stdout, stderr, code) = cli_raw(&["--db", missing.to_str().unwrap(), "sessions"]);
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty());
    assert!(stderr.contains("launch Wake once"), "{stderr}");

    let (stdout, stderr, code) = cli_raw(&["--help"]);
    assert_eq!(code, Some(0));
    assert_eq!(stderr, "");
    assert_eq!(stdout, format!("{}\n", cli::help()));

    let (stdout, _, code) = cli_raw(&["--version"]);
    assert_eq!(code, Some(0));
    assert_eq!(stdout, format!("wake-cli {}\n", env!("CARGO_PKG_VERSION")));
}

/// setup 不是工具调用,上面那张 cases 表结构上覆盖不到它
#[test]
fn setup_reports_the_binary_and_never_fails() {
    let (stdout, stderr, code) = cli_run(&["setup"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("wake-cli binary: "), "{stdout}");
    assert!(stdout.contains(env!("CARGO_BIN_EXE_wake-cli")), "{stdout}");
    assert!(stdout.contains("--project \"$PWD\""), "{stdout}");
    // 库缺失也照样退 0:"还没装好"正是跑 setup 的人的处境
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.db");
    let (stdout, _, code) = cli_raw(&["--db", missing.to_str().unwrap(), "setup"]);
    assert_eq!(code, Some(0));
    assert!(
        stdout.trim_end().ends_with("launch Wake once to build it"),
        "{stdout}"
    );
}

/// Linux 上目录名可以不是 UTF-8,而 `--project "$PWD"` 正是主用法:
/// std::env::args() 会 panic 成 101,args_os 才能给出契约内的退出码
#[cfg(unix)]
#[test]
fn non_utf8_arguments_exit_two_instead_of_panicking() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let out = Command::new(env!("CARGO_BIN_EXE_wake-cli"))
        .arg("sessions")
        .arg("--project")
        .arg(OsStr::from_bytes(b"/tmp/caf\xe9"))
        .output()
        .expect("spawn wake-cli");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("wake-cli: argument is not valid UTF-8"),
        "{stderr}"
    );
}

/// SKILL.md 是 CLI 的第二张脸:CLI 改了而它没跟上,agent 就会照着过时的说明
/// 去敲命令。frontmatter 齐、四个子命令全提到、退出码那条约定在,少一样就红
#[test]
fn the_skill_stays_in_sync_with_the_cli() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/wake/SKILL.md")
        .canonicalize()
        .expect("skills/wake/SKILL.md 必须存在——它是 `npx skills add` 装的东西");
    let md = std::fs::read_to_string(&path).expect("read SKILL.md");

    // frontmatter:name 与 description 是懒加载时常驻上下文的那两行。
    // **一律按 `lines()` 切**(它同时吃 \n 与 \r\n):仓库没有 .gitattributes,
    // Windows CI 按 core.autocrlf=true 检出的 SKILL.md 是 CRLF,硬比 "---\n"
    // 会让这条测试在三平台里只红一个
    let mut lines = md.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "SKILL.md 要以 YAML frontmatter 开头"
    );
    let front: Vec<&str> = lines.by_ref().take_while(|l| *l != "---").collect();
    assert!(lines.next().is_some(), "frontmatter 要闭合,后面还要有正文");
    assert!(front.contains(&"name: wake"), "frontmatter 缺 name");
    let desc = front
        .iter()
        .find_map(|l| l.strip_prefix("description: "))
        .expect("frontmatter 缺单行的 `description: …`");
    // description 是触发条件,不是名词解释——没有 "Use when" 就等于没有触发器
    assert!(
        desc.contains("Use when"),
        "description 要写成触发条件: {desc}"
    );

    for c in cli::COMMANDS {
        assert!(
            md.contains(&format!("wake-cli {}", c.name)),
            "SKILL.md 没有 `wake-cli {}` 的例子",
            c.name
        );
    }
    // 最容易被误读的两条约定
    assert!(
        md.contains("--project \"$PWD\""),
        "SKILL.md 要交代 --project 的约定"
    );
    assert!(md.contains("wake://session/"), "SKILL.md 要交代引用格式");
    assert!(md.contains("read-only"), "SKILL.md 要声明只读");
}

/// 一旦有人把 open_or_rebuild 摸进来,或者把 `index` 那道口子放宽到已存在的
/// 库上,这条就红
#[test]
fn the_cli_never_writes_an_index_that_exists() {
    let db = &env().db;
    let before = std::fs::read(db).unwrap();
    for argv in [
        vec!["sessions", "--limit", "3"],
        vec!["search", "二维码"],
        vec!["show", CLAUDE_KEY],
        vec!["projects"],
        // index 对**已存在**的库同样不许动一个字节——那道口子只开给"库不存在"
        vec!["index"],
    ] {
        let (_, stderr, code) = cli_run(&argv);
        assert_eq!(code, Some(0), "{stderr}");
    }
    assert!(
        before == std::fs::read(db).unwrap(),
        "wake-cli must never write an index that already exists"
    );
}

/// 这条口子存在的唯一理由:装了 Wake 但从没启动过。库不存在时建一次、建完
/// 立刻可查;库已存在就退让(字节不变那一半由上面那条卡)。
///
/// **先 `env()` 再 spawn**:它建 fixture home 并钉住 WAKE_HOME/HOME 给子进程
/// 继承。不走这一步,子进程扫的就是维护者真实的家目录——本机十秒、还把真实
/// 索引整个拷进 tempdir,而 CI 上家目录是空的,`starts_with("Indexed ")` 对
/// "Indexed 0 sessions" 照样为真,整条用例空过
#[test]
fn index_builds_one_from_scratch_then_defers() {
    let want = Store::open_read_only(&env().db)
        .unwrap()
        .list_sessions(&SessionFilter {
            limit: 1,
            ..Default::default()
        })
        .unwrap()
        .1;
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fresh.db");
    // 主库不在、边车还躺着(主库被手删,或 open_or_rebuild 崩在挪库与删边车
    // 之间):留下任何一个,新库都会接着读旧日志
    for suffix in ["-wal", "-shm"] {
        std::fs::write(format!("{}{suffix}", db.display()), b"stale").unwrap();
    }

    let (stdout, stderr, code) = cli_raw(&["--db", db.to_str().unwrap(), "index"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(db.is_file(), "应当真的建出库来");
    // 扫描落在 <db>.build-<pid>;占位成功后那个名字必须收干净,不然一次运行就
    // 在用户的索引目录里留下几百 MB
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains(".build-")),
        "临时库没收干净:{names:?}"
    );
    // 孤儿边车必须被清掉。之后的只读连接会自己再建一个 -shm,所以判据是
    // "还在不在" 之外那一条:planted 的内容不许活下来
    for suffix in ["-wal", "-shm"] {
        let p = format!("{}{suffix}", db.display());
        if let Ok(bytes) = std::fs::read(&p) {
            assert_ne!(bytes, b"stale", "{p} 还是那份孤儿日志");
        }
    }
    // 这个功能的主张就是这一句:CLI 自己建的索引 == GUI 从同一棵树建的那个
    assert!(
        stdout.starts_with(&format!("Indexed {want} session")),
        "应当与 fixture 库同样收录 {want} 条:{stdout}"
    );

    // 建完就能查,不必先启动 GUI
    let (stdout, _, code) = cli_raw(&["--db", db.to_str().unwrap(), "projects"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains(CLAUDE_PROJECT), "{stdout}");

    // 第二次是退让,不是重建
    let (stdout, _, code) = cli_raw(&["--db", db.to_str().unwrap(), "index"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("An index already exists"), "{stdout}");
}
