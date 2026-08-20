pub mod adapters;
pub mod db;
pub mod models;
pub mod scanner;
pub mod services;
pub mod watcher;

/// Resolve the user's agent-data home directory.
///
/// Production uses the OS-native home directory. `WAKE_HOME` is an explicit
/// override for deterministic tests and portable/demo datasets.
pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("WAKE_HOME")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default()
}
