//! 不走 Store 的偏好文件:外观在开窗前就要读,主窗几何在 Workbench(与它的
//! Store)建立之前就要用,都等不了库。统一目录、统一原子写法
use std::path::PathBuf;

/// `<config dir>/wake/<name>`
fn path(name: &str) -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wake")
        .join(name)
}

/// 读整个文件并去掉首尾空白;不存在、读不到或为空 = None
pub fn read(name: &str) -> Option<String> {
    let text = std::fs::read_to_string(path(name)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 建目录、先写临时文件再 rename——半截文件不会被下次启动读成坏内容
pub fn write(name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = path(name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}
