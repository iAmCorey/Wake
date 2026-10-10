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
use wake_core::db::{self, Store};
use wake_core::mcp::tools::{self, TranscriptCache};
use wake_core::models::{AgentId, Role, SessionFilter, SessionMeta};
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

fn cli_raw(args: &[&str]) -> (String, String, Option<i32>) {
    cli_raw_with(args, |_| {})
}

/// 唯一的 spawn 实现(`with` 改环境与工作目录)。不要 .env_clear():子进程的
/// roster 全靠继承 WAKE_HOME/HOME
fn cli_raw_with(args: &[&str], with: impl FnOnce(&mut Command)) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wake-cli"));
    cmd.args(args);
    with(&mut cmd);
    let out = cmd.output().expect("spawn wake-cli");
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
            argv(&[
                "show",
                CLAUDE_KEY,
                "--subagent",
                "agent-fixture01",
                "--messages",
                "2",
            ]),
            tools::GET_SESSION,
            json!({"key":CLAUDE_KEY,"subagent":"agent-fixture01","max_messages":2}),
        ),
        (
            argv(&["projects", "--since", ALWAYS, "--limit", "10"]),
            tools::LIST_PROJECTS,
            json!({"since":ALWAYS,"limit":10}),
        ),
        (
            argv(&[
                "memories",
                "--project",
                CLAUDE_PROJECT,
                "--agent",
                "claude-code",
                "--limit",
                "10",
            ]),
            tools::LIST_MEMORIES,
            json!({"project":CLAUDE_PROJECT,"agents":["claude-code"],"limit":10}),
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
        (vec!["search", "zzqqxx-no-such-term"], "No session matches"),
        (
            vec!["sessions", "--project", "/nope/nope"],
            "No indexed project matches",
        ),
        (vec!["sessions", "--since", NEVER], "No sessions"),
        (vec!["projects", "--since", NEVER], "No projects"),
        (
            vec!["memories", "--project", "/nope/nope"],
            "No indexed project matches",
        ),
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

/// refresh 用的库副本:主库是全文件共用的只读参照,别的用例正拿它做字节比对,
/// 刷新只能冲着自己那份拷贝跑(扫的仍是共用的 fixture home,只读)
fn db_copy(dir: &Path) -> PathBuf {
    let db = dir.join("copy.db");
    std::fs::copy(&env().db, &db).unwrap();
    db
}

/// 这个功能的主张:app 不开,`refresh` 也把索引追到磁盘的现状。抠掉一行(不留
/// 墓碑,scanner 删磁盘已删的会话用的正是这一手),文件还在,刷新该把它收回来——
/// 不往共用的 fixture home 里加文件,那会让并行的 index 用例数出别的数
#[test]
fn refresh_brings_an_existing_index_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    let db = db_copy(tmp.path());
    let path = db.to_str().unwrap();
    Store::open(&db)
        .unwrap()
        .remove_session(CLAUDE_KEY)
        .unwrap();
    let (_, _, code) = cli_raw(&["--db", path, "show", CLAUDE_KEY, "--messages", "1"]);
    assert_eq!(code, Some(1), "抠掉的会话按 key 该是读不出来的");

    let (stdout, stderr, code) = cli_raw(&["--db", path, "refresh"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stderr, "", "管道里跑,stderr 保持安静");
    assert!(stdout.starts_with("Refreshed the index at "), "{stdout}");
    assert!(stdout.contains(" sessions from "), "{stdout}");

    let (_, stderr, code) = cli_raw(&["--db", path, "show", CLAUDE_KEY, "--messages", "1"]);
    assert_eq!(code, Some(0), "刷新后该收回来:{stderr}");
}

/// 只在 Wake 没运行时干活:GUI 持锁就直说它在跑,另一个 wake-cli 持锁就点名,
/// 两种退让都退 0 且库一个字节不动;锁一放就能刷
#[test]
fn refresh_defers_while_the_index_is_held() {
    let tmp = tempfile::tempdir().unwrap();
    let db = db_copy(tmp.path());
    let path = db.to_str().unwrap();
    let before = std::fs::read(&db).unwrap();
    let refresh = || cli_raw(&["--db", path, "refresh"]);
    for (kind, expect) in [
        (db::LOCK_KIND_APP, "Wake is running"),
        (
            "wake-cli refresh",
            "Another process is writing the index right now (wake-cli refresh ",
        ),
    ] {
        let _lock = common::lock_as(&db, kind);
        let (stdout, stderr, code) = refresh();
        assert_eq!(code, Some(0), "{stderr}");
        assert!(stdout.starts_with(expect), "{stdout}");
        assert_eq!(before, std::fs::read(&db).unwrap(), "退让时一个字节不许动");
    }
    let (stdout, stderr, code) = refresh();
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("Refreshed the index at "), "{stdout}");
}

