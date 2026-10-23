use std::fs;
use std::path::Path;
use std::path::PathBuf;

use tracing::Level;
use tracing_appender::rolling;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry;
use tracing_subscriber::EnvFilter;

#[cfg(target_os = "windows")]
fn log_dir(home_dir: &Path) -> PathBuf { home_dir.join("logs") }

#[cfg(target_os = "macos")]
fn log_dir(home_dir: &Path) -> PathBuf { home_dir.join("logs") }

#[cfg(target_os = "linux")]
fn log_dir(home_dir: &Path) -> PathBuf { home_dir.join("logs") }

/// 把配置项 `loglevel` 解析为 tracing 级别，默认 INFO。
///
/// 说明: 原实现完全忽略 config.loglevel，固定使用 DEBUG，导致识别主循环
/// 每 ~0.6s 就写一次日志(实测 30 分钟 1359 条 DEBUG)，带来持续的磁盘 I/O 开销。
pub fn parse_level(s: &str) -> Level {
    match s.trim().to_ascii_uppercase().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" | "WARNING" => Level::WARN,
        "ERROR" | "SILENT" => Level::ERROR,
        _ => Level::INFO,
    }
}

/// 初始化tracing库，设置全局订阅者
pub fn init_tracer(level: Level, home_dir: &std::path::Path) {
    // 创建环境过滤器
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 默认过滤级别设置
        let filter_str = format!(
            "{},xqlink={},xqlink_lib={},ort=warn,tower_http=warn,hyper=warn,hyper_util=warn,xcap=warn",
            level.as_str(),
            level.as_str(),
            level.as_str()
        );
        EnvFilter::new(filter_str)
    });

    let log_dir = log_dir(home_dir);

    if !log_dir.exists() {
        fs::create_dir_all(&log_dir).expect("无法创建日志目录");
    }

    // 设置每日滚动日志文件。
    // 直接同步写：之前用 non_blocking 缓冲 + WorkerGuard，本程序是窗口进程，
    // 退出时 guard 不可靠，实测日志整段丢失（文件 0 字节），事后无法排查。
    // INFO 级别每 0.6s 至多几条，同步写的磁盘开销可以接受。
    let file_appender = rolling::daily(log_dir, "runtime.log");

    // 创建控制台输出层
    let console_layer = tracing_subscriber::fmt::layer().with_span_events(FmtSpan::CLOSE).with_ansi(true).compact();

    // 创建文件输出层
    let file_layer = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(file_appender)
        .compact();

    // 组装并设置订阅者
    registry().with(filter).with(console_layer).with(file_layer).init();
}
