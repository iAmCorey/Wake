//! 本机会话的原始镜像归档(vault)。
//!
//! Wake 的索引只是各家会话文件的视图:源文件一删(用户手动清、Claude Code 按
//! `cleanupPeriodDays` 自己清),会话就从 Wake 里消失。vault 把会话文件**原样**
//! (逐字节,不解析、不改写)镜像进 `<索引库目录>/vault/`,目录布局与 home 下一致,
//! 再由 roster 把它当成同一家的又一个 location 挂上(`adapters::remote` 的归档
//! 装饰器):
//!
//! - 源文件还在:两份副本同 key,归档那份 `dedup_rank` 恒落后,列表里只有源那一份;
//! - 源文件没了:全量扫描的去重、watcher 的 `promote_survivors` 本来就会让幸存
//!   副本顶上——归档那份接手,会话照旧能浏览、搜索、导出。
//!
//! **只增不减**:镜像从不因为源消失而删文件;转录(非 SQLite)源比归档小的时候也
//! 不覆盖——被截断、被改写成更短的一份不能冲掉更完整的存档。这条只对转录(`.jsonl`)
//! 成立:SQLite 边车(Codex 的 state 库)、skill、记忆文件不是追加型,变了就整份覆盖。
//!
//! 只收转录是独立文件的家(见 `VAULT_LAYOUTS`);会话存在别家 SQLite 库里的
//! (Cursor IDE、OpenCode…)没有"一个会话一份原始文件"可拷,不在范围内。
//! 源路径按 home 下的默认布局取,`CODEX_HOME` 这类 env 覆盖与自定义 location
//! 暂不镜像。

use crate::db::Store;
use crate::models::AgentId;
use std::path::{Path, PathBuf};

/// 一家 agent 进归档的范围。`mount` 与 `sync_paths` 的含义同
/// `remote::RemoteAgentLayout`:路径相对 home,镜像后相对 vault 根不变
pub struct VaultLayout {
    pub agent: AgentId,
    /// vault 内交给 `with_custom_root` 的目录
    pub mount: &'static str,
    /// 镜像的源(目录整树,文件单拷)。只列会话数据与读它必需的边车,绝不含凭证
    pub sync_paths: &'static [&'static str],
}

pub const VAULT_LAYOUTS: &[VaultLayout] = &[
    VaultLayout {
        agent: AgentId::ClaudeCode,
        mount: ".claude/projects",
        // skills 给网页版的 Skills 页;记忆在 projects/<dir>/memory/ 里,已经在范围内
        sync_paths: &[".claude/projects", ".claude/skills"],
    },
    VaultLayout {
        agent: AgentId::Codex,
        mount: ".codex",
        sync_paths: &[
            ".codex/sessions",
            ".codex/archived_sessions",
            ".codex/state_5.sqlite",
            ".codex/state_5.sqlite-wal",
            // 记忆与全局指令(与远程镜像的白名单同一批,都不是凭证),给网页版的 Memory 页
            ".codex/memories",
            ".codex/memories_1.sqlite",
            ".codex/memories_1.sqlite-wal",
            ".codex/AGENTS.md",
            ".codex/rules",
            // skills:Codex 自己的,与多家共读的 ~/.agents/skills
            ".codex/skills",
            ".agents/skills",
        ],
    },
];

/// 默认归档根:`<索引库目录>/vault/`
pub fn vault_dir(db_dir: &Path) -> PathBuf {
    db_dir.join("vault")
}

/// 用户自选归档根的偏好键(prefs 表;值是绝对路径,空或没有 = 默认位置)
const PREF_DIR: &str = "vault.dir";

/// 用户选了文件夹之后,归档实际落在它下面的这个子目录里:归档根直属的是 `.claude`、
/// `.codex` 这样的隐藏目录,直接铺进用户的同步盘文件夹既看不见又容易和别的东西混
pub const CUSTOM_SUBDIR: &str = "Wake Vault";

/// 这个库的归档根:用户选的位置,否则默认位置。所有读写归档的入口都走它
pub fn dir_for(store: &Store) -> Option<PathBuf> {
    match store.pref_get(PREF_DIR).filter(|v| !v.is_empty()) {
        Some(custom) => Some(PathBuf::from(custom)),
        None => Some(vault_dir(&store.db_dir()?)),
    }
}

/// 归档是不是在用户自选的位置
pub fn is_custom(store: &Store) -> bool {
    store.pref_get(PREF_DIR).is_some_and(|v| !v.is_empty())
}

/// 这条会话文件路径是不是归档里的副本(= 源已经不在了,列表里由归档顶着)
pub fn is_archived(vault: &Path, file_path: &str) -> bool {
    Path::new(file_path).starts_with(vault)
}

