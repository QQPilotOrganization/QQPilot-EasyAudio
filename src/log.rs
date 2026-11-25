//! 极简日志：控制台 + 可选文件。时间戳使用"程序启动以来的秒数"，避免额外依赖。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// 初始化日志；`path` 为 `None` 时只写控制台。
pub fn init(path: Option<&Path>) {
    START.get_or_init(Instant::now);
    if let Some(path) = path {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                let _ = FILE.set(Mutex::new(file));
            }
            Err(error) => eprintln!("[WARN] 无法打开日志文件 {}: {error}", path.display()),
        }
    }
}

fn stamp() -> String {
    let secs = START
        .get()
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    format!("[{secs:>9.3}s]")
}

fn emit(line: &str) {
    println!("{line}");
    if let Some(file) = FILE.get()
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

pub fn info(message: impl AsRef<str>) {
    emit(&format!("{} [INFO] {}", stamp(), message.as_ref()));
}

pub fn warn(message: impl AsRef<str>) {
    emit(&format!("{} [WARN] {}", stamp(), message.as_ref()));
}

pub fn error(message: impl AsRef<str>) {
    emit(&format!("{} [ERROR] {}", stamp(), message.as_ref()));
}
