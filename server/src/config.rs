use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::auto::PlayMode;
use crate::engine::EngineConfig;
use crate::engine::Strength;
use crate::engine::Style;
use crate::SHARED_STATE;

/// 自动操作配置（自动走棋 / 自动认输）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AutoConfig {
    /// 是否自动替我方走棋。
    pub auto_play: bool,
    /// 走子方式：点起点→点终点（JJ 象棋），或按住拖拽。
    pub play_mode: PlayMode,
    /// 输入通道：
    /// - `auto` —— 先用窗口消息（不动鼠标），无响应则自动降级为系统输入
    /// - `message` —— 只用窗口消息，完全不移动鼠标指针
    /// - `sendinput` —— 只用系统输入（兼容性最好，点了会把指针放回原处）
    pub input_mode: crate::auto::InputMode,
    /// **最多**自动认输多少把（0 = 不自动认输）。
    ///
    /// 语义是"认输额度"：设成 3 表示程序自动下棋时，前 3 把会在
    /// **对方走出第一步之后**（即那一把的开局）点一次认输；
    /// 认输满 3 把后自动认输功能停用，之后的棋局正常自动下，不再认输。
    /// 已认输的把数记在 `resign_count`（持久化，跨监听重启累计）。
    pub auto_resign_limit: u32,
    /// 已自动认输的把数（持久化）。
    pub resign_count: u32,
    /// 「认输」按钮在客户区**图像像素**中的坐标（由界面标定写入）。
    /// None = 尚未标定。
    pub resign_point: Option<(i32, i32)>,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            auto_play: false,
            play_mode: PlayMode::default(),
            input_mode: crate::auto::InputMode::default(),
            auto_resign_limit: 0,
            resign_count: 0,
            resign_point: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    config_path: Option<PathBuf>,
    // trace, debug, info, wran, silent
    pub loglevel: String,
    pub timer_interval: u64,
    pub confirm_interval: u64,

    pub engine: EngineConfig,
    /// 自动操作设置。`#[serde(default)]` 保证旧配置文件缺该字段时平滑升级。
    #[serde(default)]
    pub auto: AutoConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: None,
            loglevel: "INFO".to_string(),
            timer_interval: 100,
            confirm_interval: 200,
            engine: Default::default(),
            auto: Default::default(),
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

// ---------------------------------------------------------------------------
// 自动操作（自动走棋 / 自动认输）
// ---------------------------------------------------------------------------

/// 读取当前自动操作配置，供界面回显。
#[tauri::command]
pub async fn get_auto_config() -> AutoConfig {
    SHARED_STATE.get().unwrap().config.read().unwrap().auto.clone()
}

/// 开关自动走棋。
#[tauri::command]
pub async fn set_auto_play(enabled: bool) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.auto_play = enabled;
    config.save();
    debug!("set_auto_play: {}", enabled);
}

/// 设置走子方式（点击起终点 / 拖拽）。
#[tauri::command]
pub async fn set_auto_play_mode(mode: PlayMode) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.play_mode = mode;
    config.save();
    debug!("set_auto_play_mode: {:?}", mode);
}

/// 设置输入通道（自动 / 窗口消息 / 系统输入）。
#[tauri::command]
pub async fn set_auto_input_mode(mode: crate::auto::InputMode) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.input_mode = mode;
    config.save();
    debug!("set_auto_input_mode: {:?}", mode);
}

/// 设置「最多自动认输多少把」（0 = 不自动认输）。
///
/// 改这个设置会同时把已认输把数清零 —— 否则改完数字也不会再触发。
#[tauri::command]
pub async fn set_auto_resign_limit(count: u32) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.auto_resign_limit = count;
    config.auto.resign_count = 0;
    config.save();
    debug!("set_auto_resign_limit: {}", count);
}

/// 重置已认输把数（下次重新计额度）。
#[tauri::command]
pub async fn reset_resign_count() {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.resign_count = 0;
    config.save();
    debug!("reset_resign_count");
}

/// 标定「认输」按钮：读取当前鼠标位置，换算成目标窗口客户区图像坐标后保存。
///
/// 存的是**客户区坐标**而非屏幕坐标 —— 这样窗口移动、分辨率变化后
/// 依然能准确定位按钮。调用前请先把鼠标移到 JJ 象棋的「认输」按钮上。
#[tauri::command]
pub async fn calibrate_resign_point() -> Result<(i32, i32), String> {
    let (sx, sy) = crate::auto::cursor_position().ok_or("无法读取鼠标位置")?;
    let state = SHARED_STATE.get().ok_or("程序尚未初始化")?;
    let frame = state
        .auto_frame
        .lock()
        .map_err(|_| "读取窗口信息失败")?
        .clone()
        .ok_or("尚未开始监听，请先点「启」后再标定")?;
    let (ox, oy, scale) = frame;
    let cx = ((sx - ox) as f32 * scale).round() as i32;
    let cy = ((sy - oy) as f32 * scale).round() as i32;
    let mut config = state.config.write().unwrap();
    config.auto.resign_point = Some((cx, cy));
    config.save();
    debug!("calibrate_resign_point: screen({},{}) -> client({},{})", sx, sy, cx, cy);
    Ok((cx, cy))
}

/// 清除已标定的「认输」按钮位置。
#[tauri::command]
pub async fn clear_resign_point() {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.resign_point = None;
    config.save();
    debug!("clear_resign_point");
}

/// 读取当前鼠标的屏幕绝对坐标（标定流程用）。
#[tauri::command]
pub async fn capture_cursor_pos() -> Option<(i32, i32)> {
    crate::auto::cursor_position()
}