/// 没索引不是 refresh 的事(`index` 或启动 Wake 才建),与查询命令缺库同退 2,
/// 而且什么都不该建——连锁文件都不留
#[test]
fn refresh_without_an_index_exits_two() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("none.db");
    let (stdout, stderr, code) = cli_raw(&["--db", db.to_str().unwrap(), "refresh"]);
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
    assert!(
        stderr.starts_with("wake-cli: no Wake index at "),
        "{stderr}"
    );
    assert!(
        stderr.contains("wake-cli index"),
        "该指向建库的那条命令:{stderr}"
    );
    let left: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        left.is_empty(),
        "没索引就什么都不该建,连锁文件与持有者旁路都不该留:{left:?}"
    );
}

/// `index` 也认这把锁:GUI 正在启动(下一步就是自己建库)时退让,不建、退 0
#[test]
fn index_defers_while_wake_holds_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fresh.db");
    let _lock = common::lock_as(&db, db::LOCK_KIND_APP);
    let (stdout, stderr, code) = cli_raw(&["--db", db.to_str().unwrap(), "index"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("Wake is running"), "{stdout}");
    assert!(!db.exists(), "退让就不该建库");
}

/// `--db` 指错到别家 SQLite(Cursor 的 state.vscdb 一类):一个字节不许动——别家数据只读是
/// 铁律——退 2 说清楚,连锁文件都不留(Codex review 2026-09-23)
#[test]
fn refresh_refuses_a_database_that_is_not_a_wake_index() {
    let tmp = tempfile::tempdir().unwrap();
    let foreign = tmp.path().join("state.vscdb");
    rusqlite::Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);")
        .unwrap();
    let before = std::fs::read(&foreign).unwrap();
    let (stdout, stderr, code) = cli_raw(&["--db", foreign.to_str().unwrap(), "refresh"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("is not a Wake index"), "{stderr}");
    assert_eq!(
        before,
        std::fs::read(&foreign).unwrap(),
        "别家库一个字节不许动"
    );
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.contains(".lock")), "{names:?}");
}

/// 有远程会话入库、镜像目录却不在这条路径旁 = 给的不是 GUI 开它的那条路径(真库的别名,
/// 或被链接到别处的真库本身):照扫会把远程会话整批当"磁盘已删"清掉,拒掉、退 2、行还在
/// (Codex review 2026-09-23)
#[test]
fn refresh_refuses_a_path_whose_remote_mirrors_are_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let db = db_copy(tmp.path());
    Store::open(&db).unwrap().add_remote_host("box").unwrap();
    // 直接塞一行远程会话:这条路径旁没有 remotes/box
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO sessions(key, agent_id, native_id, host, file_path) \
             VALUES ('codex:box:abc', 'codex', 'abc', 'box', '/elsewhere/remotes/box/x.jsonl')",
            [],
        )
        .unwrap();
    let (stdout, stderr, code) = cli_raw(&["--db", db.to_str().unwrap(), "refresh"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("mirrors are not next to it"), "{stderr}");
    let left: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE host = 'box'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 1, "远程会话不许被清掉");
}

/// 同上,但这台 host 只剩远程记忆、没有会话行(转录清掉了、Codex 记忆还在):镜像不在照样
/// 拒,否则 `sync_memories` 会把它的记忆当消失的来源整组删掉(Codex review 2026-09-23)
#[test]
fn refresh_refuses_when_only_remote_memories_would_be_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let db = db_copy(tmp.path());
    Store::open(&db).unwrap().add_remote_host("box").unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO memories(key, agent_id, host, scope, path, title, body) \
             VALUES ('codex:box:/elsewhere/m.md', 'codex', 'box', 'user', '/elsewhere/m.md', 'm', 'x')",
            [],
        )
        .unwrap();
    let (stdout, stderr, code) = cli_raw(&["--db", db.to_str().unwrap(), "refresh"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("mirrors are not next to it"), "{stderr}");
    let left: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE host = 'box'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 1, "远程记忆不许被清掉");
}

