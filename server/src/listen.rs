use serde::Deserialize;
use serde::Serialize;
use xcap::image;
use xcap::image::GenericImage;

#[derive(Serialize, Deserialize, Debug)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub width: u32,
    pub height: u32,
}

impl Window {
    /// 读取窗口信息。取不到信息的窗口直接跳过（返回 None），
    /// 不再让个别异常窗口把整个列表拉垮。
    pub fn new(win: &xcap::Window) -> Option<Self> {
        Some(Self {
            id: win.id().ok()?,
            title: win.title().unwrap_or_default(),
            app_name: win.app_name().unwrap_or_default(),
            width: win.width().unwrap_or_default(),
            height: win.height().unwrap_or_default(),
        })
    }
}

#[tauri::command]
pub async fn list_windows() -> Result<Vec<Window>, String> {
    let windows = xcap::Window::all().map_err(|e| e.to_string())?;
    if windows.is_empty() {
        return Err("no window".to_string());
    }
    let result: Vec<Window> = windows.iter().filter_map(Window::new).collect();
    Ok(result)
}

pub struct ListenWindow {
    window: xcap::Window,

    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// 由棋子中心拟合的真实格点仿射系数（客户区图像像素）
    lattice: Option<[f32; 6]>,

    /// 客户区图像（**未裁剪**）的尺寸。
    ///
    /// 捕获到的图像是物理像素 × DPI 缩放比，用它除以客户区物理尺寸
    /// 就能精确还原缩放比 —— 自动点击换算屏幕坐标时需要这个比例。
    client_size: std::cell::Cell<(u32, u32)>,
}

impl ListenWindow {
    #[tracing::instrument]
    pub fn new(target: &Window, w: usize, h: usize) -> Option<Self> {
        let windows = xcap::Window::all().ok()?;
        for window in windows {
            if window.id().ok() == Some(target.id) {
                return Some(Self {
                    window,
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                    lattice: None,
                    client_size: std::cell::Cell::new((0, 0)),
                });
            }
        }
        None
    }

    /// 抓取目标区域。
    ///
    /// 返回 None 表示抓图失败（窗口已关闭、被系统拒绝等）。调用方应跳过本帧，
    /// 而不是让整个程序崩掉 —— 这是长时间挂机时最容易被触发的故障点。
    pub fn capture(&self) -> Option<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
        let mut pic = self.window.capture_image().ok()?;
        // 先记下未裁剪的客户区尺寸（DPI 换算要用），再裁剪。
        self.client_size.set((pic.width(), pic.height()));
        // 窗口大小可能被用户改动，裁剪前必须重新校验边界：
        // 原实现直接 sub_image，一旦缓存的区域超出新窗口尺寸就会 panic。
        if self.w > 0
            && self.x + self.w <= pic.width()
            && self.y + self.h <= pic.height()
        {
            pic = pic.sub_image(self.x, self.y, self.w, self.h).to_image();
        }
        Some(pic)
    }

    /// 抓取**整个窗口**（不裁剪）。供头像读秒检测使用 —— 头像在棋盘抓取区之外。
    pub fn capture_full(&self) -> Option<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
        self.window.capture_image().ok()
    }

    /// 棋盘抓取区（含半格缓冲）在窗口内的边界；尚未设置时返回 None。
    pub fn board_bound(&self) -> Option<(u32, u32, u32, u32)> {
        if self.w == 0 || self.h == 0 {
            return None;
        }
        Some((self.x, self.y, self.w, self.h))
    }

    /// 客户区图像像素与物理像素之比，即 DPI 缩放比。
    pub fn scale_factor(&self) -> f32 {
        let (img_w, _) = self.client_size.get();
        let phys = self.window.width().unwrap_or(0);
        if img_w == 0 || phys == 0 {
            return 1.0;
        }
        img_w as f32 / phys as f32
    }

    /// 客户区左上角的屏幕绝对坐标（物理像素）。
    pub fn client_origin(&self) -> (i32, i32) {
        (self.window.x().unwrap_or(0), self.window.y().unwrap_or(0))
    }

    /// 目标窗口句柄（自动点击前用它把窗口切到前台）。
    pub fn hwnd(&self) -> u32 {
        self.window.id().unwrap_or(0)
    }

    /// 由棋子中心拟合出的真实格点仿射（客户区图像像素）。
    /// 语义：x = k[0] + k[1]*col + k[2]*row，y = k[3] + k[4]*col + k[5]*row。
    /// None 表示未拟合成功，退回检测框外推。
    pub fn lattice(&self) -> Option<[f32; 6]> {
        self.lattice
    }

    pub fn set_lattice(&mut self, lattice: Option<[f32; 6]>) {
        self.lattice = lattice;
    }

    pub fn set_sub_bound(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.x = x;
        self.y = y;
        self.w = w;
        self.h = h;
    }
}
