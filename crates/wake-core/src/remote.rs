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
    ///(缺了 sqlite_ro 的 copy 梯度兜底)
    pub sync_paths: &'static [&'static str],
}

/// 十四家的远程布局。远程主机按 Linux/macOS 默认路径假设(两平台一致,
/// 均为 home 相对;OpenCode 的 XDG 变体、CODEX_HOME 这类 env 覆盖在远端
/// 探测不到,阶段 1 不支持非默认远程布局)。
pub const REMOTE_LAYOUTS: &[RemoteAgentLayout] = &[
    RemoteAgentLayout {
        agent: AgentId::ClaudeCode,
        mount: ".claude/projects",
        sync_paths: &[".claude/projects"],
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
    },
    RemoteAgentLayout {
        agent: AgentId::Qoder,
        mount: ".qoder/projects",
        sync_paths: &[".qoder/projects"],
    },
    RemoteAgentLayout {
        agent: AgentId::Copilot,
        // 目录层:db = <mount>/session-store.db(is_file 判据对未同步目录失真)
        mount: ".copilot",
        sync_paths: &[".copilot/session-store.db", ".copilot/session-store.db-wal"],
    },
    RemoteAgentLayout {
        agent: AgentId::Cursor,
        mount: ".cursor/projects",
        sync_paths: &[".cursor/projects"],
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
    },
    RemoteAgentLayout {
        agent: AgentId::Kiro,
        mount: ".kiro/sessions/cli",
        sync_paths: &[".kiro/sessions/cli"],
    },
    RemoteAgentLayout {
        agent: AgentId::Gemini,
        // tmp 层:projects.json 由 parent 派生(.gemini 顶层有凭证,不整树同步)
        mount: ".gemini/tmp",
        sync_paths: &[".gemini/tmp", ".gemini/projects.json"],
    },
    RemoteAgentLayout {
        agent: AgentId::Pi,
        mount: ".pi/agent/sessions",
        sync_paths: &[".pi/agent/sessions"],
    },
    RemoteAgentLayout {
        agent: AgentId::Omp,
        mount: ".omp/agent/sessions",
        sync_paths: &[".omp/agent/sessions"],
    },
    RemoteAgentLayout {
        agent: AgentId::Grok,
        // sessions 目录名判据 → home=parent,summary/chat_history 都在树内
        mount: ".grok/sessions",
        sync_paths: &[".grok/sessions"],
    },
    RemoteAgentLayout {
        agent: AgentId::Kimi,
        mount: ".kimi-code/sessions",
        sync_paths: &[".kimi-code/sessions", ".kimi-code/session_index.jsonl"],
    },
    RemoteAgentLayout {
        agent: AgentId::Antigravity,
        // 库所在目录层(is_file 判据对未同步目录失真,目录层恒正确)
        mount: ".gemini/antigravity-cli",
        sync_paths: &[
            ".gemini/antigravity-cli/conversation_summaries.db",
            ".gemini/antigravity-cli/conversation_summaries.db-wal",
        ],
    },
    RemoteAgentLayout {
        agent: AgentId::Dsh,
        mount: ".dsh/sessions",
        sync_paths: &[".dsh/sessions"],
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

/// 单 host 的 rsync 参数(不含程序名)。纯函数,单测卡命令形状。
/// `-R`(--relative)让 `host:./<相对路径>` 在目标重建相对布局;
/// `--delete` 让远程删除传播到缓存(watcher 收 Remove 后清库内行);
/// 缺失的源(该家没装)由 rsync 跳过并以 exit 23 报部分传输,视为软成功。
pub fn rsync_args(host: &str, dest: &Path) -> Vec<String> {
    let mut args = vec![
        "-az".to_string(),
        "-R".to_string(),
        "--delete".to_string(),
        // Grok 自家的搜索索引库,量大且 Wake 不读
        "--exclude=session_search.sqlite*".to_string(),
        "--timeout=120".to_string(),
        "-e".to_string(),
        "ssh -oBatchMode=yes -oConnectTimeout=10".to_string(),
    ];
    for layout in REMOTE_LAYOUTS {
        for path in layout.sync_paths {
            args.push(format!("{host}:./{path}"));
        }
    }
    args.push(dest.to_string_lossy().to_string());
    args
}

/// 同步指定 host(调用方给"全部启用的"或单个新加的)。**绝不 panic、绝不
/// Err**——任何失败只记进 `remote_hosts.last_sync_error`,面板展示,后续
/// 扫描照常消费既有缓存。各 host 的 rsync 彼此独立(纯网络等待),并行跑,
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
                    run_rsync(name, &dest).err()
                } else {
                    Some("invalid host name".to_string())
                };
                let _ = store.record_remote_sync(name, error.as_deref());
            });
        }
    });
}

/// 起 rsync 并归类退出码。0 = 全量成功;23(部分源缺失,该家远端没装)与
/// 24(传输中源文件消失)是多源白名单同步的常态,同样算成功。
fn run_rsync(host: &str, dest: &Path) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(dest) {
        return Err(format!("cannot create cache dir {}: {e}", dest.display()));
    }
    let output = std::process::Command::new("rsync")
        .args(rsync_args(host, dest))
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("rsync not found on PATH — install rsync to sync remote hosts".into());
        }
        Err(e) => return Err(format!("failed to run rsync: {e}")),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        // 24 = 传输中源文件消失(agent 正在写,下轮自愈)
        Some(0) | Some(24) => Ok(()),
        // 23 是"部分传输失败"泛码:白名单里的源在远端不存在(那台只装了
        // 部分 agent)是常态,豁免;但权限/读写错误同样报 23,把它们记成功
        // 会隐藏真实缺数据——只有 stderr 里的错误行**全部**是 ENOENT 才算软
        // 成功(总结行不算错误行;openrsync/rsync 的缺源消息都带这句话)
        Some(23)
            if stderr
                .lines()
                .filter(|l| l.contains("rsync") && (l.contains("error") || l.contains("failed")))
                .filter(|l| !l.contains("some files/attrs were not transferred"))
                .all(|l| l.contains("No such file or directory")) =>
        {
            Ok(())
        }
        code => {
            // 取末几行(rsync 把决定性错误留在最后),字符封顶防单行巨量输出
            let mut lines: Vec<&str> = stderr.trim().lines().rev().take(3).collect();
            lines.reverse();
            let tail: String = lines.join("; ").chars().take(500).collect();
            Err(match code {
                Some(c) => format!("rsync exited with {c}: {tail}"),
                None => format!("rsync terminated by signal: {tail}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rsync_args_shape() {
        let args = rsync_args("devbox", Path::new("/tmp/cache/devbox"));
        assert_eq!(args[0], "-az");
        assert!(args.contains(&"-R".to_string()));
        assert!(args.contains(&"--delete".to_string()));
        assert!(args
            .iter()
            .any(|a| a == "ssh -oBatchMode=yes -oConnectTimeout=10"));
        // 源都是 host:./ 锚定的 home 相对路径
        assert!(args.contains(&"devbox:./.claude/projects".to_string()));
        assert!(args.contains(&"devbox:./.codex/state_5.sqlite".to_string()));
        // 目标在最后
        assert_eq!(args.last().unwrap(), "/tmp/cache/devbox");
        // 凭证绝不在名单上
        assert!(!args.iter().any(|a| a.contains("auth.json")));
        assert!(!args.iter().any(|a| a.contains("oauth_creds")));
    }
}