/// `context` 给 SessionStart 钩子用:当前目录所在项目最近两周、各个 agent 停在哪(最后一问、
/// 最后一答),子目录归到所在项目。每个 agent 先各取一条再按时间补满五条——常用的那一家
/// 不能把别家挤出去;一句提问都没有的会话不列。认不出项目、两周内没有会话时一个字节都不
/// 输出、退 0——钩子的输出原样进 agent 的上下文。家目录不当"所在项目":在家目录随手开的
/// 会话不该被塞进每个新项目
#[test]
fn context_shows_where_each_agent_left_off_or_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = common::real_dir(tmp.path());
    let project = home.join("app");
    let fresh = home.join("fresh");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir_all(&fresh).unwrap();
    let db = home.join("wake.db");
    {
        let store = Store::open(&db).unwrap();
        let now = db::now_ms();
        let write = |key: &str, at: &Path, minutes_ago: i64, turns: &[(Role, &str)]| {
            let updated_at = now - minutes_ago * 60_000;
            let meta = SessionMeta {
                agent: AgentId::from_str(key.split(':').next().unwrap()).unwrap(),
                project_path: at.to_string_lossy().into_owned(),
                created_at: updated_at,
                updated_at,
                ..common::meta(key, &format!("about {key}"))
            };
            let units: Vec<_> = turns
                .iter()
                .enumerate()
                .map(|(seq, (role, text))| common::unit(seq as i64, *role, text))
                .collect();
            store.write_session(&meta, updated_at, &units).unwrap();
        };
        // Claude Code 五条都比 Codex 那条新,只按时间挑 Codex 就进不来。每条最后一个用户单元
        // 是 `/model` 一类命令的输出——不是人说的话,最后一问要往前找到真正的提问
        for n in 1..=5 {
            write(
                &format!("claude-code:c{n}"),
                &project,
                n,
                &[
                    (Role::User, &format!("ask c{n}")),
                    (Role::Assistant, &format!("did c{n}\nmore")),
                    (
                        Role::User,
                        "<local-command-stdout>Set model to opus</local-command-stdout>",
                    ),
                ],
            );
        }
        write(
            "codex:review",
            &project,
            2 * 24 * 60,
            &[
                (
                    Role::User,
                    "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>--base main</command-args>",
                ),
                (Role::Assistant, "Looks good overall"),
                // 只有工具调用的消息:单元以换行开头,不算回复
                (Role::Assistant, "\nBash git diff main"),
            ],
        );
        // 一句提问都没有(别的 agent 代跑、刚开还没说话)
        write("codex:silent", &project, 0, &[(Role::Assistant, "working")]);
        write(
            "claude-code:stale",
            &project,
            30 * 24 * 60,
            &[(Role::User, "old")],
        );
        write("claude-code:in-home", &home, 0, &[(Role::User, "hi")]);
    }
    let context = |dir: &Path| {
        let (out, _, code) = cli_raw_with(&["--db", db.to_str().unwrap(), "context"], |cmd| {
            cmd.env("WAKE_HOME", &home).current_dir(dir);
        });
        (out, code)
    };
    let (text, code) = context(&project.join("src"));
    assert_eq!(code, Some(0));
    assert!(
        text.starts_with(
            "Wake: recent sessions in this project, across your coding agents (5 of 7 from the last 14 days):"
        ),
        "{text}"
    );
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} 不在: {text}"))
    };
    // 按时间排:c1 最新,Codex 那条最旧
    assert!(
        at("`claude-code:c1`") < at("`claude-code:c4`")
            && at("`claude-code:c4`") < at("`codex:review`")
    );
    assert!(
        !text.contains("claude-code:c5"),
        "补满五条,c5 排不上: {text}"
    );
    assert!(
        text.contains("last asked: \"ask c1\"\n  last reply: \"did c1\"\n"),
        "{text}"
    );
    assert!(
        text.contains("last asked: \"/review --base main\""),
        "slash 命令写回原样: {text}"
    );
    assert!(
        text.contains("last reply: \"Looks good overall\""),
        "工具调用不算回复: {text}"
    );
    assert!(!text.contains("codex:silent"), "没有提问的会话不列: {text}");
    assert!(!text.contains("stale"), "两周以前的不列: {text}");
    assert!(!text.contains("in-home"), "家目录的会话不跟进来: {text}");

    let elsewhere = tempfile::tempdir().unwrap();
    for dir in [fresh.as_path(), elsewhere.path()] {
        assert_eq!(context(dir), (String::new(), Some(0)), "{}", dir.display());
    }
}

