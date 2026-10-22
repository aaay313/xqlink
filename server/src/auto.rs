//! 自动操作：屏幕输入模拟 + 棋盘坐标到屏幕坐标的换算。
//!
//! 目标软件为 JJ 象棋：走子用「点起点 → 点终点」的方式，认输按钮位置由用户标定。
//!
//! 这里手写 Win32 FFI 而不引入 `windows` crate，与 `engine/command.rs` 里手写
//! CreatePipe 的做法一致：本机工具链是 x86_64-pc-windows-gnu（w64devkit），
//! 引入 MSVC 目标的 `windows` crate 会带来不必要的构建风险。

use std::thread::sleep;
use std::time::Duration;

use crate::chess::Camp;

// ---------------------------------------------------------------------------
// Win32 FFI
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod ffi {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MouseInput {
        pub dx: i32,
        pub dy: i32,
        pub mouse_data: u32,
        pub dw_flags: u32,
        pub time: u32,
        pub dw_extra_info: usize,
    }

    /// 与 Win32 INPUT 结构体布局一致：type(4) + 对齐填充(4) + MOUSEINPUT(24) = 32 字节。
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Input {
        pub r#type: u32,
        pub mi: MouseInput,
    }

    impl Input {
        pub fn mouse(flags: u32) -> Self {
            Self {
                r#type: 0, // INPUT_MOUSE
                mi: MouseInput {
                    dx: 0,
                    dy: 0,
                    mouse_data: 0,
                    dw_flags: flags,
                    time: 0,
                    dw_extra_info: 0,
                },
            }
        }
    }

    pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;

    unsafe extern "system" {
        pub fn SendInput(num: u32, inputs: *mut Input, size: i32) -> u32;
        pub fn SetCursorPos(x: i32, y: i32) -> i32;
        pub fn GetCursorPos(pt: *mut Point) -> i32;
        pub fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        pub fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    /// SW_RESTORE —— 目标窗口最小化时先还原，否则点击不会落到它身上。
    pub const SW_RESTORE: i32 = 9;
}

#[cfg(target_os = "windows")]
use ffi::{
    Input,
    MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP,
    Point,
    SW_RESTORE,
};

/// 走子方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlayMode {
    /// 点起点 → 再点终点（JJ 象棋等多数软件的方式）。
    #[default]
    Click,
    /// 按住起点拖到终点。
    Drag,
}

/// 目标窗口在屏幕上的位置与客户区信息。
///
/// 坐标换算需要三样东西：
/// - `origin`：客户区左上角的**屏幕绝对坐标**（物理像素）
/// - `scale`：客户区**图像像素 / 物理像素**（DPI 缩放比）
/// - `board_rect`：棋盘裁剪区在客户区**图像像素**中的位置
#[derive(Debug, Clone, Copy)]
pub struct ScreenMap {
    pub origin: (i32, i32),
    pub scale: f32,
    /// 棋盘裁剪区（客户区图像坐标）：(x, y, w, h)
    pub board_rect: (u32, u32, u32, u32),
}

impl ScreenMap {
    /// 把「红方在下」视角的棋盘坐标 (row, col) 换算成屏幕绝对坐标。
    ///
    /// 两步变换：
    /// 1. board 数组恒为「红方在下」视角（`chess::board_fix` 已按 camp 翻转过），
    ///    而裁剪区图像是屏幕方向 —— 黑方在下时行列都要反。
    /// 2. 裁剪区是「棋盘框 ± 半格」，故棋盘框在裁剪区内占 10 格宽 / 11 格高，
    ///    格子 col 的中心位于 `w * (col + 1) / 10`。
    pub fn board_to_screen(&self, camp: &Camp, row: usize, col: usize) -> Option<(i32, i32)> {
        if self.scale <= 0.0 {
            return None;
        }
        let (img_row, img_col) = match camp {
            Camp::Black => (9usize.checked_sub(row)?, 8usize.checked_sub(col)?),
            _ => (row, col),
        };
        let (bx, by, bw, bh) = self.board_rect;
        if bw == 0 || bh == 0 {
            return None;
        }
        // 裁剪区内的像素位置
        let px = bw as f32 * (img_col as f32 + 1.0) / 10.0;
        let py = bh as f32 * (img_row as f32 + 1.0) / 11.0;
        // → 客户区图像坐标 → 客户区物理坐标 → 屏幕绝对坐标
        let cx = bx as f32 + px;
        let cy = by as f32 + py;
        let sx = self.origin.0 as f32 + cx / self.scale;
        let sy = self.origin.1 as f32 + cy / self.scale;
        Some((sx.round() as i32, sy.round() as i32))
    }
}

