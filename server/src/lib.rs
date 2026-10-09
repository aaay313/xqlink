use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::thread;

use engine::Engine;
use tauri::Manager as _;

mod chess;
mod common;
mod config;
mod engine;
mod listen;
mod logger;
mod worker;
mod yolo;

// 全局共享状态，用Arc和Mutex包装以实现线程安全共享
struct SharedState {
    config: Arc<RwLock<config::Config>>,
    engine: Arc<Mutex<Engine>>,
    listen_thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// 最近一次推送给界面的盘面 FEN，供界面"复制局面"使用
    latest_fen: Mutex<String>,
    /// 本局棋谱（中文着法），由监听线程在每步走子时追加
    history: Mutex<Vec<worker::MoveRecord>>,
}

static SHARED_STATE: OnceLock<SharedState> = OnceLock::new();

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // 先读配置，再按配置里的 loglevel 初始化日志。
            // 原实现固定用 DEBUG，忽略了 config.loglevel，导致识别主循环高频写盘。
            let config = config::Config::load(&app.path().config_dir().unwrap());
            logger::init_tracer(logger::parse_level(&config.loglevel), &app.path().app_data_dir().unwrap());

            let _ = SHARED_STATE.get_or_init(|| {
                let lib_path = app.path().resolve("../libs/pikafish", tauri::path::BaseDirectory::Resource).unwrap();
                let mut engine = engine::Engine::new(&lib_path);
                engine.set_show_wdl(config.engine.show_wdl);
                engine.set_hash(config.engine.hash);
                engine.set_threads(config.engine.threads);

                SharedState {
                    config: Arc::new(RwLock::new(config)),
                    engine: Arc::new(Mutex::new(engine)),
                    listen_thread: Mutex::new(None),
                    latest_fen: Mutex::new(String::new()),
                    history: Mutex::new(Vec::new()),
                }
            });

            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            reload_engine,
            current_fen,
            game_history,
            save_history,
            listen::list_windows,
            worker::start_listen,
            worker::stop_listen,
            config::get_engine_config,
            config::set_engine_depth,
            config::set_engine_time,
            config::set_engine_threads,
            config::set_engine_hash,
            config::set_chessdb,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
fn reload_engine(app: tauri::AppHandle) {
    let lib_path = app.path().resolve("../libs/pikafish", tauri::path::BaseDirectory::Resource).unwrap();
    let state = SHARED_STATE.get().unwrap();
    let engine_config = state.config.read().unwrap().engine;
    state.engine.lock().unwrap().reload(&lib_path, &engine_config);
}

/// 返回最近一次识别出的盘面 FEN，供界面"复制局面"使用。
/// 未在监听、或尚未识别出盘面时返回空串。
#[tauri::command]
fn current_fen() -> String {
    SHARED_STATE
        .get()
        .and_then(|state| state.latest_fen.lock().ok().map(|fen| fen.clone()))
        .unwrap_or_default()
}

/// 返回本局棋谱（按时间顺序的中文着法），供界面展示与导出。
#[tauri::command]
fn game_history() -> Vec<worker::MoveRecord> {
    SHARED_STATE
        .get()
        .and_then(|state| state.history.lock().ok().map(|history| history.clone()))
        .unwrap_or_default()
}

/// 把当前棋谱保存成文本文件，返回文件路径。
///
/// 棋谱原先只存在内存里，关掉程序就没了；导出后可以留档、也可以拿去分析。
#[tauri::command]
fn save_history(app: tauri::AppHandle, stamp: String) -> Result<String, String> {
    let state = SHARED_STATE.get().ok_or("程序尚未初始化")?;
    let history = state.history.lock().map_err(|_| "读取棋谱失败")?.clone();
    if history.is_empty() {
        return Err("还没有记录到走子".to_string());
    }

    // 文件名来自前端传的时间戳，过滤掉路径分隔符等特殊字符
    let safe: String = stamp.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let dir = app.path().config_dir().map_err(|e| e.to_string())?.join("games");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("game-{safe}.txt"));

    let mut text = String::from("中国象棋 棋谱\n");
    text.push_str(&format!("共 {} 步\n\n", history.len()));
    for (i, m) in history.iter().enumerate() {
        text.push_str(&format!(
            "{:>3}. {:<10} {:>7}{}\n",
            i + 1,
            m.chinese,
            m.score,
            if m.ours { "  （我方）" } else { "" }
        ));
    }
    text.push_str("\n注：分数为走出该手之前的局面评估，正数对我方有利。\n");

    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}
