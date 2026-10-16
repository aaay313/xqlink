use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::engine::EngineConfig;
use crate::engine::Strength;
use crate::engine::Style;
use crate::SHARED_STATE;

#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    config_path: Option<PathBuf>,
    // trace, debug, info, wran, silent
    pub loglevel: String,
    pub timer_interval: u64,
    pub confirm_interval: u64,

    pub engine: EngineConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: None,
            loglevel: "INFO".to_string(),
            timer_interval: 100,
            confirm_interval: 200,
            engine: Default::default(),
        }
    }
}

impl Config {
    pub fn load(base: &Path) -> Self {
        let dir = base.join("xqlink");
        if !dir.exists() {
            let _ = fs::create_dir(&dir);
        };

        let config_path = dir.join("config.json");
        debug!("load config from '{}'", config_path.display());

        if config_path.exists() {
            // 读取为字符串并去掉可能存在的 UTF-8 BOM。
            //
            // 背景：用 Windows PowerShell 的 `Set-Content -Encoding UTF8`、
            // 或记事本"另存为 UTF-8"保存过配置时，文件会带上 BOM；而 serde_json
            // 不认 BOM，会解析失败 —— 触发下面的"重建默认值"分支，
            // 用户的全部设置（含 time/depth 等影响响应速度的参数）在下次启动时
            // 会被悄悄重置为默认值。这里统一剥离 BOM，避免该问题。
            let text = fs::read_to_string(&config_path).unwrap_or_default();
            let text = text.trim_start_matches('\u{feff}');
            if let Ok(mut config) = serde_json::from_str::<Config>(text) {
                match config.config_path {
                    Some(ref path) => {
                        if path != &config_path {
                            config.config_path = Some(config_path);
                            config.save();
                        }
                    }
                    None => {
                        config.config_path = Some(config_path);
                        config.save();
                    }
                };
                return config;
            };

            // 解析失败代表配置不兼容。**先备份再重建** —— 原实现直接删除，
            // 用户遇到时所有设置无声丢失，既无法排查也无法恢复。
            let backup = config_path.with_extension("json.bak");
            let _ = fs::rename(&config_path, &backup);
            debug!("配置解析失败，已备份到 '{}' 并重建默认配置", backup.display())
        }

        let config = Config { config_path: Some(config_path), ..Default::default() };
        config.save();
        config
    }

    pub fn save(&self) {
        let path = self.config_path.as_ref().unwrap();
        debug!("save config to '{}'", path.display());
        // 将对象序列化为格式化的JSON字符串
        let json_string = serde_json::to_string_pretty(self).unwrap();

        // 创建一个新文件或打开一个已存在的文件以写入JSON数据
        let mut file = File::create(path).unwrap();
        file.write_all(json_string.as_bytes()).unwrap();
    }
}

#[tauri::command]
pub async fn get_engine_config() -> EngineConfig { SHARED_STATE.get().unwrap().config.read().unwrap().engine.clone() }

#[tauri::command]
pub async fn set_engine_depth(depth: usize) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.depth = depth;
    config.save();
    debug!("set_engine_depth: {}", depth);
}

#[tauri::command]
pub async fn set_engine_time(time: f32) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    // 前端 Toolbar.vue 传来的是毫秒(秒 × 1000)，此处不可再乘 1000。
    // 原实现多乘了一次，界面上设 1 秒会写成 1_000_000 ms，等于把时间限制取消。
    config.engine.time = time.max(0.0) as usize;
    config.save();
    debug!("set_engine_time: {}", time);
}

#[tauri::command]
pub async fn set_engine_threads(num: usize) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.threads = num;
    config.save();
    debug!("set_engine_threads: {}", num);
}

#[tauri::command]
pub async fn set_engine_hash(size: usize) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.hash = size;
    config.save();
    debug!("set_engine_hash: {}", size);
}

#[tauri::command]
pub async fn set_engine_strength(strength: Strength) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.strength = strength;
    config.save();
    debug!("set_engine_strength: {:?}", strength);
}

#[tauri::command]
pub async fn set_engine_style(style: Style) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.style = style;
    config.save();
    debug!("set_engine_style: {:?}", style);
}

#[tauri::command]
pub async fn set_flying_knife(enabled: bool, knife: String) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.engine.flying_knife = enabled;
    config.engine.knife = knife.clone();
    config.save();
    debug!("set_flying_knife: {} -> {}", enabled, knife);
}

#[tauri::command]
pub async fn set_chessdb(enabled: bool, timeout: Option<u64>) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    let timeout = timeout.unwrap_or_else(|| config.engine.chessdb_timeout.min(1));
    config.engine.chessdb_enabled = enabled;
    config.engine.chessdb_timeout = timeout;
    config.save();
    debug!("set_chessdb: {} -> {}", enabled, timeout);
}
