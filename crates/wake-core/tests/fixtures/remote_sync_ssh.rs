//! A native SSH stand-in for remote_sync_paths: the real rsync sender reads only
//! synthetic directories. No shell, network connection or SSH credentials are used.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    while args.peek().is_some_and(|arg| arg.starts_with("-o")) {
        args.next();
    }
    let host = args.next().expect("SSH host");
    assert!(matches!(host.as_str(), "devbox" | "otherbox"));
    let home = PathBuf::from(std::env::var_os("WAKE_TEST_REMOTE_HOME").unwrap()).join(host);
    let args: Vec<_> = args.collect();
    if args.len() == 1 && args[0].starts_with("sh -c ") {
        assert!(args[0].contains(".claude/projects"));
        if home.join(".claude/projects").is_dir() {
            println!(".claude/projects");
        }
        return;
    }
    assert_eq!(args[0], "rsync");
    assert!(args.iter().any(|arg| arg == "--server"));
    assert!(args.iter().any(|arg| arg == "--sender"));
    let status = Command::new("rsync")
        .args(&args[1..])
        .current_dir(home)
        .status()
        .expect("start local rsync sender");
    std::process::exit(status.code().unwrap_or(1));
}
