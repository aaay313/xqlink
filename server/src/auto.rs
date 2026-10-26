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
        pub fn PostMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> i32;
        pub fn FindWindowExW(parent: *mut c_void, after: *mut c_void, class: *const u16, title: *const u16) -> *mut c_void;
        pub fn SendMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
        pub fn ScreenToClient(hwnd: *mut c_void, pt: *mut Point) -> i32;
        pub fn ClientToScreen(hwnd: *mut c_void, pt: *mut Point) -> i32;
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    /// Win32 POINTER_INFO（x64 布局）。字段顺序必须与 SDK 一致，
    /// 否则 InjectTouchInput 会读错偏移 —— 全零初始化后只填必要字段。
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PointerInfo {
        pub pointer_type: i32,
        pub pointer_id: u32,
        pub frame_id: u32,
        pub pointer_flags: u32,
        pub source_device: usize,
        pub hwnd_target: usize,
        pub pt_pixel_location: Point,
        pub pt_himetric_location: Point,
        pub pt_pixel_location_raw: Point,
        pub pt_himetric_location_raw: Point,
        pub time: u32,
        pub history_count: u32,
        pub input_state: i32,
        pub key_states: u32,
        pub performance_count: u64,
        pub button_change_type: i32,
    }

    impl Default for PointerInfo {
        fn default() -> Self {
            // SAFETY: 全部是 POD（整数与 POINT），全零是合法初值。
            unsafe { std::mem::zeroed() }
        }
    }

    /// Win32 POINTER_TOUCH_INFO（x64 布局）。
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PointerTouchInfo {
        pub pointer_info: PointerInfo,
        pub touch_flags: u32,
        pub touch_mask: u32,
        pub rc_contact: Rect,
        pub rc_contact_raw: Rect,
        pub orientation: u32,
        pub pressure: u32,
    }

    impl Default for PointerTouchInfo {
        fn default() -> Self {
            unsafe { std::mem::zeroed() }
        }
    }

    pub const PT_TOUCH: i32 = 0x0000_0002;
    pub const TOUCH_FLAG_NONE: u32 = 0;
    pub const TOUCH_MASK_CONTACTAREA: u32 = 0x1;
    pub const TOUCH_MASK_ORIENTATION: u32 = 0x2;
    pub const TOUCH_MASK_PRESSURE: u32 = 0x4;
    /// 无视觉反馈 —— 屏幕上不会出现"触摸圆点"。
    pub const TOUCH_FEEDBACK_NONE: u32 = 0x3;
    pub const POINTER_FLAG_NEW: u32 = 0x0000_0001;
    pub const POINTER_FLAG_INRANGE: u32 = 0x0000_0002;
    pub const POINTER_FLAG_INCONTACT: u32 = 0x0000_0004;
    pub const POINTER_FLAG_DOWN: u32 = 0x0001_0000;
    pub const POINTER_FLAG_UP: u32 = 0x0004_0000;

    unsafe extern "system" {
        pub fn InitializeTouchInjection(max_count: u32, feedback: u32) -> i32;
        pub fn InjectTouchInput(count: u32, contacts: *const PointerTouchInfo) -> i32;
    }

    /// SW_RESTORE —— 目标窗口最小化时先还原，否则点击不会落到它身上。
    pub const SW_RESTORE: i32 = 9;

    pub const WM_MOUSEMOVE: u32 = 0x0200;
    pub const WM_LBUTTONDOWN: u32 = 0x0201;
    pub const WM_LBUTTONUP: u32 = 0x0202;
    pub const MK_LBUTTON: usize = 0x0001;
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
    /// 目标窗口句柄（窗口消息方式需要）。
    pub hwnd: u32,
}

