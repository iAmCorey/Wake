//! POSIX 双端(macOS/Linux)共享层:login shell 探测、单引号 quote、
//! shell 命令拼装、file:// 编码、剪贴板管道。整个模块由 mod.rs 以**正向**
//! cfg 圈定——新平台不会静默继承这里的任何前提,想用得自己点名。
//! macos.rs / linux.rs 经 `super::` 取用(mod.rs 中转 re-export)。

use std::collections::HashMap;
use std::process::Command;

const CLI_PROBE_MARKER: &str = "__WAKE_CLI__";

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

/// 批量探测缺失 bin 的绝对路径:login shell 里 `command -v`,把用户 rc 文件
/// 加进 PATH 的目录一并覆盖(GUI 进程 PATH 不含 ~/.local/bin 等)。每条记录
/// 先换行再打固定标记,rc 链打到 stdout 的无换行文本或 ANSI/OSC 噪声
/// 都会留在上一行;解析器只接受本次请求且带标记的记录。
/// Windows 的对应物在 windows.rs(注册表 PATH 天然完整,纯 Rust 遍历)。
pub(super) fn probe_clis(missing: &[&str]) -> HashMap<String, String> {
    let script = missing
        .iter()
        .map(|b| {
            format!(
                "printf '\\n{}\\t%s\\t' {b}; command -v {b} || printf '\\n'",
                CLI_PROBE_MARKER
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let out = Command::new(login_shell()).args(["-lic", &script]).output();
    out.ok()
        .map(|o| parse_probe_output(&o.stdout, missing))
        .unwrap_or_default()
}

fn parse_probe_output(stdout: &[u8], requested: &[&str]) -> HashMap<String, String> {
    let stdout = String::from_utf8_lossy(stdout);
    let mut found = HashMap::new();

    for line in stdout.lines() {
        let Some(record) = line
            .strip_prefix(CLI_PROBE_MARKER)
            .and_then(|rest| rest.strip_prefix('\t'))
        else {
            continue;
        };
        let Some((name, path)) = record.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if requested.contains(&name) && path.starts_with('/') {
            found.insert(name.to_string(), path.to_string());
        }
    }

    found
}

/// 展示/剪贴板/启动共用的一条可执行命令。POSIX 双端只有一种 shell 方言,
/// 与宿主无关,`_term` 只为对齐 Windows 的同名接缝(那边按宿主分
/// cmd/PowerShell 两派,见 windows.rs);拼装本体在 mod.rs 的
/// sh_command_line(quote 与连接规则的唯一实现,远程 ssh 同用)
pub(super) fn compose_command(
    _term: super::TerminalApp,
    cli: &str,
    args: &[String],
    cwd: Option<&str>,
) -> String {
    super::sh_command_line(cli, args, cwd)
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
    use super::parse_probe_output;

    #[test]
    fn probe_parser_ignores_osc_title_before_record() {
        let stdout = b"\x1b]0;Wake\x07\n__WAKE_CLI__\tomp\t/Users/loosheng/Library/pnpm/omp\n";

        let found = parse_probe_output(stdout, &["omp"]);

        assert_eq!(
            found.get("omp").map(String::as_str),
            Some("/Users/loosheng/Library/pnpm/omp")
        );
    }

    #[test]
    fn probe_parser_ignores_unterminated_osc_before_record() {
        let stdout = b"\x1b]0;Wake\n__WAKE_CLI__\tomp\t/opt/bin/omp\n";

        let found = parse_probe_output(stdout, &["omp"]);

        assert_eq!(found.get("omp").map(String::as_str), Some("/opt/bin/omp"));
    }

    #[test]
    fn probe_parser_ignores_unmarked_noise() {
        let stdout = b"noise\t/tmp/fake\n__WAKE_CLI__\tomp\t/opt/bin/omp\n";

        let found = parse_probe_output(stdout, &["omp"]);

        assert_eq!(found.len(), 1);
        assert_eq!(found.get("omp").map(String::as_str), Some("/opt/bin/omp"));
    }

    #[test]
    fn probe_parser_filters_missing_and_unrequested_clis() {
        let stdout = b"__WAKE_CLI__\tomp\t/opt/bin/omp\n\
__WAKE_CLI__\tcodex\t\n\
__WAKE_CLI__\tgemini\t/opt/bin/gemini\n";

        let found = parse_probe_output(stdout, &["omp", "codex"]);

        assert_eq!(found.len(), 1);
        assert_eq!(found.get("omp").map(String::as_str), Some("/opt/bin/omp"));
    }
}
