//! POSIX 双端(macOS/Linux)共享层:login shell 探测、单引号 quote、
//! shell 命令拼装、file:// 编码、剪贴板管道。整个模块由 mod.rs 以**正向**
//! cfg 圈定——新平台不会静默继承这里的任何前提,想用得自己点名。
//! macos.rs / linux.rs 经 `super::` 取用(mod.rs 中转 re-export)。

use std::collections::HashMap;
use std::process::Command;

/// 解析 PATH 用的 login shell:macOS 固定 zsh(系统默认);Linux 尊重 $SHELL
/// (bash/zsh/fish 对 `-lic` 与 `command -v` 语义一致),缺省 /bin/bash
fn login_shell() -> String {
    if cfg!(target_os = "macos") {
        return "/bin/zsh".to_string();
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| "/bin/bash".to_string())
}

/// 剥离 ANSI 转义序列与游离 BEL/CR。login shell 的 rc 链会往 stdout 打
/// 终端标题转义(oh-my-zsh/starship 等,常不带换行),不剥会把
/// `\x1b]0;…\aomp` 整段粘成 bin 名,`command -v` 找到了也恒判 miss。
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // OSC:ESC ] … BEL,或 ESC \(ST) 收尾
            0x1b if bytes.get(i + 1) == Some(&b']') => {
                i += 2;
                while i < bytes.len() {
                    match bytes[i] {
                        0x07 => {
                            i += 1;
                            break;
                        }
                        0x1b if bytes.get(i + 1) == Some(&b'\\') => {
                            i += 2;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            // CSI / 字符集选择:ESC [ 或 ESC ( 到 0x40..=0x7e 终止字节
            0x1b if matches!(bytes.get(i + 1), Some(b'[' | b'(')) => {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
            }
            // 其余 ESC 序列按双字节处理(ESC 7 / ESC M;尾部孤 ESC 直接越界收敛)
            0x1b => i += 2,
            0x07 | b'\r' => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 批量探测缺失 bin 的绝对路径:login shell 里 `command -v`,把用户 rc 文件
/// 加进 PATH 的目录一并覆盖(GUI 进程 PATH 不含 ~/.local/bin 等)。rc 链的
/// stdout 噪声两手防:脚本开头强制换行,吸收非转义的无换行残尾;输出再经
/// strip_ansi 剥掉转义序列,防 `\x1b]0;…\aomp` 整段粘成 bin 名。
/// Windows 的对应物在 windows.rs(注册表 PATH 天然完整,纯 Rust 遍历)。
pub(super) fn probe_clis(missing: &[&str]) -> HashMap<String, String> {
    let script = format!(
        "printf '\\n'; {}",
        missing
            .iter()
            .map(|b| format!("printf '%s\\t' {b}; command -v {b} || echo"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    let out = Command::new(login_shell()).args(["-lic", &script]).output();
    let stdout = out
        .ok()
        .map(|o| strip_ansi(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let mut found = HashMap::new();
    for line in stdout.lines() {
        if let Some((name, path)) = line.split_once('\t') {
            if path.starts_with('/') {
                found.insert(name.trim().to_string(), path.trim().to_string());
            }
        }
    }
    found
}

/// POSIX 单引号 quote
pub(crate) fn posix_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 展示/剪贴板/启动共用的一条可执行命令。POSIX 双端只有一种 shell 方言,
/// 与宿主无关,`_term` 只为对齐 Windows 的同名接缝(那边按宿主分
/// cmd/PowerShell 两派,见 windows.rs)
pub(super) fn compose_command(
    _term: super::TerminalApp,
    cli: &str,
    args: &[String],
    cwd: Option<&str>,
) -> String {
    let core = std::iter::once(cli)
        .chain(args.iter().map(|s| s.as_str()))
        .map(posix_quote)
        .collect::<Vec<_>>()
        .join(" ");
    match cwd {
        Some(dir) => format!("cd {} && {core}", posix_quote(dir)),
        None => core,
    }
}

/// 保守 percent-encode(RFC 3986 unreserved 之外全编;keep_slash 供
/// file:// URL 保路径分隔)。POSIX 两端共用,编码集只此一份。
pub(crate) fn percent_encode(s: &str, keep_slash: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// 把 text 经管道写给 `bin args…` 的 stdin(剪贴板工具用),按退出码报成败
/// ——"copied to clipboard" 的用户提示以此为据,不能 spawn 成功就算数。
/// 三家 Linux 工具与 pbcopy 都在拿到内容后自行 fork 常驻,wait 即刻返回。
/// Windows 不走子进程管道(clip.exe 按控制台 codepage 解码,非 ASCII 必乱),
/// windows.rs 直接调 Win32 剪贴板。
pub(crate) fn pipe_to(bin: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_keeps_plain_probe_line() {
        assert_eq!(
            strip_ansi("omp\t/usr/local/bin/omp\n"),
            "omp\t/usr/local/bin/omp\n"
        );
    }

    /// 用户机器实测样本:zsh rc 在探测输出前打了不带换行的 OSC 标题
    /// (`\x1b]0;Wake\a`,标题取 cwd basename),粘掉 bin 名导致恒 miss。
    #[test]
    fn strip_ansi_removes_osc_title_glued_onto_probe_line() {
        let raw = "\x1b]0;Wake\x07omp\t/Users/loosheng/Library/pnpm/omp\n";
        assert_eq!(strip_ansi(raw), "omp\t/Users/loosheng/Library/pnpm/omp\n");
    }

    #[test]
    fn strip_ansi_removes_csi_and_trailing_osc() {
        let raw = "omp\t/opt/omp\x1b[0m\x1b]2;title\x1b\\";
        assert_eq!(strip_ansi(raw), "omp\t/opt/omp");
    }
}
