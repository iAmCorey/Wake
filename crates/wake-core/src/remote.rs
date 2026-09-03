//! 远程 host 的会话聚合(SSH,2026-09-01 阶段 1)。
//!
//! 形态:**同步到本地缓存,而不是远程直读**——`rsync` 把远程 home 下的会话
//! 数据树按白名单镜像到 `<索引库目录>/remotes/<host>/`,缓存树保持远程 home
//! 的相对布局,由 `adapters::remote::RemoteAdapter`(装饰器)以各家
//! `with_custom_root` 挂进 roster。adapter/解析器/FTS 对远程零感知。
//!
//! shell out 到系统 `ssh`/`rsync` 而不用 SSH 库:`~/.ssh/config` 的别名、
//! ProxyJump、IdentityAgent、ControlMaster 全部免费继承。`BatchMode=yes`
//! 确保 GUI 进程(无 TTY)绝不挂在交互认证上——密钥要么已在 agent 里,
//! 要么明确失败并把 stderr 记进 `remote_hosts.last_sync_error`。
//!
//! 一次同步 = **先探测再镜像**:`ssh host sh -c '…'` 逐个 `[ -e ]` 白名单
//! 路径,只把远端真实存在的交给 `rsync -R --delete`,远端整个不存在的在
//! 缓存里就地删掉。不能把整张白名单直接喂 rsync(2026-09-02 本机实测):
//! ①openrsync(macOS 15+ 的 /usr/bin/rsync)做发送端时,某个源连父目录
//! 都不存在会让它中止整份文件列表,排在后面的源全部静默不传,退出码却
//! 只是 23;②rsync 家族只要发送端遇到任何 I/O 错误(缺源即算)就整体跳过
//! 删除阶段——没有哪台机器十六家全装,`--delete` 就永远不会生效。
//!
//! 同步跑在**独立于扫描的线程**(Workbench::spawn_remote_sync):本地扫描
//! 不等网络,不可达 host 只拖慢自己;缓存落盘由 watcher 增量收编,同步
//! 完成事件再补一轮增量兜底。
//!
//! 白名单只含**会话数据与其侧档**,绝不同步凭证(`.claude` 顶层、
//! `.codex/auth.json`、`.gemini/oauth_creds.json`、opencode 的 auth.json
//! 都不在名单上)。新增 adapter 必须在 `REMOTE_LAYOUTS` 补一行,契约测试
//! 卡全家覆盖。

use crate::db::Store;
use crate::models::AgentId;
use crate::services::terminal::sh_quote;
use std::path::{Path, PathBuf};

/// 一家 agent 在远程 home 下的数据布局。
pub struct RemoteAgentLayout {
    pub agent: AgentId,
    /// 缓存树内交给 `with_custom_root` 的目录(相对缓存根 = 相对远程 home)。
    /// 必须选"整形判据在目录**不存在**时也落到正确形态"的那一层——同步是
    /// 异步的,实例构造时缓存树可能还没落盘(各家判据见其 with_custom_root)
    pub mount: &'static str,
    /// rsync 白名单源(相对远程 home 的 POSIX 路径;目录整树,文件单拉)。
    /// SQLite 库连 `-wal` 一起拉(数据在 wal 里),`-shm` 是运行时产物不拉
    ///(缺了 sqlite_ro 的 copy 梯度兜底)。字符集限 `[A-Za-z0-9._/-]`——
    /// 它们不加引号直接进探测脚本(sh -c 单引号串内)与 rsync 源参数(单测卡)
    pub sync_paths: &'static [&'static str],
    /// rsync `--exclude` 模式(按文件名匹配,作用于整次同步)。凭证与本家不读的
    /// 大索引写在自己这一行——不放全局列表,新增带凭证的 agent 在这里声明
    pub exclude: &'static [&'static str],
}

