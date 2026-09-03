//! 远程同步的端到端(假 ssh + 真 rsync + 真实 roster/scanner):
//!
//! 卡 2026-09-02 本机实测出的两个 bug——①缺父目录的源让 openrsync 发送端
//! 截断文件列表、后面的源静默不传;②任何缺源都让 rsync 跳过 --delete。修法
//! 是先探测再镜像,这里断言的就是"远端缺一大半 agent 时,存在的照样全到、
//! 删除照样传播",再接上 `create_adapter_roster_for` + `run_scan`,确认远程
//! 会话以 `{agent}:{host}:{native}` 入库、Remove host 后按"磁盘已删"出清。
//!
//! 假 ssh 把远端命令交给 sh 在假 home 里跑,与真 ssh 把命令串交给远端登录
//! shell 的行为同形;rsync 用系统真身(macOS 是 openrsync,CI ubuntu 是 GNU
//! rsync,两种发送端都覆盖)。Windows 无 sh/rsync,整文件不编译。
//! PATH/WAKE_HOME 是进程级环境:默认只跑一个用例(live 那个默认 ignore、
//! 手动单跑),不会有两个线程同时改 env。
#![cfg(unix)]

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use wake_core::adapters::create_adapter_roster_for;
use wake_core::db::{RemoteHost, Store};
use wake_core::models::{AgentId, SessionFilter};
use wake_core::scanner::{run_scan, NullEvents};

fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn touch(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn host_row(store: &Store, name: &str) -> RemoteHost {
    store
        .list_remote_hosts()
        .unwrap()
        .into_iter()
        .find(|h| h.name == name)
        .unwrap_or_else(|| panic!("remote host {name} not configured"))
}

mod common;

/// 把合成 fixtures 摆成一台远程机器的 home(与契约测试同一份搭建),再把
/// Copilot 整个拿掉:`.copilot` 是白名单里唯一"连父目录都不存在"的形状,
/// 且排在 cursor…dsh 之前——正是截断 bug 的触发条件,也是"远端只装了一部分
/// agent"的常态。
fn build_remote_home(home: &Path) {
    common::stage_dir_fixtures(home);
    common::stage_sidecars(home);
    fs::remove_dir_all(home.join(".copilot")).unwrap();
    // 凭证在白名单外,必须留在远端
    touch(&home.join(".codex/auth.json"), "NOT-A-REAL-SECRET");
}

/// 假远端上装着的 agent(Copilot 故意缺席,见 build_remote_home)
fn remote_agents() -> impl Iterator<Item = AgentId> {
    AgentId::ALL.into_iter().filter(|a| *a != AgentId::Copilot)
}

const CLAUDE_U1: &str =
    ".claude/projects/-Users-tester-Github-wakefx/11111111-aaaa-bbbb-cccc-000000000001.jsonl";
const CODEX_R2: &str =
    ".codex/sessions/2026/08/02/rollout-2026-08-02T09-15-00-22222222-aaaa-bbbb-cccc-000000000002.jsonl";
const GROK_S7: &str =
    ".grok/sessions/%2FUsers%2Ftester%2FGithub%2Fwakefx/77777777-aaaa-bbbb-cccc-000000000007";
const DSH_LOG: &str =
    ".dsh/sessions/--Users-tester-Github-wakefx--/dsh-e2e4-0001/session.jsonl.zstd";

#[test]
fn remote_pipeline_end_to_end() {
    if std::process::Command::new("rsync")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("rsync not installed; skipping remote sync e2e");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("remote-home");
    let bin = tmp.path().join("bin");
    let local_home = tmp.path().join("local-home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&local_home).unwrap();
    // 假 ssh:吞掉 -o 选项与 host;host 为 down 模拟连不上(ssh 的 255);
    // 其余把拼接后的远端命令交给 sh 在假 home 里执行——探测脚本与 rsync 的
    // --server 都走这条路。WAKE_TEST_VANISH 指向一个文件时,第一次
    // rsync --server 起跑前把它删掉(只删一次,.done 标记),模拟"探测到
    // rsync 之间远端 agent 退出删了 -wal"的竞争
    write_exec(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do case \"$1\" in -*) shift;; *) break;; esac; done\n\
             host=$1; shift\n\
             if [ \"$host\" = down ]; then \
               echo 'ssh: connect to host down port 22: Connection refused' >&2; exit 255; \
             fi\n\
             if [ -n \"$WAKE_TEST_VANISH\" ] && [ ! -e \"$WAKE_TEST_VANISH.done\" ]; then \
               case \"$*\" in *'rsync --server'*) rm -f \"$WAKE_TEST_VANISH\"; : > \"$WAKE_TEST_VANISH.done\";; esac; \
             fi\n\
             export HOME='{home}'\n\
             cd \"$HOME\" || exit 99\n\
             exec sh -c \"$*\"\n",
            home = home.display()
        ),
    );
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    // 本地 roster 指向一个空 home:库里出现的只能是远程行,也不会读到这台
    // 开发机的真实数据;env 根覆盖一并清掉
    std::env::set_var("WAKE_HOME", &local_home);
    std::env::set_var("HOME", &local_home);
    for var in [
        "XDG_DATA_HOME",
        "CODEX_HOME",
        "QODER_CONFIG_DIR",
        "HERMES_HOME",
        "OPENCLAW_STATE_DIR",
    ] {
        std::env::remove_var(var);
    }
    build_remote_home(&home);

    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    store.add_remote_host("devbox").unwrap();
    store.add_remote_host("down").unwrap();
    let db_dir = store.db_dir().unwrap();
    let cache = wake_core::remote::host_cache_dir(&db_dir, "devbox");
    let devbox = || host_row(&store, "devbox");

    // ① 首次同步:.copilot(缺父目录)之后的 cursor…dsh 必须到位,exclude 与凭证
    // 不进缓存。同时让 opencode-next.db 在探测之后、rsync 之前消失:rsync 报
    // 只含 ENOENT 的 23,sync_host 必须重探一轮并成功,而不是记成 Sync failed
    let vanish = home.join(".local/share/opencode/opencode-next.db");
    let vanish_marker = Path::new(&format!("{}.done", vanish.display())).to_path_buf();
    std::env::set_var("WAKE_TEST_VANISH", &vanish);
    wake_core::remote::sync_hosts(&store, &["devbox".to_string()]);
    std::env::remove_var("WAKE_TEST_VANISH");
    assert!(vanish_marker.exists(), "假 ssh 没触发消失钩子");
    assert_eq!(
        devbox().last_sync_error,
        None,
        "探测后消失的源应重探一轮,不是报错"
    );
    assert!(devbox().last_sync_at.is_some());
    assert!(
        !cache
            .join(".local/share/opencode/opencode-next.db")
            .exists(),
        "消失的源不该进缓存"
    );
    assert!(cache.join(".local/share/opencode/opencode.db").is_file());
    for rel in [
        CLAUDE_U1,
        CODEX_R2,
        ".gemini/projects.json",
        ".kimi-code/session_index.jsonl",
        DSH_LOG,
    ] {
        assert!(cache.join(rel).is_file(), "{rel} 没同步到缓存");
    }
    assert!(
        cache.join(GROK_S7).join("updates.jsonl").is_file(),
        "缺席源之后的目录型源被截断"
    );
    assert!(!cache.join(".codex/auth.json").exists(), "凭证进了缓存");
    assert!(!cache.join(".copilot").exists());
    assert!(
        cache
            .join(".gemini/antigravity-cli/conversation_summaries.db")
            .is_file(),
        "SQLite 型的文件源没到"
    );

    // ② 真实 roster + 全量扫描:十五家远程会话以三段 key 入库,host/id/file_path 都对
    let roster = create_adapter_roster_for(&store);
    run_scan(&roster.active, &store, &NullEvents, true).expect("scan ok");
    let all = SessionFilter {
        include_archived: true,
        limit: 1000,
        ..Default::default()
    };
    let (sessions, total) = store.list_sessions(&all).unwrap();
    assert!(total > 0, "没有任何远程会话入库");
    let cache_prefix = cache.to_string_lossy().to_string();
    for s in &sessions {
        assert_eq!(s.host, "devbox", "{}", s.key);
        assert!(
            s.key.starts_with(&format!("{}:devbox:", s.agent.as_str())),
            "key 不是三段格式: {}",
            s.key
        );
        assert!(!s.id.contains(':'), "native id 混进了 host 段: {}", s.id);
        assert!(
            s.file_path.starts_with(&cache_prefix),
            "file_path 不在缓存树内: {}",
            s.file_path
        );
    }
    let agents: HashSet<AgentId> = sessions.iter().map(|s| s.agent).collect();
    for expected in remote_agents() {
        assert!(agents.contains(&expected), "{expected:?} 的远程会话没进库");
    }
    assert_eq!(
        store.host_counts().unwrap().get("devbox").copied(),
        Some(total),
        "Remote hosts 面板计数与列表不一致"
    );
    let count_of = |agent: AgentId| {
        let f = SessionFilter {
            agents: vec![agent],
            ..all.clone()
        };
        store.list_sessions(&f).unwrap().1
    };
    let claude_before = count_of(AgentId::ClaudeCode);

    // ③ 远端变化:删一个 grok 会话目录、整个卸掉 Codex、新增一个 claude 文件;
    // 再同步 + 增量扫描,三种变化都要传播到缓存与库
    fs::remove_dir_all(home.join(GROK_S7)).unwrap();
    fs::remove_dir_all(home.join(".codex")).unwrap();
    let added =
        ".claude/projects/-Users-tester-Github-wakefx/eeeeeeee-aaaa-bbbb-cccc-00000000000e.jsonl";
    fs::copy(home.join(CLAUDE_U1), home.join(added)).unwrap();
    wake_core::remote::sync_hosts(&store, &["devbox".to_string()]);
    assert_eq!(devbox().last_sync_error, None);
    assert!(
        !cache.join(GROK_S7).exists(),
        "源树内的删除未传播(--delete 被缺源压掉)"
    );
    assert!(
        !cache.join(".codex/sessions").exists(),
        "整棵源消失未传播到缓存"
    );
    assert!(cache.join(added).is_file(), "新增文件没同步到缓存");
    assert!(cache.join(CLAUDE_U1).is_file(), "无关的树被误删");
    run_scan(&roster.active, &store, &NullEvents, false).expect("rescan ok");
    assert_eq!(count_of(AgentId::Grok), 0, "远端删掉的 grok 会话还挂在库里");
    assert_eq!(
        count_of(AgentId::Codex),
        0,
        "远端卸掉的 codex 会话还挂在库里"
    );
    assert!(count_of(AgentId::ClaudeCode) >= claude_before);

    // ④ 连不上的 host:错误落库、缓存一字不动(不能把"连不上"当"远端空了")
    let down_cache = wake_core::remote::host_cache_dir(&db_dir, "down");
    touch(&down_cache.join(".claude/projects/p/keep.jsonl"), "{}\n");
    wake_core::remote::sync_hosts(&store, &["down".to_string()]);
    let down = host_row(&store, "down");
    let err = down
        .last_sync_error
        .expect("unreachable host must record an error");
    assert!(err.contains("Connection refused"), "{err}");
    assert!(down.last_sync_at.is_none());
    assert!(
        down_cache.join(".claude/projects/p/keep.jsonl").is_file(),
        "探测失败不得清缓存"
    );

    // ⑤ Remove host 的数据面:配置删掉 → roster 不再含该 host → 扫描把行按
    // "磁盘已删"出清 → 孤儿缓存清理只删它、不碰仍配置着的 down
    store.remove_remote_host("devbox").unwrap();
    let roster = create_adapter_roster_for(&store);
    assert!(
        roster.active.iter().all(|a| a.host() != "devbox"),
        "移除后的 host 仍在 roster 里"
    );
    run_scan(&roster.active, &store, &NullEvents, false).expect("rescan ok");
    assert_eq!(
        store.host_counts().unwrap().get("devbox"),
        None,
        "移除 host 后它的会话还在库里"
    );
    wake_core::remote::purge_orphan_caches(&store);
    assert!(!cache.exists(), "移除 host 的缓存目录没清");
    assert!(down_cache.exists(), "仍配置着的 host 缓存被误清");
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                count_files(&e.path())
            } else {
                1
            }
        })
        .sum()
}

