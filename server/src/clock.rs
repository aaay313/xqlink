//! 头像读秒检测：正在行棋的一方，头像上会显示倒计时读秒圈（暗红圆环 + 白色数字），
//! 等待方没有。据此判断轮到谁行棋 —— 比盘面推断（infer_mover）更可靠。
//!
//! 特征经 JJ 象棋对弈窗口实测校准（441×840 截图）：
//! - 行棋侧条带暗红像素 ≈2700，等待侧 ≈130 → 阈值取 800；
//! - 窗口两侧有常驻红色装饰（暗红 ≈3600），通过只统计「窗口中列带」排除；
//! - 条带边界取「棋盘线」之外（棋盘线 = 抓取区边界向内缩半格），
//!   棋盘线外没有棋子，红方棋子的红色不会干扰统计。

use xcap::image::ImageBuffer;
use xcap::image::Rgba;

/// 中列带半宽（窗口宽的比例）：读秒圈在窗口水平中央。
const CENTER_BAND: f32 = 0.09;

/// 条带内暗红像素达到该数量即判定该侧正在行棋。
const RED_THRESHOLD: u32 = 800;

/// 暗红像素判定（读秒圈的暗红圆环，含数字描边）。
fn is_warm_red(r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    r >= 70 && r - g >= 25 && r - b >= 25
}

/// 抓取区高度换算半格：抓取区 = 棋盘 ± 半格 → 半格 = 抓取区高 / 22。
fn half_cell(bh: u32) -> u32 { bh / 22 }

/// 统计矩形区域内的暗红像素数。
fn count_warm_red(img: &ImageBuffer<Rgba<u8>, Vec<u8>>, x0: u32, x1: u32, y0: u32, y1: u32) -> u32 {
    let (w, h) = (img.width(), img.height());
    let (x0, x1) = (x0.min(w), x1.min(w));
    let (y0, y1) = (y0.min(h), y1.min(h));
    if x0 >= x1 || y0 >= y1 {
        return 0;
    }
    let mut n = 0u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let px = img.get_pixel(x, y);
            if is_warm_red(px[0], px[1], px[2]) {
                n += 1;
            }
        }
    }
    n
}

/// 检测轮到谁行棋。
///
/// `board` 是棋盘抓取区（含半格缓冲）在整窗口内的边界。采样条带：
/// 棋盘**线**之外的上/下区域 × 窗口中列带。
///
/// 返回 Some(true)=下方（我方）正在行棋、Some(false)=上方（对方）正在行棋、
/// None=两侧都无法判定（调用方退回盘面推断）。
pub fn detect_turn(
    img: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    board: (u32, u32, u32, u32),
) -> Option<bool> {
    let (_bx, by, bw, bh) = board;
    if bw == 0 || bh == 0 {
        return None;
    }
    let w = img.width();
    let half = (w as f32 * CENTER_BAND) as u32;
    let cx = w / 2;
    let (x0, x1) = (cx.saturating_sub(half), (cx + half).min(w));

    // 棋盘线顶/底（抓取区边界向内缩半格）；线外即为头像条带
    let pad = half_cell(bh);
    let top = count_warm_red(img, x0, x1, 0, by + pad);
    let bottom = count_warm_red(img, x0, x1, by + bh - pad, img.height());

    match (top >= RED_THRESHOLD, bottom >= RED_THRESHOLD) {
        (true, false) => Some(false),
        (false, true) => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试图：在指定圆心画暗红圆环（模拟读秒圈）。
    ///
    /// 尺寸按真实 JJ 象棋窗口比例构造：窗口 440 宽，读秒圈半径 40（直径 ≈18% 窗口宽），
    /// 环宽 9 —— 与实测截图（441×840，红圈外径 ≈42）一致。
    fn draw_ring(img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, cx: i32, cy: i32, radius: i32) {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let d2 = dx * dx + dy * dy;
                let (x, y) = (cx + dx, cy + dy);
                if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
                    continue;
                }
                // 环宽约 9px
                if d2 <= radius * radius && d2 >= (radius - 9) * (radius - 9) {
                    img.put_pixel(x as u32, y as u32, Rgba([160, 70, 70, 255]));
                }
            }
        }
    }

    #[test]
    fn test_top_ring_means_opponent_turn() {
        // 棋盘抓取区 y[200, 800]（bh=600，半格=27），红圈画在上方头像处 (220, 120)
        let mut img = ImageBuffer::from_pixel(440, 840, Rgba([40, 60, 40, 255]));
        draw_ring(&mut img, 220, 120, 40);
        assert_eq!(detect_turn(&img, (0, 200, 440, 600)), Some(false));
    }

    #[test]
    fn test_bottom_ring_means_our_turn() {
        let mut img = ImageBuffer::from_pixel(440, 840, Rgba([40, 60, 40, 255]));
        draw_ring(&mut img, 220, 820, 40);
        assert_eq!(detect_turn(&img, (0, 200, 440, 600)), Some(true));
    }

    #[test]
    fn test_no_ring_returns_none() {
        // 只有少量暗红噪声，达不到阈值
        let mut img = ImageBuffer::from_pixel(440, 840, Rgba([90, 60, 50, 255]));
        for y in 0..40 {
            for x in 0..10 {
                img.put_pixel(x, y, Rgba([160, 70, 70, 255]));
            }
        }
        assert_eq!(detect_turn(&img, (0, 200, 440, 600)), None);
    }

    #[test]
    fn test_side_decorations_ignored() {
        // 红色装饰在中列带之外（贴左右边），不应触发判定
        let mut img = ImageBuffer::from_pixel(440, 840, Rgba([40, 60, 40, 255]));
        draw_ring(&mut img, 25, 120, 40);
        draw_ring(&mut img, 415, 120, 40);
        assert_eq!(detect_turn(&img, (0, 200, 440, 600)), None);
    }
}
