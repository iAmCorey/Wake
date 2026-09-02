//! 不走 Store 的偏好文件:外观在开窗前就要读,主窗几何在 Workbench(与它的
//! Store)建立之前就要用,都等不了库。统一目录、统一原子写法
use std::path::{Path, PathBuf};

/// `<config dir>/wake/<name>`
pub fn path(name: &str) -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wake")
        .join(name)
}

/// 建目录、先写临时文件再 rename——半截文件不会被下次启动读成坏内容
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}