/// 十六家的远程布局。远程主机按 Linux/macOS 默认路径假设(两平台一致,
/// 均为 home 相对;OpenCode 的 XDG 变体、CODEX_HOME 这类 env 覆盖在远端
/// 探测不到,阶段 1 不支持非默认远程布局)。
pub const REMOTE_LAYOUTS: &[RemoteAgentLayout] = &[
    RemoteAgentLayout {
        agent: AgentId::ClaudeCode,
        mount: ".claude/projects",
        sync_paths: &[".claude/projects"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Codex,
        // home 形态:sessions/archived_sessions/state_5.sqlite 全相对派生
        mount: ".codex",
        sync_paths: &[
            ".codex/sessions",
            ".codex/archived_sessions",
            ".codex/state_5.sqlite",
            ".codex/state_5.sqlite-wal",
        ],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Qoder,
        mount: ".qoder/projects",
        sync_paths: &[".qoder/projects"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Copilot,
        // 目录层:db = <mount>/session-store.db(is_file 判据对未同步目录失真)
        mount: ".copilot",
        sync_paths: &[".copilot/session-store.db", ".copilot/session-store.db-wal"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Cursor,
        mount: ".cursor/projects",
        sync_paths: &[".cursor/projects"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Opencode,
        // custom_db_paths 对目录给出 stable + next 两个固定候选
        mount: ".local/share/opencode",
        sync_paths: &[
            ".local/share/opencode/opencode.db",
            ".local/share/opencode/opencode.db-wal",
            ".local/share/opencode/opencode-next.db",
            ".local/share/opencode/opencode-next.db-wal",
        ],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Kiro,
        mount: ".kiro/sessions/cli",
        sync_paths: &[".kiro/sessions/cli"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Gemini,
        // tmp 层:projects.json 由 parent 派生(.gemini 顶层有凭证,不整树同步)
        mount: ".gemini/tmp",
        sync_paths: &[".gemini/tmp", ".gemini/projects.json"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Pi,
        mount: ".pi/agent/sessions",
        sync_paths: &[".pi/agent/sessions"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Omp,
        mount: ".omp/agent/sessions",
        sync_paths: &[".omp/agent/sessions"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Grok,
        // sessions 目录名判据 → home=parent,summary/chat_history 都在树内
        mount: ".grok/sessions",
        sync_paths: &[".grok/sessions"],
        // Grok 自家的搜索索引库,量大且 Wake 不读
        exclude: &["session_search.sqlite*"],
    },
    RemoteAgentLayout {
        agent: AgentId::Kimi,
        mount: ".kimi-code/sessions",
        sync_paths: &[".kimi-code/sessions", ".kimi-code/session_index.jsonl"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Antigravity,
        // 库所在目录层(is_file 判据对未同步目录失真,目录层恒正确)
        mount: ".gemini/antigravity-cli",
        sync_paths: &[
            ".gemini/antigravity-cli/conversation_summaries.db",
            ".gemini/antigravity-cli/conversation_summaries.db-wal",
        ],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Dsh,
        mount: ".dsh/sessions",
        sync_paths: &[".dsh/sessions"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Hermes,
        // 库所在目录层:db = <mount>/state.db(profiles 多档案不同步)
        mount: ".hermes",
        sync_paths: &[".hermes/state.db", ".hermes/state.db-wal"],
        exclude: &[],
    },
    RemoteAgentLayout {
        agent: AgentId::Openclaw,
        // agents/<id>/ 整树,exclude 挡掉每个 agent 目录里的凭证:auth-profiles.json
        // 与 openclaw-agent.sqlite(库内 auth_profile_store 表存着 API key/OAuth
        // token,会话与凭证同库,只能整库不拉)——远程 OpenClaw 因此只覆盖旧版
        // sessions/*.jsonl 转录
        mount: ".openclaw/agents",
        sync_paths: &[".openclaw/agents"],
        exclude: &["auth-profiles.json", "openclaw-agent.sqlite*"],
    },
];

/// host 名合法性:进 session key 作中段(不能有 ':')、直接作为 ssh/rsync
/// 目标(不能以 '-' 开头——那会被当作命令行选项,是参数注入面)。允许
/// `user@host`、别名、IPv4;IPv6 字面量请在 `~/.ssh/config` 里包一个别名。
pub fn valid_host_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// 删掉 `remotes/` 下不再对应任何已配置 host 的缓存目录。同步进行中被
/// Remove 的 host,其 rsync 无法取消、会把刚删的目录写回——每轮同步收工
/// 后按配置表裁决一次,幂等自愈(该目录树只有 Wake 自己写)。
pub fn purge_orphan_caches(store: &Store) {
    let Some(db_dir) = store.db_dir() else { return };
    let known: std::collections::HashSet<String> = store
        .list_remote_hosts()
        .unwrap_or_default()
        .into_iter()
        .map(|h| h.name)
        .collect();
    let Ok(entries) = std::fs::read_dir(db_dir.join("remotes")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_none_or(|n| !known.contains(n)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 单 host 的缓存树(各 host 挂在 `<索引库目录>/remotes/<host>/` 下)。
pub fn host_cache_dir(db_dir: &Path, host: &str) -> PathBuf {
    db_dir.join("remotes").join(host)
}

/// ssh 的批处理选项:探测与 rsync 的 `-e` 同一份——认证/超时行为两步一致,
/// 探测过了 rsync 就不会再卡在认证上
const SSH_OPTS: [&str; 2] = ["-oBatchMode=yes", "-oConnectTimeout=10"];

/// 白名单全部 sync_paths,按 REMOTE_LAYOUTS 顺序(探测脚本、rsync 源列表、
/// 缺席清理三处同序)
fn all_sync_paths() -> impl Iterator<Item = &'static str> {
    REMOTE_LAYOUTS
        .iter()
        .flat_map(|layout| layout.sync_paths.iter().copied())
}

/// 远端存在性探测的 ssh 参数(不含程序名)。远端命令是一整条
/// `sh -c '…'`:ssh 把它交给登录 shell 再解析,单引号串在 fish/csh 里同样
/// 原样传给 sh,循环语法不依赖登录 shell 方言(quote 走 terminal::sh_quote,
/// 与本地/远程 resume 命令同一实现)。存在的路径按行回显;循环体用 if 而非
/// `&&`,末项缺席不会把命令退出码带成非零——非零退出只表示 ssh 本身失败
///(认证/不可达/远端无 sh)。
fn probe_args(host: &str) -> Vec<String> {
    let list = all_sync_paths().collect::<Vec<_>>().join(" ");
    let script =
        format!("for p in {list}; do if [ -e \"$p\" ]; then printf \"%s\\n\" \"$p\"; fi; done");
    SSH_OPTS
        .iter()
        .map(|s| s.to_string())
        .chain([host.to_string(), format!("sh -c {}", sh_quote(&script))])
        .collect()
}

/// 探测输出 → 白名单顺序的存在路径。只认与白名单**逐字相等**的行:远端
/// rc 文件的 echo、MOTD、警告之类都不能变成 rsync 的源参数。
fn present_remote_paths(stdout: &str) -> Vec<&'static str> {
    let lines: std::collections::HashSet<&str> = stdout.lines().map(str::trim).collect();
    all_sync_paths().filter(|p| lines.contains(p)).collect()
}

/// 单 host 的 rsync 参数(不含程序名),`paths` 是探测确认存在的源。纯函数,
/// 单测卡命令形状。`-R`(--relative)让 `host:./<相对路径>` 在目标重建相对
/// 布局;`--delete` 让远程删除传播到缓存(watcher 收 Remove 后清库内行)——
/// 它只作用于各源树**内部**,整棵源消失由 sync_host 的缺席清理负责。
fn rsync_args(host: &str, dest: &Path, paths: &[&str]) -> Vec<String> {
    let mut args = vec!["-az".to_string(), "-R".to_string(), "--delete".to_string()];
    args.extend(
        REMOTE_LAYOUTS
            .iter()
            .flat_map(|l| l.exclude.iter())
            .map(|pat| format!("--exclude={pat}")),
    );
    args.extend([
        "--timeout=120".to_string(),
        "-e".to_string(),
        format!("ssh {}", SSH_OPTS.join(" ")),
    ]);
    args.extend(paths.iter().map(|path| format!("{host}:./{path}")));
    args.push(dest.to_string_lossy().to_string());
    args
}

/// 同步指定 host(调用方给"全部启用的"或单个新加的)。**绝不 panic、绝不
/// Err**——任何失败只记进 `remote_hosts.last_sync_error`,面板展示,后续
/// 扫描照常消费既有缓存。各 host 的同步彼此独立(纯网络等待),并行跑,
/// 总耗时 = 最慢一台而非各台之和;状态写库经 write Mutex 天然串行。
pub fn sync_hosts(store: &Store, names: &[String]) {
    let Some(db_dir) = store.db_dir() else {
        return;
    };
    std::thread::scope(|scope| {
        for name in names {
            let dest = host_cache_dir(&db_dir, name);
            scope.spawn(move || {
                // 已入库的 host 也再卡一道:老库可能被未来版本写入过更宽的名字
                let error = if valid_host_name(name) {
                    sync_host(name, &dest).err()
                } else {
                    Some("invalid host name".to_string())
                };
                let _ = store.record_remote_sync(name, error.as_deref());
            });
        }
    });
}

/// 单 host 的一次同步:探测 → 缺席清理 → rsync。探测失败即整体失败,缓存
/// 一字不动(不能把"连不上"当成"远端空了"去清缓存)。探测到 rsync 之间隔着
/// 一次握手,远端 agent 恰好在这个空档退出会把 `-wal` 之类删掉,rsync 对
/// 命令行源 link_stat 失败报 23 且整轮跳过 --delete——只含 ENOENT 的 23
/// 重探一轮再镜像(第二轮拿到的源集合已经是消失后的),第二轮结果照实报。
fn sync_host(host: &str, dest: &Path) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(dest) {
        return Err(format!("cannot create cache dir {}: {e}", dest.display()));
    }
    let mut retried = false;
    loop {
        let present = probe_remote(host)?;
        // 远端整个不存在的白名单项(该家卸了、库被清了):镜像也删。rsync 的
        // --delete 只作用于它拿到的源树内部,整棵源消失只有这里能传播
        for path in all_sync_paths() {
            if !present.contains(&path) {
                remove_cached(&dest.join(path))?;
            }
        }
        if present.is_empty() {
            return Ok(());
        }
        match run_rsync(host, dest, &present) {
            Ok(()) => return Ok(()),
            Err(RsyncError::SourcesVanished(_)) if !retried => retried = true,
            Err(RsyncError::SourcesVanished(msg) | RsyncError::Other(msg)) => return Err(msg),
        }
    }
}

/// 删缓存里的一个白名单项(目录整树 / 单文件),不存在即无事;删不掉(权限、
/// 锁着的库文件、瞬时 I/O 错误)要当同步失败上报——否则远端已删的会话留在
/// 缓存和索引里,面板却显示同步成功
fn remove_cached(path: &Path) -> Result<(), String> {
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if not_found(&e) => return Ok(()),
        Err(e) => {
            return Err(format!(
                "cannot inspect stale cache {}: {e}",
                path.display()
            ))
        }
    };
    let removed = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if not_found(&e) => Ok(()),
        Err(e) => Err(format!("cannot remove stale cache {}: {e}", path.display())),
    }
}

/// 起 ssh 探测。ssh 非零退出(255 = 连接/认证失败)整体报错,stderr 末几行
/// 是用户能看懂的原话("Permission denied (publickey)"、"Host key
/// verification failed")
fn probe_remote(host: &str) -> Result<Vec<&'static str>, String> {
    let output = run_tool("ssh", "OpenSSH", &probe_args(host))?;
    classify("ssh", &output, &[])?;
    Ok(present_remote_paths(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

enum RsyncError {
    /// 只含 ENOENT 的 exit 23:探测过的源在传输前消失(附 rsync 原话,
    /// 调用方重探失败时照实报)
    SourcesVanished(String),
    Other(String),
}

/// 起 rsync。源已经过探测,缺源不再是常态:0 = 全量成功;24(传输中源文件
/// 消失,agent 正在写,下轮自愈)算软成功;23 是"部分传输失败"泛码——错误
/// 行全是 ENOENT 时是探测后消失的源,交调用方重探,权限/读错误同样报 23,
/// 照常报错(记成功会隐藏真实缺数据)。
fn run_rsync(host: &str, dest: &Path, paths: &[&str]) -> Result<(), RsyncError> {
    let output =
        run_tool("rsync", "rsync", &rsync_args(host, dest, paths)).map_err(RsyncError::Other)?;
    if output.status.code() == Some(23) && only_missing_sources(&output.stderr) {
        return Err(RsyncError::SourcesVanished(exit_error("rsync", &output)));
    }
    classify("rsync", &output, &[24]).map_err(RsyncError::Other)
}

/// rsync 的错误行是否**全部**是"源不存在"。GNU rsync 报
/// `rsync: [sender] link_stat "…" failed: No such file or directory (2)`,
/// openrsync 报 `rsync(pid): error: …: (l)stat: No such file or directory`;
/// 总结行(some files/attrs were not transferred)不算错误行;一行错误都
/// 没有的 23 不认(形状未知,当真错误报)
fn only_missing_sources(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    let mut errors = stderr
        .lines()
        .filter(|l| l.contains("rsync") && (l.contains("error") || l.contains("failed")))
        .filter(|l| !l.contains("some files/attrs were not transferred"))
        .peekable();
    errors.peek().is_some() && errors.all(|l| l.contains("No such file or directory"))
}

/// 起一个外部工具:程序不在 PATH → 安装提示,起不来 → 原因;退出码不在这里
/// 裁决(见 classify),调用方还要看原始 stderr
fn run_tool(
    bin: &str,
    install_hint: &str,
    args: &[String],
) -> Result<std::process::Output, String> {
    let output = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(o) => Ok(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "{bin} not found on PATH — install {install_hint} to sync remote hosts"
        )),
        Err(e) => Err(format!("failed to run {bin}: {e}")),
    }
}

/// 退出码归类:{0} ∪ `soft_ok` 通过,其余给 exit_error
fn classify(bin: &str, output: &std::process::Output, soft_ok: &[i32]) -> Result<(), String> {
    match output.status.code() {
        Some(0) => Ok(()),
        Some(c) if soft_ok.contains(&c) => Ok(()),
        _ => Err(exit_error(bin, output)),
    }
}

/// `<bin> exited with N: <stderr 末几行>`——ssh/rsync 都把决定性错误留在
/// 最后,字符封顶防单行巨量输出
fn exit_error(bin: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut lines: Vec<&str> = stderr.trim().lines().rev().take(3).collect();
    lines.reverse();
    let tail: String = lines.join("; ").chars().take(500).collect();
    match output.status.code() {
        Some(c) => format!("{bin} exited with {c}: {tail}"),
        None => format!("{bin} terminated by signal: {tail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_missing_sources_recognizes_both_rsync_dialects() {
        // GNU rsync(Linux 远端做发送端,经本地客户端转出)
        let gnu = b"rsync: [sender] link_stat \"/home/u/.codex/state_5.sqlite-wal\" failed: No such file or directory (2)\n\
                    rsync error: some files/attrs were not transferred (see previous errors) (code 23) at main.c(1338)\n";
        assert!(only_missing_sources(gnu));
        // openrsync(macOS 远端做发送端)
        let openrsync =
            b"rsync(63771): error: .codex/state_5.sqlite-wal: (l)stat: No such file or directory\n";
        assert!(only_missing_sources(openrsync));
        // 混进一条权限错误就不算"只是消失"
        let mixed = b"rsync: [sender] link_stat \"/x\" failed: No such file or directory (2)\n\
                      rsync: [sender] send_files failed to open \"/y\": Permission denied (13)\n";
        assert!(!only_missing_sources(mixed));
        // 一行错误都没有的 23:形状未知,不认
        assert!(!only_missing_sources(b""));
        assert!(!only_missing_sources(
            b"rsync error: some files/attrs were not transferred (code 23)\n"
        ));
    }

    #[test]
    fn remove_cached_reports_failures_but_not_absence() {
        let dir = tempfile::tempdir().unwrap();
        // 不存在:无事
        assert_eq!(remove_cached(&dir.path().join("nope")), Ok(()));
        // 文件与目录都删得掉
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(remove_cached(&file), Ok(()));
        assert!(!file.exists());
        let sub = dir.path().join("d/e");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(remove_cached(&dir.path().join("d")), Ok(()));
        assert!(!sub.exists());
        // 路径穿过一个普通文件(ENOTDIR):不是"不存在",必须报错
        std::fs::write(&file, b"x").unwrap();
        let err = remove_cached(&file.join("child")).unwrap_err();
        assert!(err.contains("cannot inspect stale cache"), "{err}");
    }

    #[test]
    fn layouts_cover_every_agent_exactly_once() {
        // 新增 agent 忘了补远程布局表,这里就是第一道报警
        let mut seen = std::collections::HashSet::new();
        for layout in REMOTE_LAYOUTS {
            assert!(
                seen.insert(layout.agent),
                "duplicate layout {:?}",
                layout.agent
            );
            assert!(!layout.sync_paths.is_empty());
            for path in layout.sync_paths {
                assert!(!path.starts_with('/'), "sync path must be home-relative");
                assert!(!path.contains(".."));
            }
            assert!(!layout.mount.starts_with('/'));
        }
        for agent in AgentId::ALL {
            assert!(seen.contains(&agent), "missing remote layout for {agent:?}");
        }
    }

    #[test]
    fn host_name_validation_blocks_injection() {
        assert!(valid_host_name("devbox"));
        assert!(valid_host_name("corey@192.168.1.5"));
        assert!(valid_host_name("my-host.example.com"));
        assert!(valid_host_name("a"));
        // 选项注入与 key 分隔符
        assert!(!valid_host_name("-oProxyCommand=evil"));
        assert!(!valid_host_name(""));
        assert!(!valid_host_name("host:22"));
        assert!(!valid_host_name("host name"));
        assert!(!valid_host_name("host/../x"));
        assert!(!valid_host_name(".hidden"));
    }

    #[test]
    fn sync_paths_are_shell_safe() {
        // 白名单路径不加引号直接进探测脚本(sh -c 单引号串内)与 rsync 源参数
        for path in all_sync_paths() {
            assert!(
                path.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')),
                "sync path {path:?} needs quoting"
            );
        }
        // 凭证绝不在名单上
        assert!(!all_sync_paths().any(|p| p.contains("auth.json") || p.contains("oauth_creds")));
    }

    #[test]
    fn probe_args_shape() {
        let args = probe_args("devbox");
        assert_eq!(
            args[..3].to_vec(),
            ["-oBatchMode=yes", "-oConnectTimeout=10", "devbox"]
        );
        // 一整条 sh -c '…':每个白名单路径都在 for 列表里,循环体按行回显存在项
        let script = &args[3];
        assert!(script.starts_with("sh -c 'for p in "));
        for path in all_sync_paths() {
            assert!(
                script.contains(&format!(" {path} ")) || script.contains(&format!(" {path};")),
                "{path} 不在探测列表里"
            );
        }
        assert!(script.ends_with("; do if [ -e \"$p\" ]; then printf \"%s\\n\" \"$p\"; fi; done'"));
    }

    #[test]
    fn present_remote_paths_keeps_only_whitelist_in_layout_order() {
        // 乱序、重复、rc 文件的杂音、白名单项的父目录/子路径、绝对路径都要滤掉
        let out = "Welcome to devbox\n.grok/sessions\n.codex/sessions\n.codex\n\
                   .codex/sessions\n  .claude/projects  \n.claude/projects/evil\n/etc/passwd\n";
        assert_eq!(
            present_remote_paths(out),
            vec![".claude/projects", ".codex/sessions", ".grok/sessions"]
        );
        assert!(present_remote_paths("").is_empty());
    }

    #[test]
    fn rsync_args_shape() {
        let present = [".claude/projects", ".codex/state_5.sqlite"];
        let args = rsync_args("devbox", Path::new("/tmp/cache/devbox"), &present);
        assert_eq!(args[0], "-az");
        assert!(args.contains(&"-R".to_string()));
        assert!(args.contains(&"--delete".to_string()));
        assert!(args
            .iter()
            .any(|a| a == "ssh -oBatchMode=yes -oConnectTimeout=10"));
        // 只有探测确认存在的源,且都是 host:./ 锚定的 home 相对路径
        let sources: Vec<&str> = args
            .iter()
            .filter(|a| a.starts_with("devbox:"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            sources,
            vec![
                "devbox:./.claude/projects",
                "devbox:./.codex/state_5.sqlite"
            ]
        );
        // 目标在最后
        assert_eq!(args.last().unwrap(), "/tmp/cache/devbox");
        // 各家声明的 exclude 全部落到命令行(OpenClaw 的凭证靠它挡)
        for pat in REMOTE_LAYOUTS.iter().flat_map(|l| l.exclude.iter()) {
            assert!(
                args.contains(&format!("--exclude={pat}")),
                "{pat} 未进 rsync 参数"
            );
            assert!(
                pat.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '*')),
                "exclude {pat:?} needs quoting"
            );
        }
        assert!(args.contains(&"--exclude=openclaw-agent.sqlite*".to_string()));
    }
}