/// 插件只是一层壳:清单、钩子、MCP 配置都指向仓库里真实存在的脚本,钩子调的是 `wake-cli
/// context`,启动脚本找的正是各平台打包落下二进制的地方。Claude Code 与 Codex 共用一个插件
/// 目录,各读各的清单与市场文件。`claude plugin validate` 与 Codex 都不在 CI 里跑,这里卡住
/// 最容易漂的几处
#[test]
fn the_plugins_point_at_real_files() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let json = |path: PathBuf| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    // 安装命令最后一段 `<插件>@<市场>` 与市场文件对得上
    let install_id = |market: &Value| {
        format!(
            "{}@{}",
            market["plugins"][0]["name"].as_str().unwrap(),
            market["name"].as_str().unwrap()
        )
    };
    let market = json(repo.join(".claude-plugin/marketplace.json"));
    let entry = &market["plugins"][0];
    assert!(cli::CLAUDE_PLUGIN_INSTALL.ends_with(&install_id(&market)));
    let plugin = repo.join(entry["source"].as_str().unwrap());
    let manifest = json(plugin.join(".claude-plugin/plugin.json"));
    assert_eq!(manifest["name"], entry["name"]);

    // 钩子两家共用,整份钉死:Codex 按这份定义(matcher、命令、超时)认信任,改一个字所有
    // Codex 用户的钩子就停了、要去 /hooks 重新信任——行为上的改动放进 scripts/session-start
    assert_eq!(
        json(plugin.join("hooks/hooks.json")),
        json!({ "hooks": { "SessionStart": [{
            "matcher": "startup|clear",
            "hooks": [{
                "type": "command",
                "command": "sh \"${CLAUDE_PLUGIN_ROOT}/scripts/session-start\"",
                "timeout": 30
            }]
        }]}})
    );
    let start = std::fs::read_to_string(plugin.join("scripts/session-start")).unwrap();
    assert!(start.contains("wake-cli context"), "{start}");

    // MCP 两家各写一份:Claude Code 的 stdio 配置没有 cwd、插件路径靠 ${CLAUDE_PLUGIN_ROOT};
    // Codex 不展开 MCP 参数里的变量(只展开钩子命令),写相对路径 + cwd "."(插件根,Codex
    // 自带的插件也这么写),直接放在清单里。Codex 只给 MCP 子进程几个默认环境变量(PATH、
    // HOME 这些),启动脚本认的 WAKE_BIN_DIR 要点名放行,不然 Wake 装在别处的人钩子能用、
    // MCP 却找不到 wake-mcp
    let mcp = json(plugin.join(".mcp.json"));
    let args = &mcp["mcpServers"]["wake"]["args"];
    assert_eq!(args[0], "${CLAUDE_PLUGIN_ROOT}/scripts/wake");
    assert_eq!(args[1], "wake-mcp");
    let codex_market = json(repo.join(".agents/plugins/marketplace.json"));
    let codex_entry = &codex_market["plugins"][0];
    assert!(cli::CODEX_PLUGIN_INSTALL.ends_with(&install_id(&codex_market)));
    assert_eq!(
        codex_entry["source"]["path"], entry["source"],
        "同一个插件目录"
    );
    let codex_manifest = json(plugin.join(".codex-plugin/plugin.json"));
    assert_eq!(codex_manifest["name"], codex_entry["name"]);
    // Codex 按版本缓存装好的插件,版本不变就不重装:跟 Wake 一起走,发版改 Cargo.toml 时一并改
    assert_eq!(codex_manifest["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        codex_manifest["mcpServers"]["wake"],
        json!({
            "command": "sh",
            "args": ["./scripts/wake", "wake-mcp"],
            "cwd": ".",
            "env_vars": ["WAKE_BIN_DIR"]
        })
    );
    for field in ["logo", "composerIcon"] {
        let asset = codex_manifest["interface"][field].as_str().unwrap();
        assert!(plugin.join(asset).is_file(), "{field}: {asset}");
    }
    // 文档里的安装命令就是 Connect 页复制的那条
    for doc in ["README.md", "docs/cli.md", "docs/mcp.md"] {
        let text = std::fs::read_to_string(repo.join(doc)).unwrap();
        for command in [cli::CLAUDE_PLUGIN_INSTALL, cli::CODEX_PLUGIN_INSTALL] {
            assert!(
                text.contains(command),
                "{doc} 里的安装命令过时了: {command}"
            );
        }
    }

    let launcher = std::fs::read_to_string(plugin.join("scripts/wake")).unwrap();
    assert!(launcher.contains("/Applications/Wake.app/Contents/MacOS"));
    // tar 包装进 ~/.local/bin、deb 装进 /usr/bin
    let linux = std::fs::read_to_string(repo.join("scripts/make-linux.sh")).unwrap();
    for dir in ["$HOME/.local/bin", "/usr/bin"] {
        assert!(launcher.contains(dir), "启动脚本没找 {dir}");
        assert!(
            linux.contains(&format!("{dir}/wake-cli")),
            "Linux 打包不再落在 {dir}"
        );
    }
}