impl ScreenMap {
    /// 把「红方在下」视角的棋盘坐标 (row, col) 换算成**客户区物理坐标**。
    ///
    /// 两步变换：
    /// 1. board 数组恒为「红方在下」视角（`chess::board_fix` 已按 camp 翻转过），
    ///    而裁剪区图像是屏幕方向 —— 黑方在下时行列都要反。
    /// 2. 裁剪区是「棋盘框 ± 半格」，故棋盘框在裁剪区内占 10 格宽 / 11 格高，
    ///    格子 col 的中心位于 `w * (col + 1) / 10`。
    ///
    /// 客户区物理坐标是 `PostMessage` 的 lParam 需要的量（消息坐标是
    /// 客户区坐标，不是屏幕坐标）。
    pub fn board_to_client(&self, camp: &Camp, row: usize, col: usize) -> Option<(i32, i32)> {
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
        // → 客户区图像坐标 → 客户区物理坐标
        let cx = (bx as f32 + px) / self.scale;
        let cy = (by as f32 + py) / self.scale;
        Some((cx.round() as i32, cy.round() as i32))
    }

    /// 把棋盘坐标换算成屏幕绝对坐标（`SendInput` / `SetCursorPos` 用）。
    ///
    /// 优先走系统 API `ClientToScreen`：它与 `ScreenToClient` 严格互逆，
    /// 不受进程 DPI 感知状态影响。hwnd 缺失时（如单元测试）退回按原点相加。
    pub fn board_to_screen(&self, camp: &Camp, row: usize, col: usize) -> Option<(i32, i32)> {
        let (cx, cy) = self.board_to_client(camp, row, col)?;
        if let Some(p) = client_to_screen(self.hwnd, cx, cy) {
            return Some(p);
        }
        Some((self.origin.0 + cx, self.origin.1 + cy))
    }
}

/// 目标窗口的几何信息 —— 标定按钮位置时用它做坐标换算。
#[derive(Debug, Clone, Copy)]
pub struct AutoFrame {
    /// 客户区左上角的屏幕坐标（物理像素）
    pub origin: (i32, i32),
    /// 客户区图像像素 / 物理像素（DPI 缩放比）
    pub scale: f32,
    /// 窗口句柄
    pub hwnd: u32,
}

/// 触摸注入：在屏幕坐标 (x, y) 处点一下 —— **不移动鼠标指针**。
///
/// 走 `InjectTouchInput`（产生 `WM_POINTER` 触摸消息），与鼠标轨道无关。
/// Chromium 在页面未绑定 touch 事件时会把触摸**合成**为鼠标点击，
/// 因此对 QQ 小程序这类 Chromium 应用是一条可能的"不动鼠标"通路。
///
/// 注意：仍需目标窗口处于前台；触摸坐标是屏幕物理像素。
#[cfg(target_os = "windows")]
pub fn touch_tap(x: i32, y: i32) -> bool {
    use std::sync::OnceLock;
    static TOUCH_READY: OnceLock<bool> = OnceLock::new();

    // 触摸注入需先按进程初始化一次（仅首次调用真正执行）
    let ready = *TOUCH_READY.get_or_init(|| unsafe {
        ffi::InitializeTouchInjection(1, ffi::TOUCH_FEEDBACK_NONE) != 0
    });
    if !ready {
        return false;
    }

    let mut contact = ffi::PointerTouchInfo::default();
    contact.pointer_info.pointer_type = ffi::PT_TOUCH;
    contact.pointer_info.pointer_id = 0;
    contact.pointer_info.pt_pixel_location = ffi::Point { x, y };
    contact.pointer_info.pointer_flags =
        ffi::POINTER_FLAG_DOWN | ffi::POINTER_FLAG_INRANGE | ffi::POINTER_FLAG_INCONTACT;
    contact.touch_flags = ffi::TOUCH_FLAG_NONE;
    contact.touch_mask =
        ffi::TOUCH_MASK_CONTACTAREA | ffi::TOUCH_MASK_ORIENTATION | ffi::TOUCH_MASK_PRESSURE;
    // 接触区域 4x4 像素，够小以免误触相邻格
    contact.rc_contact = ffi::Rect {
        left: x - 2,
        top: y - 2,
        right: x + 2,
        bottom: y + 2,
    };
    contact.rc_contact_raw = contact.rc_contact;
    contact.orientation = 90;
    contact.pressure = 32000;

    unsafe {
        if ffi::InjectTouchInput(1, &contact) == 0 {
            return false;
        }
        sleep(Duration::from_millis(60));
        // 抬起：只需 UP 标志，压力归零
        contact.pointer_info.pointer_flags = ffi::POINTER_FLAG_UP;
        contact.pressure = 0;
        ffi::InjectTouchInput(1, &contact) != 0
    }
}

