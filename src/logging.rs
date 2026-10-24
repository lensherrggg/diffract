use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};

use crate::config::LogConfig;

struct FileLogger {
    file: Mutex<File>,
    level: LevelFilter,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "{} [{:<5}] {} - {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            );
            let _ = file.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

pub fn parse_level(value: &str) -> Result<LevelFilter> {
    match value.to_ascii_lowercase().as_str() {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        other => bail!("日志级别非法：{other}"),
    }
}

/// 日志只写文件，绝不写 stderr/stdout，避免污染 TUI 界面或 CI 输出。
pub fn init(config: &LogConfig) -> Result<()> {
    let level = parse_level(&config.level)?;
    let Some(path) = &config.path else {
        log::set_max_level(LevelFilter::Off);
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建日志目录失败：{}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开日志文件失败：{}", path.display()))?;

    log::set_boxed_logger(Box::new(FileLogger {
        file: Mutex::new(file),
        level,
    }))
    .map_err(|err| anyhow::anyhow!("初始化日志失败：{err}"))?;
    log::set_max_level(level);
    Ok(())
}
