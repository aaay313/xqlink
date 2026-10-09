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
}

impl ListenWindow {
    #[tracing::instrument]
    pub fn new(target: &Window, w: usize, h: usize) -> Option<Self> {
        let windows = xcap::Window::all().ok()?;
        for window in windows {
            if window.id().ok() == Some(target.id) {
                return Some(Self { window, x: 0, y: 0, w: 0, h: 0 });
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

    pub fn set_sub_bound(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.x = x;
        self.y = y;
        self.w = w;
        self.h = h;
    }
}