#[cfg(not(target_os = "windows"))]
pub fn touch_tap(_x: i32, _y: i32) -> bool {
    false
}

/// 屏幕坐标 → 客户区坐标。
///
/// **不要**自己算「鼠标屏幕坐标 − 窗口原点」：那要求两侧处在同一套 DPI 坐标系，
/// 而程序不是 DPI 感知时 `GetCursorPos` 返回逻辑坐标、xcap 返回物理坐标，
/// 相减会得到负数（实测标定结果 x 全是 -129~-168，导致认输点完全错位）。
/// `ScreenToClient` 由系统负责一致性，且与 `ClientToScreen` 严格互逆。
#[cfg(target_os = "windows")]
pub fn screen_to_client(hwnd: u32, x: i32, y: i32) -> Option<(i32, i32)> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut pt = Point { x, y };
        if ffi::ScreenToClient(hwnd as *mut std::ffi::c_void, &mut pt) == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn screen_to_client(_hwnd: u32, _x: i32, _y: i32) -> Option<(i32, i32)> {
    None
}

/// 客户区坐标 → 屏幕坐标。
#[cfg(target_os = "windows")]
pub fn client_to_screen(hwnd: u32, x: i32, y: i32) -> Option<(i32, i32)> {
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut pt = Point { x, y };
        if ffi::ClientToScreen(hwnd as *mut std::ffi::c_void, &mut pt) == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn client_to_screen(_hwnd: u32, _x: i32, _y: i32) -> Option<(i32, i32)> {
    None
}

/// 输入方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    /// 自动：先试后台消息（渲染子窗口，不动鼠标不抢焦点）→ 触摸注入
    /// （不动鼠标但需前台）→ 系统输入（动鼠标）。
    Auto,
    /// 仅用窗口消息（`SendMessage`）—— **完全不移动鼠标指针**。
    ///
    /// 只对用标准 Win32 消息处理点击的程序有效。
    /// **对 Chromium / Electron / QQ 小程序 / WebView 一律无效** ——
    /// 它们的输入由浏览器进程经 IPC 转发给渲染进程，不读窗口消息。
    Message,
    /// 仅用系统输入（`SendInput`）—— 真实鼠标事件，兼容性最好。
    ///
    /// QQ 小程序（Chromium 内核）只能走这条：实测 JJ 象棋是
    /// `Chrome_WidgetWin_0` + `WeChatAppEx.exe`（WMPF 小程序运行时），
    /// 且未开启调试端口，不存在绕过系统输入的可用通道。
    #[default]
    SendInput,
    /// 触摸注入（`InjectTouchInput`）—— **不移动鼠标指针**。
    ///
    /// 发的是 `WM_POINTER` 触摸消息，与鼠标轨道无关；Chromium 在页面未绑定
    /// touch 事件时会把它合成为鼠标点击。对 QQ 小程序值得一试，
    /// 但需目标窗口在前台，且页面若只监听 mouse 事件则可能无效。
    Touch,
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

/// 通过**窗口消息**点击客户区坐标 —— 完全不移动鼠标指针。
///
/// 直接给目标窗口发 `WM_LBUTTONDOWN/UP`，坐标是客户区物理坐标
/// （消息坐标属于客户区，不是屏幕坐标）。
///
/// 这里用 **`SendMessage`（同步）而非 `PostMessage`（异步）**：
/// `PostMessage` 只是把消息投进目标线程的消息队列，而游戏/Flash 的渲染循环
/// 通常不调用 `GetMessage`/`PeekMessage`，消息会一直躺在队列里不被处理。
/// `SendMessage` 直接调用窗口过程（必要时跨线程封送），只要该窗口自己处理
/// 鼠标消息就会立即生效 —— 对 Flash ActiveX、Qt 自绘窗口都更可靠。
///
/// 限制：目标是 DirectX 自绘 + 原始输入（Raw Input）的程序时不响应，
/// 此时需退回 `SendInput`。
#[cfg(target_os = "windows")]
pub fn post_click(hwnd: u32, client_x: i32, client_y: i32) -> bool {
    if hwnd == 0 {
        return false;
    }
    // lParam = (y << 16) | x，均为 16 位有符号的客户区坐标
    let lp = (((client_y as u32) & 0xFFFF) << 16) | ((client_x as u32) & 0xFFFF);
    unsafe {
        let h = hwnd as *mut std::ffi::c_void;
        // 先发移动消息：不少程序靠鼠标移动维护 hover/选中态
        ffi::SendMessageW(h, ffi::WM_MOUSEMOVE, 0, lp as isize);
        sleep(Duration::from_millis(25));
        ffi::SendMessageW(h, ffi::WM_LBUTTONDOWN, ffi::MK_LBUTTON, lp as isize);
        sleep(Duration::from_millis(40));
        ffi::SendMessageW(h, ffi::WM_LBUTTONUP, 0, lp as isize);
    }
    true
}

#[cfg(not(target_os = "windows"))]
pub fn post_click(_hwnd: u32, _client_x: i32, _client_y: i32) -> bool {
    false
}

/// 找 Chromium 的渲染子窗口 `Chrome_RenderWidgetHostHWND`。
///
/// Chromium 真正接收鼠标输入的是这个子窗口，不是顶层 `Chrome_WidgetWin_0`。
/// 对**顶层**发窗口消息无效，不代表对渲染子窗口无效 —— 后台点击（不动鼠标、
/// 不抢焦点）的唯一希望就在它身上。
#[cfg(target_os = "windows")]
fn find_render_child(hwnd: u32) -> Option<u32> {
    const CLASS: &[u16] = &[0x0043, 0x0068, 0x0072, 0x006F, 0x006D, 0x0065, 0x005F,
        0x0052, 0x0065, 0x006E, 0x0064, 0x0065, 0x0072, 0x0057, 0x0069, 0x0064,
        0x0067, 0x0065, 0x0074, 0x0048, 0x006F, 0x0073, 0x0074, 0x0048, 0x0057,
        0x004E, 0x0044, 0x0000]; // "Chrome_RenderWidgetHostHWND\0"
    let child = unsafe {
        ffi::FindWindowExW(
            hwnd as *mut std::ffi::c_void,
            std::ptr::null_mut(),
            CLASS.as_ptr(),
            std::ptr::null(),
        )
    };
    if child.is_null() {
        None
    } else {
        Some(child as u32)
    }
}

#[cfg(not(target_os = "windows"))]
fn find_render_child(_hwnd: u32) -> Option<u32> {
    None
}

/// 后台点击：向渲染子窗口投递鼠标消息。**不移动鼠标指针、不抢焦点**。
///
/// 传入的坐标是顶层窗口客户区坐标；内部换算到子窗口客户区再投递。
/// 返回 false 表示没有渲染子窗口（非 Chromium 窗口），调用方自行降级。
#[cfg(target_os = "windows")]
pub fn post_click_background(hwnd: u32, client_x: i32, client_y: i32) -> bool {
    let Some(child) = find_render_child(hwnd) else {
        return false;
    };
    // 顶层客户区 → 屏幕 → 子窗口客户区（子窗口原点未必与顶层重合，交给系统换算）
    let Some((sx, sy)) = client_to_screen(hwnd, client_x, client_y) else {
        return false;
    };
    let Some((cx, cy)) = screen_to_client(child, sx, sy) else {
        return false;
    };
    post_click(child, cx, cy)
}

#[cfg(not(target_os = "windows"))]
pub fn post_click_background(_hwnd: u32, _client_x: i32, _client_y: i32) -> bool {
    false
}

/// 点击屏幕坐标，**点完把鼠标指针放回原处**。
///
/// 系统输入方式无法避免移动指针，但可以"用完即还"，用户基本不被打断。
#[cfg(target_os = "windows")]
pub fn click_at_restoring(x: i32, y: i32) -> bool {
    let saved = cursor_position();
    let ok = click_at(x, y);
    if let Some((sx, sy)) = saved {
        unsafe {
            ffi::SetCursorPos(sx, sy);
        }
    }
    ok
}

#[cfg(not(target_os = "windows"))]
pub fn click_at_restoring(_x: i32, _y: i32) -> bool {
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
///
/// `input` 决定输入通道：
/// - `Message` —— 窗口消息，不动鼠标指针
/// - `SendInput` —— 系统输入，兼容性最好，点了会把指针放回原处
/// - `Auto` —— 先按 `Message` 发（不动鼠标）；是否真的生效由调用方
///   检查盘面变化后决定是否降级
pub fn play_move(
    map: &ScreenMap,
    camp: &Camp,
    from: (usize, usize),
    to: (usize, usize),
    mode: PlayMode,
    input: InputMode,
    clear: Option<(usize, usize)>,
) -> bool {
    match mode {
        // 拖拽依赖连续的移动轨迹，窗口消息难以还原，只走系统输入
        PlayMode::Drag => {
            let Some((fx, fy)) = map.board_to_screen(camp, from.0, from.1) else {
                return false;
            };
            let Some((tx, ty)) = map.board_to_screen(camp, to.0, to.1) else {
                return false;
            };
            drag_from_to(fx, fy, tx, ty, 12)
        }
        PlayMode::Click => {
            // 三个通道只是「怎么点」不同，点哪些格子/间隔完全一致。
            let click_cell = |cell: (usize, usize)| -> bool {
                match input {
                    // Message / Auto：后台点击 —— 向 Chromium 渲染子窗口投递鼠标
                    // 消息，不移动鼠标指针、不抢焦点。
                    InputMode::Message | InputMode::Auto => {
                        let Some((cx, cy)) = map.board_to_client(camp, cell.0, cell.1) else {
                            return false;
                        };
                        post_click_background(map.hwnd, cx, cy)
                    }
                    InputMode::Touch => {
                        let Some((x, y)) = map.board_to_screen(camp, cell.0, cell.1) else {
                            return false;
                        };
                        touch_tap(x, y)
                    }
                    InputMode::SendInput => {
                        let Some((x, y)) = map.board_to_screen(camp, cell.0, cell.1) else {
                            return false;
                        };
                        click_at_restoring(x, y)
                    }
                }
            };

            // 先点一个空格清掉可能残留的选中态。
            // JJ 的交互是「点棋子选中 → 点目标落子」；上一次尝试若在落子那步失败，
            // 棋子仍处于选中状态，下一次重试的首击会把选中**取消**，导致必须
            // 奇数次尝试才能成功（日志里 3 次、11 次成功正是这个模式）。
            // 每次点击前先归零选中态，重试才是幂等的。
            if let Some(cc) = clear {
                if !click_cell(cc) {
                    return false;
                }
                sleep(Duration::from_millis(150));
            }
            if !click_cell(from) {
                return false;
            }
            sleep(Duration::from_millis(220));
            click_cell(to)
        }
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
            hwnd: 0,
        }
    }

    #[test]
    fn board_to_client_uses_client_coordinates() {
        // 客户区坐标不含屏幕原点 —— PostMessage 的 lParam 用的就是它
        let (x, y) = map().board_to_client(&Camp::Red, 0, 0).unwrap();
        assert_eq!((x, y), (40, 50));
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