/// 读取当前鼠标位置（屏幕绝对坐标）。用于标定「认输」按钮位置。
#[cfg(target_os = "windows")]
pub fn cursor_position() -> Option<(i32, i32)> {
    unsafe {
        let mut pt = Point::default();
        if ffi::GetCursorPos(&mut pt) == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn cursor_position() -> Option<(i32, i32)> {
    None
}

/// 把目标窗口切到前台并还原（最小化时）。自动点击必须先做这一步，
/// 否则点击会落到当前活动窗口上。
#[cfg(target_os = "windows")]
pub fn focus_window(hwnd: u32) {
    unsafe {
        let h = hwnd as *mut std::ffi::c_void;
        ffi::ShowWindow(h, SW_RESTORE);
        ffi::SetForegroundWindow(h);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn focus_window(_hwnd: u32) {}

/// 在屏幕坐标 (x, y) 处点击一次。
#[cfg(target_os = "windows")]
pub fn click_at(x: i32, y: i32) -> bool {
    unsafe {
        if ffi::SetCursorPos(x, y) == 0 {
            return false;
        }
        // 给目标程序一个处理"光标已移动"的时间，否则部分软件会忽略这次点击
        sleep(Duration::from_millis(60));
        let mut inputs = [
            Input::mouse(MOUSEEVENTF_LEFTDOWN),
            Input::mouse(MOUSEEVENTF_LEFTUP),
        ];
        let sent = ffi::SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<Input>() as i32,
        );
        sent == inputs.len() as u32
    }
}

#[cfg(not(target_os = "windows"))]
pub fn click_at(_x: i32, _y: i32) -> bool {
    false
}

/// 从 (fx, fy) 拖拽到 (tx, ty)。
#[cfg(target_os = "windows")]
pub fn drag_from_to(fx: i32, fy: i32, tx: i32, ty: i32, steps: u32) -> bool {
    unsafe {
        if ffi::SetCursorPos(fx, fy) == 0 {
            return false;
        }
        sleep(Duration::from_millis(60));
        let mut down = [Input::mouse(MOUSEEVENTF_LEFTDOWN)];
        if ffi::SendInput(1, down.as_mut_ptr(), std::mem::size_of::<Input>() as i32) != 1 {
            return false;
        }
        // 分步移动：一次性瞬移的部分软件识别不到拖拽轨迹
        let steps = steps.max(2);
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let x = fx + ((tx - fx) as f32 * t).round() as i32;
            let y = fy + ((ty - fy) as f32 * t).round() as i32;
            ffi::SetCursorPos(x, y);
            sleep(Duration::from_millis(25));
        }
        sleep(Duration::from_millis(60));
        let mut up = [Input::mouse(MOUSEEVENTF_LEFTUP)];
        ffi::SendInput(1, up.as_mut_ptr(), std::mem::size_of::<Input>() as i32) == 1
    }
}

#[cfg(not(target_os = "windows"))]
pub fn drag_from_to(_fx: i32, _fy: i32, _tx: i32, _ty: i32, _steps: u32) -> bool {
    false
}

/// 按给定方式走一步棋。
pub fn play_move(map: &ScreenMap, camp: &Camp, from: (usize, usize), to: (usize, usize), mode: PlayMode) -> bool {
    let (fx, fy) = match map.board_to_screen(camp, from.0, from.1) {
        Some(p) => p,
        None => return false,
    };
    let (tx, ty) = match map.board_to_screen(camp, to.0, to.1) {
        Some(p) => p,
        None => return false,
    };
    match mode {
        PlayMode::Click => {
            if !click_at(fx, fy) {
                return false;
            }
            sleep(Duration::from_millis(220));
            click_at(tx, ty)
        }
        PlayMode::Drag => drag_from_to(fx, fy, tx, ty, 12),
    }
}

#[cfg(test)]
mod tests {
    use super::PlayMode;
    use super::ScreenMap;
    use crate::chess::Camp;

    /// 构造一个便于心算的映射：裁剪区 300×330，屏幕原点 (100,200)，缩放比 1。
    ///
    /// 裁剪区是「棋盘框 ± 半格」，所以棋盘框只占 10 格宽 / 11 格高：
    /// 格子中心在裁剪区内位于 `w*(col+1)/10` 与 `h*(row+1)/11`。
    fn map() -> ScreenMap {
        ScreenMap {
            origin: (100, 200),
            scale: 1.0,
            board_rect: (10, 20, 300, 330),
        }
    }

    #[test]
    fn red_bottom_left_corner_maps_to_upper_left_of_board() {
        // 红方在下：board 的 (row0,col0) = 黑方底线左端 = 屏幕左上角
        let (x, y) = map().board_to_screen(&Camp::Red, 0, 0).unwrap();
        assert_eq!((x, y), (140, 250), "裁剪区 x=10 + 300/10 = 40 → 屏幕 100+40");
    }

    #[test]
    fn red_bottom_maps_to_screen_bottom_right() {
        // 红方在下：board 的 (row9,col8) = 红方底线右端 = 屏幕右下角
        // 裁剪区内坐标：x = 300*9/10 = 270 → 客户区 10+270 = 280 → 屏幕 100+280 = 380
        //               y = 330*10/11 = 300 → 客户区 20+300 = 320 → 屏幕 200+320 = 520
        let (x, y) = map().board_to_screen(&Camp::Red, 9, 8).unwrap();
        assert_eq!((x, y), (380, 520));
    }

    #[test]
    fn black_bottom_flips_both_axes() {
        // 黑方在下：board 已被 board_fix 翻转，屏幕方向与红方在下相反
        let (x, y) = map().board_to_screen(&Camp::Black, 0, 0).unwrap();
        assert_eq!((x, y), (380, 520), "黑方在下时 (0,0) 应落在屏幕右下角");
    }

    #[test]
    fn scale_factor_applies_to_physical_pixels() {
        let mut m = map();
        m.scale = 2.0; // 200% 缩放：图像像素是物理像素的 2 倍
        let (x, y) = m.board_to_screen(&Camp::Red, 0, 0).unwrap();
        assert_eq!((x, y), (120, 225), "图像坐标 40,50 除以缩放比 2 再加屏幕原点");
    }

    #[test]
    fn invalid_scale_returns_none() {
        let mut m = map();
        m.scale = 0.0;
        assert!(m.board_to_screen(&Camp::Red, 0, 0).is_none());
    }

    #[test]
    fn play_mode_default_is_click() {
        assert_eq!(PlayMode::default(), PlayMode::Click);
    }
}