/// 把归档搬到 `folder` 下的 `CUSTOM_SUBDIR`(`None` = 搬回默认位置),返回新的归档根。
///
/// 搬法就是把旧归档当源、按镜像的规矩(`mirror`:只增不减、先临时名再 rename)并进新位置,
/// 所以跨卷也行;新位置已经有一份归档(另一台机器同步过来的、重装前留下的)就是合并,
/// 哪边更完整留哪边。全部拷成功才改偏好、删旧目录——中途失败旧归档原样可用。
/// 阻塞执行(跨卷时是整份拷贝),调用方放后台线程;搬完要重组 roster 并补扫
pub fn relocate(store: &Store, folder: Option<&Path>) -> anyhow::Result<PathBuf> {
    let old = dir_for(store).ok_or_else(|| anyhow::anyhow!("index has no directory"))?;
    let new = match folder {
        Some(folder) => {
            anyhow::ensure!(folder.is_absolute(), "choose an absolute path");
            folder.join(CUSTOM_SUBDIR)
        }
        None => vault_dir(
            &store
                .db_dir()
                .ok_or_else(|| anyhow::anyhow!("index has no directory"))?,
        ),
    };
    if new == old {
        return Ok(new);
    }
    anyhow::ensure!(
        !new.starts_with(&old) && !old.starts_with(&new),
        "the new location can't be inside the current archive"
    );
    // 归档放进某家的会话目录里,那一家会把归档副本当成自己的会话再镜像一遍
    if let Some(home) = crate::adapters::home_dir() {
        for layout in VAULT_LAYOUTS {
            for rel in layout.sync_paths {
                anyhow::ensure!(
                    !new.starts_with(home.join(rel)),
                    "the archive can't live inside an agent's own session folder"
                );
            }
        }
    }
    std::fs::create_dir_all(&new)?;
    let mut stats = SyncStats::default();
    for entry in walkdir::WalkDir::new(&old)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        if let Ok(tail) = entry.path().strip_prefix(&old) {
            mirror(entry.path(), &new.join(tail), &mut stats);
        }
    }
    anyhow::ensure!(
        stats.failed == 0,
        "{} files couldn't be copied; the archive stays where it was",
        stats.failed
    );
    store.pref_set(
        PREF_DIR,
        &folder.map_or(String::new(), |_| new.to_string_lossy().into_owned()),
    )?;
    // 偏好已经指向新位置:旧目录删不掉只是多占盘,不算失败
    if let Err(e) = std::fs::remove_dir_all(&old) {
        eprintln!("[vault] couldn't remove {}: {e}", old.display());
    }
    Ok(new)
}