/// 手动 live 检查:对着一台**真实** host(`~/.ssh/config` 别名,或 scratchpad
/// 里的 Docker 容器)跑同一条管线——真 ssh、真远端登录 shell、真 rsync 发送端,
/// 是假 ssh 覆盖不到的那一层。默认 ignore:
/// `WAKE_LIVE_REMOTE_HOST=<host> cargo test -p wake-core --test remote_sync live -- --ignored --nocapture`
/// 不做 fixture 级断言,只要求同步无错、缓存非空、扫描后有该 host 的会话入库,
/// 分家数量打印出来人工看。临时库与缓存都在 tempdir,不碰真实索引。
#[test]
#[ignore]
fn live_remote_host() {
    let Ok(host) = std::env::var("WAKE_LIVE_REMOTE_HOST") else {
        eprintln!("WAKE_LIVE_REMOTE_HOST not set; nothing to do");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let local_home = tmp.path().join("local-home");
    fs::create_dir_all(&local_home).unwrap();
    // 本地 roster 指向空 home(HOME 保留,ssh 要读 ~/.ssh)
    std::env::set_var("WAKE_HOME", &local_home);
    for var in [
        "XDG_DATA_HOME",
        "CODEX_HOME",
        "QODER_CONFIG_DIR",
        "HERMES_HOME",
        "OPENCLAW_STATE_DIR",
    ] {
        std::env::remove_var(var);
    }
    let store = Arc::new(Store::open(&tmp.path().join("wake.db")).unwrap());
    store.add_remote_host(&host).unwrap();
    let cache = wake_core::remote::host_cache_dir(&store.db_dir().unwrap(), &host);

    wake_core::remote::sync_hosts(&store, std::slice::from_ref(&host));
    let row = host_row(&store, &host);
    assert_eq!(row.last_sync_error, None, "sync failed");
    let files = count_files(&cache);
    eprintln!(
        "[live] {files} files mirrored from {host} into {}",
        cache.display()
    );
    assert!(files > 0, "cache is empty after a successful sync");

    let roster = create_adapter_roster_for(&store);
    run_scan(&roster.active, &store, &NullEvents, true).expect("scan ok");
    let (sessions, total) = store
        .list_sessions(&SessionFilter {
            include_archived: true,
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    let mut per_agent: std::collections::BTreeMap<&str, i64> = Default::default();
    for s in &sessions {
        assert_eq!(s.host, host, "{}", s.key);
        *per_agent.entry(s.agent.as_str()).or_default() += 1;
    }
    eprintln!("[live] {total} remote sessions from {host}: {per_agent:?}");
    assert!(total > 0, "no remote sessions indexed");
}
