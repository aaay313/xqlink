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
    /// 认输**第一步**：JJ 象棋底部工具栏里「投降」入口的位置（点它弹出工具菜单）。
    ///
    /// 坐标是客户区**图像像素**（界面标定写入）。None = 未标定。
    pub resign_menu_point: Option<(i32, i32)>,
    /// 认输**第二步**：弹出菜单里「退出 / 投降」按钮的位置。
    pub resign_confirm_point: Option<(i32, i32)>,
    /// 认输**第三步**：确认弹窗（"退出将判负，确定要退出吗？"）里的「退出」按钮。
    pub resign_dialog_point: Option<(i32, i32)>,
    /// 对局结束后「开始下一局」按钮的位置。
    pub next_game_point: Option<(i32, i32)>,
    /// 认输后是否自动开始下一局。
    pub auto_next_game: bool,
}

impl AutoConfig {
    /// 清除无效的标定坐标，返回是否发生了改动。
    ///
    /// 旧版本用「鼠标屏幕坐标 − 窗口原点」换算标定点，在非 DPI 感知的进程里
    /// 两侧坐标系不一致，会算出负数（实测出现 x = -168 这类坐标）。
    /// 这类坐标必然点不中任何按钮，启动时直接清空让用户重新标定。
    pub fn sanitize_points(&mut self) -> bool {
        fn bad(p: &Option<(i32, i32)>) -> bool {
            matches!(p, Some((x, y)) if *x < 0 || *y < 0)
        }
        let mut changed = false;
        for slot in [
            &mut self.resign_menu_point,
            &mut self.resign_confirm_point,
            &mut self.resign_dialog_point,
            &mut self.next_game_point,
        ] {
            if bad(slot) {
                *slot = None;
                changed = true;
            }
        }
        if changed {
            tracing::warn!("检测到无效的负坐标标定点（旧版本 DPI 换算缺陷），已清除，请重新标定");
        }
        changed
    }
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            auto_play: false,
            play_mode: PlayMode::default(),
            input_mode: crate::auto::InputMode::default(),
            auto_resign_limit: 0,
            resign_count: 0,
            resign_menu_point: None,
            resign_confirm_point: None,
            resign_dialog_point: None,
            next_game_point: None,
            auto_next_game: true,
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

/// 递归复制目录（std 没有现成实现，配置目录只有两层：config.json + games/）。
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if let Err(e) = fs::copy(&from, &to) {
            tracing::warn!("迁移 {} 失败: {e}", from.display());
        }
    }
    Ok(())
}

impl Config {
    pub fn load(base: &Path) -> Self {
        let dir = base.join("yimou");
        // 从旧版（xqlink）目录一次性迁移：认输点位等标定值是用户手工点出来的，
        // 改名不该让它失效。
        let legacy = base.join("xqlink");
        if !dir.exists() && legacy.exists() {
            if copy_dir(&legacy, &dir).is_ok() {
                tracing::info!("已迁移旧配置目录 {} → {}", legacy.display(), dir.display());
            }
        }
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
                // 清理旧版本因 DPI 换算缺陷留下的负坐标标定点
                if config.auto.sanitize_points() {
                    config.save();
                }
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

/// 标定界面上的某个按钮位置：读取当前鼠标位置，换算成客户区图像坐标后保存。
///
/// `target` 取值：
/// - `resign_menu`    —— 认输第一步：底部工具栏的「投降」入口（点它弹出菜单）
/// - `resign_confirm` —— 认输第二步：弹出菜单里的「退出 / 投降」
/// - `resign_dialog`  —— 认输第三步：确认弹窗里的「退出」
/// - `next_game`      —— 对局结束后的「开始下一局」
///
/// 存的是**客户区坐标**而非屏幕坐标 —— 窗口移动、分辨率变化后依然准确。
#[tauri::command]
pub async fn calibrate_point(target: String) -> Result<(i32, i32), String> {
    let (sx, sy) = crate::auto::cursor_position().ok_or("无法读取鼠标位置")?;
    let state = SHARED_STATE.get().ok_or("程序尚未初始化")?;
    let frame = state
        .auto_frame
        .lock()
        .map_err(|_| "读取窗口信息失败")?
        .ok_or("尚未开始监听，请先点「启」后再标定")?;

    // 交给系统换算屏幕坐标 → 客户区坐标，避免与 DPI 缩放混用坐标系
    //（自己相减会得到负数，实测标定出 -168 这种不可用坐标）。
    let (cx, cy) =
        crate::auto::screen_to_client(frame.hwnd, sx, sy).ok_or("坐标换算失败（窗口句柄无效）")?;
    let mut config = state.config.write().unwrap();
    match target.as_str() {
        "resign_menu" => config.auto.resign_menu_point = Some((cx, cy)),
        "resign_confirm" => config.auto.resign_confirm_point = Some((cx, cy)),
        "resign_dialog" => config.auto.resign_dialog_point = Some((cx, cy)),
        "next_game" => config.auto.next_game_point = Some((cx, cy)),
        other => return Err(format!("未知的标定目标: {other}")),
    }
    config.save();
    debug!(
        "calibrate_point {}: screen({},{}) -> client({},{})",
        target, sx, sy, cx, cy
    );
    Ok((cx, cy))
}

/// 清除所有已标定的按钮位置。
#[tauri::command]
pub async fn clear_auto_points() {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.resign_menu_point = None;
    config.auto.resign_confirm_point = None;
    config.auto.resign_dialog_point = None;
    config.auto.next_game_point = None;
    config.save();
    debug!("clear_auto_points");
}

/// 开关「认输后自动开始下一局」。
#[tauri::command]
pub async fn set_auto_next_game(enabled: bool) {
    let state = SHARED_STATE.get().unwrap();
    let mut config = state.config.write().unwrap();
    config.auto.auto_next_game = enabled;
    config.save();
    debug!("set_auto_next_game: {}", enabled);
}

/// 请求**立即**执行一次认输（无视额度限制）。
///
/// 两个用途：
/// 1. 验证三个标定点是否点得准 —— 不消耗额度也不用等开局；
/// 2. 中途启动监听（当前不是开局局面）时手动触发一次。
#[tauri::command]
pub async fn request_resign() -> Result<(), String> {
    let state = SHARED_STATE.get().ok_or("程序尚未初始化")?;
    *state.resign_request.lock().map_err(|_| "状态锁失败")? = true;
    debug!("已请求立即认输");
    Ok(())
}

/// 读取当前鼠标的屏幕绝对坐标（标定流程用）。
#[tauri::command]
pub async fn capture_cursor_pos() -> Option<(i32, i32)> {
    crate::auto::cursor_position()
}