/// 归档的磁盘占用(文件逻辑大小之和)。APFS 上源文件还在时副本是 clone,实际多占的
/// 比这个数小;逐文件 stat,放后台线程算
pub fn cache_bytes(vault: &Path) -> u64 {
    walkdir::WalkDir::new(vault)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// 建好归档根。roster 按"目录在不在"决定挂不挂归档实例,所以要在组 roster
/// **之前**调;不调(测试、`--tmp` 隔离库)就完全没有归档这回事
pub fn ensure(store: &Store) -> Option<PathBuf> {
    let dir = dir_for(store)?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 归档已启用时给出 (home, vault 根);watcher 每批事件问一次
pub fn active(store: &Store) -> Option<(PathBuf, PathBuf)> {
    let vault = dir_for(store)?;
    if !vault.is_dir() {
        return None;
    }
    Some((crate::adapters::home_dir()?, vault))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncStats {
    /// 新拷或更新了的文件
    pub copied: usize,
    /// 与归档一致,没动
    pub unchanged: usize,
    /// 源比归档短,保留了归档那份
    pub kept: usize,
    pub failed: usize,
}

/// 把本机 home 下各家的会话文件增量镜像进归档。逐文件 stat,没变的不读;
/// 阻塞执行,调用方放后台线程(扫描之前跑一次)
pub fn sync_for(store: &Store) -> Option<SyncStats> {
    let (home, vault) = active(store)?;
    Some(sync(&home, &vault))
}

pub fn sync(home: &Path, vault: &Path) -> SyncStats {
    let mut stats = SyncStats::default();
    for layout in VAULT_LAYOUTS {
        for rel in layout.sync_paths {
            let src_root = home.join(rel);
            // 不跟符号链接:链接指到 home 外的东西不归这一家,也防环
            for entry in walkdir::WalkDir::new(&src_root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let Ok(tail) = entry.path().strip_prefix(home) else {
                    continue;
                };
                mirror(entry.path(), &vault.join(tail), &mut stats);
            }
        }
    }
    stats
}

/// watcher 的增量入口:`path` 落在某家的镜像范围内就拷这一份。返回是否写了归档
pub fn mirror_path(home: &Path, vault: &Path, path: &Path) -> bool {
    let Ok(tail) = path.strip_prefix(home) else {
        return false;
    };
    let in_scope = VAULT_LAYOUTS
        .iter()
        .flat_map(|l| l.sync_paths.iter())
        .any(|rel| tail.starts_with(rel));
    if !in_scope {
        return false;
    }
    let is_file = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    if !is_file {
        return false;
    }
    let mut stats = SyncStats::default();
    mirror(path, &vault.join(tail), &mut stats);
    stats.copied > 0
}

/// 只增不减只对转录成立(追加写的 `.jsonl`):skill、记忆、SQLite 边车都是会被正常
/// 改短的文件,变了就整份覆盖
fn append_only(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

fn mirror(src: &Path, dst: &Path, stats: &mut SyncStats) {
    let Ok(src_meta) = std::fs::metadata(src) else {
        // 枚举到 stat 之间没了:下一轮再说,不算失败
        return;
    };
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        let same_time = match (src_meta.modified(), dst_meta.modified()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if src_meta.len() == dst_meta.len() && same_time {
            stats.unchanged += 1;
            return;
        }
        if append_only(src) && src_meta.len() < dst_meta.len() {
            stats.kept += 1;
            return;
        }
    }
    match copy_atomic(src, dst, &src_meta) {
        Ok(()) => stats.copied += 1,
        Err(e) => {
            eprintln!("[vault] {} -> {}: {e}", src.display(), dst.display());
            stats.failed += 1;
        }
    }
}

/// 先拷到同目录的临时名再 rename:归档里永远不会有写了一半的会话文件。
/// 临时名不以 `.jsonl` 结尾,各家枚举不会把它当会话。mtime 跟源对齐,下一轮
/// 靠 (大小, mtime) 判没变。macOS 上 `fs::copy` 走 clonefile,APFS 同卷不占额外空间
fn copy_atomic(src: &Path, dst: &Path, src_meta: &std::fs::Metadata) -> std::io::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    std::fs::create_dir_all(parent)?;
    let name = dst
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.wake-tmp"));
    let result = (|| {
        std::fs::copy(src, &tmp)?;
        if let Ok(modified) = src_meta.modified() {
            let file = std::fs::OpenOptions::new().write(true).open(&tmp)?;
            file.set_modified(modified)?;
        }
        std::fs::rename(&tmp, dst)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn mirrors_bytes_and_survives_source_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, vault) = (tmp.path().join("home"), tmp.path().join("vault"));
        let rel = ".claude/projects/-p/abc.jsonl";
        write(&home.join(rel), "{\"a\":1}\n");
        write(&home.join(".claude/settings.json"), "secret");

        let stats = sync(&home, &vault);
        assert_eq!((stats.copied, stats.failed), (1, 0));
        assert_eq!(
            std::fs::read(vault.join(rel)).unwrap(),
            std::fs::read(home.join(rel)).unwrap()
        );
        // 范围外的文件不进归档
        assert!(!vault.join(".claude/settings.json").exists());

        // 没变:不重拷
        assert_eq!(sync(&home, &vault).unchanged, 1);

        // 源删了:归档原样留着
        std::fs::remove_file(home.join(rel)).unwrap();
        assert_eq!(sync(&home, &vault), SyncStats::default());
        assert_eq!(
            std::fs::read_to_string(vault.join(rel)).unwrap(),
            "{\"a\":1}\n"
        );
    }

    #[test]
    fn grows_with_source_but_never_shrinks() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, vault) = (tmp.path().join("home"), tmp.path().join("vault"));
        let rel = ".codex/sessions/2026/01/01/rollout-x.jsonl";
        write(&home.join(rel), "one\n");
        sync(&home, &vault);

        write(&home.join(rel), "one\ntwo\n");
        assert_eq!(sync(&home, &vault).copied, 1);
        assert_eq!(
            std::fs::read_to_string(vault.join(rel)).unwrap(),
            "one\ntwo\n"
        );

        // 源被截断:保留更完整的归档
        write(&home.join(rel), "x\n");
        assert_eq!(sync(&home, &vault).kept, 1);
        assert_eq!(
            std::fs::read_to_string(vault.join(rel)).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn sqlite_sidecar_is_overwritten_even_when_smaller() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, vault) = (tmp.path().join("home"), tmp.path().join("vault"));
        let rel = ".codex/state_5.sqlite";
        write(&home.join(rel), "bigger-content");
        sync(&home, &vault);
        write(&home.join(rel), "small");
        assert_eq!(sync(&home, &vault).copied, 1);
        assert_eq!(std::fs::read_to_string(vault.join(rel)).unwrap(), "small");
    }

    #[test]
    fn mirror_path_only_takes_files_in_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, vault) = (tmp.path().join("home"), tmp.path().join("vault"));
        let inside = home.join(".claude/projects/-p/abc.jsonl");
        let outside = home.join(".claude/history.jsonl");
        write(&inside, "a\n");
        write(&outside, "b\n");
        assert!(mirror_path(&home, &vault, &inside));
        assert!(!mirror_path(&home, &vault, &outside));
        assert!(!mirror_path(
            &home,
            &vault,
            &home.join(".claude/projects/-p")
        ));
        assert!(vault.join(".claude/projects/-p/abc.jsonl").exists());
        assert!(!vault.join(".claude/history.jsonl").exists());
    }
}
