//! 棋盘识别 —— 不使用任何机器学习模型。
//!
//! 替换原先的 `yolo.rs`（ONNX 推理）。原方案需要 28.6 MB 模型 + onnxruntime，
//! 且对深色皮肤、低对比画面敏感；本模块用确定性算法，零模型依赖。
//!
//! # 为什么不用「找棋盘线」
//!
//! JJ 象棋的盘面是石纹，缝线只有 1~2 px、对比极低，而棋子金边的梯度强得多。
//! 行列投影会被棋子主导 —— 实测 19 条线只有 4 条达到强度要求，等间距拟合必然失败。
//!
//! # 方案：棋子 → 格点 → 分类
//!
//! 1. `detect_piece_centers` 金色连通域找棋子（棋盘边的列号数字也满足金色条件，
//!    靠后面的格点一致性剔除）。
//! 2. `detect_board_bbox` 低饱和 + 中亮度的最大连通块 = 棋盘石面。
//! 3. `fit_grid` 由棋子坐标差求格距，再由「棋盘内居中」定相位。
//!    相位**不能**用「哪个点在最左」推：整行/整列偏移一格时残差完全相同。
//! 4. `read_board` 每个交叉点先用「金色圆环占比」判占位，再用整块图块的
//!    归一化互相关（NCC）判兵种。
//!
//! # 为什么分类不用字形匹配
//!
//! 棋子是 3D 风格，面盘内侧有一圈深色线圈、外圈金边带高光。先把字形分割出来
//! 再匹配，分割误差比字体差异还大（实测字体模板 NCC 峰值仅 0.1~0.35 且首选全错）。
//! 整块图块 NCC 的模板取自游戏自身，同一皮肤下峰值可达 0.95。

use xcap::image::RgbImage;

/// 分类取样窗相对格距的比例。
///
/// 取 0.70 是为了避开邻子：棋子直径约等于一个格距，整格窗压在空交叉点上时
/// 会切到邻子的金边，使空位的 NCC 也能到 0.7 以上。
/// **模板与待测必须用同一比例**，否则相关度会整体崩塌。
pub const WIN_RATIO: f32 = 0.70;

/// 归一化后的图块边长。
const VEC: usize = 24;

/// 占位判定：圆环内金色像素占比阈值。
const RING_GOLD: f32 = 0.16;

/// 占位判定：圆环内「非石面」像素占比阈值（用于高亮棋子，金边被白环盖住时）。
const RING_NOT_STONE: f32 = 0.42;

/// 分类接受阈值。实测有子时 NCC 最低 0.92、中位 0.95；空位最高 0.78。
const NCC_ACCEPT: f32 = 0.86;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub x0: f32,
    pub y0: f32,
    pub cw: f32,
    pub ch: f32,
}

impl Grid {
    /// 是否在图像范围内（允许半格余量）。
    pub fn within(&self, w: u32, h: u32) -> bool {
        self.cw > 6.0
            && self.ch > 6.0
            && self.x0 > -self.cw * 0.6
            && self.y0 > -self.ch * 0.6
            && self.x0 + 8.0 * self.cw < w as f32 + self.cw * 0.6
            && self.y0 + 9.0 * self.ch < h as f32 + self.ch * 0.6
    }

    /// 格距是否接近正方形。象棋盘是正方形格子，偏离说明拟合错了。
    pub fn square_enough(&self) -> bool {
        let r = self.cw / self.ch;
        (0.82..=1.22).contains(&r)
    }
}

// ---------------------------------------------------------------------------
// 模板库
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct Bank {
    items: Vec<(char, Vec<f32>)>,
}

impl Bank {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// 从图像中按中心点取一块，归一化成单位向量后入库。
    pub fn add_at(&mut self, img: &RgbImage, cx: f32, cy: f32, size: u32, ch: char) {
        if let Some(v) = patch_vec(img, cx, cy, size) {
            self.items.push((ch, v));
        }
    }

    /// 用标准开局局面自动播种 —— 无需人工标注。
    ///
    /// 调用方需保证当前帧确实是开局：否则标签与图像不符，模板全废。
    /// 返回实际入库数量。
    pub fn seed_start(&mut self, img: &RgbImage, g: &Grid, board: &[[char; 9]; 10]) -> usize {
        let size = (g.cw.min(g.ch) * WIN_RATIO).round().max(8.0) as u32;
        let mut n = 0;
        for r in 0..10 {
            for c in 0..9 {
                let p = board[r][c];
                if p == ' ' {
                    continue;
                }
                let before = self.items.len();
                self.add_at(img, g.x0 + c as f32 * g.cw, g.y0 + r as f32 * g.ch, size, p);
                if self.items.len() > before {
                    n += 1;
                }
            }
        }
        n
    }

    /// 返回 (最佳字符, 最佳分数, 次佳分数)。
    pub fn best(&self, v: &[f32]) -> (Option<char>, f32, f32) {
        let mut best = (None, -2.0f32, -2.0f32);
        for (ch, tpl) in &self.items {
            if tpl.len() != v.len() {
                continue;
            }
            let mut s = 0.0f32;
            for i in 0..v.len() {
                s += v[i] * tpl[i];
            }
            if s > best.1 {
                best = (Some(*ch), s, best.1);
            } else if s > best.2 {
                best.2 = s;
            }
        }
        best
    }

    pub fn to_json(&self) -> String {
        let mut s = String::from("{\"vec\":24,\"items\":[");
        for (i, (ch, v)) in self.items.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("[\"{}\",[", ch));
            for (j, x) in v.iter().enumerate() {
                if j > 0 {
                    s.push(',');
                }
                s.push_str(&format!("{:.5}", x));
            }
            s.push_str("]]");
        }
        s.push_str("]}");
        s
    }

    /// 极简解析：只认自己写出的格式，避免为模板库引入 serde 结构体。
    pub fn from_json(text: &str) -> Option<Self> {
        let start = text.find("\"items\":[")? + 9;
        let body = &text[start..];
        let mut bank = Bank::new();
        let mut i = 0usize;
        let bytes = body.as_bytes();
        while i < bytes.len() {
            // 找 '["c",['
            if bytes[i] != b'[' {
                break;
            }
            let q1 = body[i..].find('"')? + i;
            let q2 = body[q1 + 1..].find('"')? + q1 + 1;
            let ch = body[q1 + 1..q2].chars().next()?;
            let lb = body[q2..].find('[')? + q2;
            let rb = body[lb..].find(']')? + lb;
            let nums: Vec<f32> = body[lb + 1..rb]
                .split(',')
                .filter(|t| !t.is_empty())
                .filter_map(|t| t.trim().parse::<f32>().ok())
                .collect();
            if nums.len() == VEC * VEC * 3 {
                bank.items.push((ch, nums));
            }
            // 跳过数字数组的 ']'，落在条目自己的 ']' 上
            let close = body[rb + 1..].find(']')? + rb + 1;
            i = close + 1;
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
            }
        }
        if bank.items.is_empty() {
            None
        } else {
            Some(bank)
        }
    }
}

// ---------------------------------------------------------------------------
// 图块 → 单位向量
// ---------------------------------------------------------------------------

/// 按最近邻取 size×size 的图块，逐通道去均值后归一化。
///
/// 保留 RGB 三通道：红黑两方的棋子天然可分，灰度化会丢掉这个信息。
fn patch_vec(img: &RgbImage, cx: f32, cy: f32, size: u32) -> Option<Vec<f32>> {
    if size < 6 {
        return None;
    }
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let half = size as f32 / 2.0;
    let x0 = cx - half;
    let y0 = cy - half;
    let n = size as usize;
    let mut buf = vec![0f32; n * n * 3];
    let mut sum = [0f32; 3];
    for j in 0..n {
        for i in 0..n {
            // 采样点落在格内，超界则夹到边缘（棋盘外的格子不应参与，但要有值）
            let sx = (x0 + i as f32 * size as f32 / n as f32).round() as i32;
            let sy = (y0 + j as f32 * size as f32 / n as f32).round() as i32;
            let sx = sx.clamp(0, iw - 1);
            let sy = sy.clamp(0, ih - 1);
            let p = img.get_pixel(sx as u32, sy as u32);
            let idx = (j * n + i) * 3;
            for k in 0..3 {
                let v = p[k] as f32;
                buf[idx + k] = v;
                sum[k] += v;
            }
        }
    }
    let total = (n * n) as f32;
    for k in 0..3 {
        sum[k] /= total;
    }
    let mut norm = 0f32;
    for j in 0..n {
        for i in 0..n {
            let idx = (j * n + i) * 3;
            for k in 0..3 {
                let v = buf[idx + k] - sum[k];
                buf[idx + k] = v;
                norm += v * v;
            }
        }
    }
    let norm = norm.sqrt();
    if norm < 1e-3 {
        return None;
    }
    for v in buf.iter_mut() {
        *v /= norm;
    }
    Some(buf)
}

// ---------------------------------------------------------------------------
// 连通域
// ---------------------------------------------------------------------------

/// 4 邻域连通域标记，返回 (标签图, 各标签的 bbox, 各标签面积)。
fn label_components(mask: &[bool], w: usize, h: usize) -> (Vec<u32>, Vec<(usize, usize, usize, usize, u32)>) {
    let mut labels = vec![0u32; w * h];
    let mut boxes: Vec<(usize, usize, usize, usize, u32)> = Vec::new();
    let mut stack: Vec<usize> = Vec::with_capacity(1024);
    let mut cur = 0u32;
    for start in 0..w * h {
        if !mask[start] || labels[start] != 0 {
            continue;
        }
        cur += 1;
        labels[start] = cur;
        stack.clear();
        stack.push(start);
        let mut x1 = start % w;
        let mut x2 = x1;
        let mut y1 = start / w;
        let mut y2 = y1;
        let mut area = 0u32;
        while let Some(p) = stack.pop() {
            area += 1;
            let x = p % w;
            let y = p / w;
            if x < x1 {
                x1 = x;
            }
            if x > x2 {
                x2 = x;
            }
            if y < y1 {
                y1 = y;
            }
            if y > y2 {
                y2 = y;
            }
            if x > 0 {
                let q = p - 1;
                if mask[q] && labels[q] == 0 {
                    labels[q] = cur;
                    stack.push(q);
                }
            }
            if x + 1 < w {
                let q = p + 1;
                if mask[q] && labels[q] == 0 {
                    labels[q] = cur;
                    stack.push(q);
                }
            }
            if y > 0 {
                let q = p - w;
                if mask[q] && labels[q] == 0 {
                    labels[q] = cur;
                    stack.push(q);
                }
            }
            if y + 1 < h {
                let q = p + w;
                if mask[q] && labels[q] == 0 {
                    labels[q] = cur;
                    stack.push(q);
                }
            }
        }
        boxes.push((x1, y1, x2 + 1, y2 + 1, area));
    }
    (labels, boxes)
}

/// 形态学闭运算（先膨胀后腐蚀），用于把断续的金边连成整块。
fn morph_close(mask: &mut [bool], w: usize, h: usize, r: usize) {
    if r == 0 {
        return;
    }
    let dil = dilate(mask, w, h, r);
    let clo = erode(&dil, w, h, r);
    mask.copy_from_slice(&clo);
}

fn dilate(src: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    // 可分离：先横向再纵向
    let mut tmp = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            if !src[y * w + x] {
                continue;
            }
            let lo = x.saturating_sub(r);
            let hi = (x + r).min(w - 1);
            for xx in lo..=hi {
                tmp[y * w + xx] = true;
            }
        }
    }
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            if !tmp[y * w + x] {
                continue;
            }
            let lo = y.saturating_sub(r);
            let hi = (y + r).min(h - 1);
            for yy in lo..=hi {
                out[yy * w + x] = true;
            }
        }
    }
    out
}

fn erode(src: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    let mut tmp = vec![true; w * h];
    for y in 0..h {
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x + r).min(w - 1);
            let mut all = true;
            for xx in lo..=hi {
                if !src[y * w + xx] {
                    all = false;
                    break;
                }
            }
            tmp[y * w + x] = all;
        }
    }
    let mut out = vec![true; w * h];
    for y in 0..h {
        for x in 0..w {
            let lo = y.saturating_sub(r);
            let hi = (y + r).min(h - 1);
            let mut all = true;
            for yy in lo..=hi {
                if !tmp[yy * w + x] {
                    all = false;
                    break;
                }
            }
            out[y * w + x] = all;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 棋盘区域
// ---------------------------------------------------------------------------

/// 定位棋盘石面区域，返回 (x0, y0, x1, y1)。
///
/// 棋盘是低饱和、中等亮度的石面；棋子金边高饱和，草地高饱和偏绿，
/// 因此「低饱和 + 中亮度」的最大连通块即棋盘。
pub fn detect_board_bbox(img: &RgbImage) -> Option<(f32, f32, f32, f32)> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < 40 || h < 40 {
        return None;
    }
    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x as u32, y as u32);
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            let sat = if mx > 1.0 { (mx - mn) / mx } else { 0.0 };
            let lum = (r + g + b) / 3.0;
            mask[y * w + x] = sat < 0.14 && lum > 110.0 && lum < 205.0;
        }
    }
    morph_close(&mut mask, w, h, 8);
    let (_, boxes) = label_components(&mask, w, h);
    let best = boxes.iter().max_by_key(|b| b.4)?;
    let (x1, y1, x2, y2, area) = *best;
    if (area as f32) < (w * h) as f32 * 0.05 {
        return None;
    }
    Some((x1 as f32, y1 as f32, x2 as f32, y2 as f32))
}

// ---------------------------------------------------------------------------
// 棋子中心
// ---------------------------------------------------------------------------

/// 金色连通域找棋子中心。
///
/// 只做尺寸/长宽比粗筛 —— 棋盘边上的列号数字也满足金色条件，且尺寸相近，
/// 单靠形状分不干净；真正的误检交给后续的格点一致性剔除。
pub fn detect_piece_centers(img: &RgbImage) -> Vec<(f32, f32)> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x as u32, y as u32);
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            mask[y * w + x] = r > 140.0 && g > 100.0 && b < 145.0 && (r - b) > 45.0 && (r - g) > 5.0;
        }
    }
    morph_close(&mut mask, w, h, 2);
    let (labels, boxes) = label_components(&mask, w, h);

    // 尺寸参考：格距的合理范围由图像宽度估计（9 列）
    let min_s = ((w as f32 / 40.0).max(12.0)) as usize;
    let max_s = ((w as f32 / 8.0).max(40.0)) as usize;

    let mut out = Vec::new();
    for (idx, (x1, y1, x2, y2, _)) in boxes.iter().enumerate() {
        let bw = x2 - x1;
        let bh = y2 - y1;
        if bw < min_s || bh < min_s || bw > max_s || bh > max_s {
            continue;
        }
        let diff = (bw as i32 - bh as i32).abs() as f32;
        if diff > 10.0f32.max(0.22 * bw.max(bh) as f32) {
            continue;
        }
        let label = (idx + 1) as u32;
        let mut sx = 0f64;
        let mut sy = 0f64;
        let mut n = 0f64;
        for y in *y1..*y2 {
            for x in *x1..*x2 {
                if labels[y * w + x] == label {
                    sx += x as f64;
                    sy += y as f64;
                    n += 1.0;
                }
            }
        }
        if n < 1.0 {
            continue;
        }
        out.push((sx as f32 / n as f32, sy as f32 / n as f32));
    }
    out
}

// ---------------------------------------------------------------------------
// 格点拟合
// ---------------------------------------------------------------------------

/// 由一堆坐标求格距：用成对差值的整数倍关系迭代收敛到最小周期。
fn fundamental(vals: &[f32]) -> Option<f32> {
    let mut diffs = Vec::new();
    for i in 0..vals.len() {
        for j in (i + 1)..vals.len() {
            let d = (vals[i] - vals[j]).abs();
            if d > 8.0 {
                diffs.push(d);
            }
        }
    }
    if diffs.is_empty() {
        return None;
    }
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let take = (diffs.len() / 20).max(3).min(diffs.len());
    let mut step = median(&diffs[..take]);
    for _ in 0..40 {
        let mut ratios = Vec::new();
        for d in &diffs {
            let k = (d / step).round().max(1.0);
            let s = d / k;
            if s > step * 0.55 && s < step * 1.6 {
                ratios.push(s);
            }
        }
        if ratios.len() < 5 {
            break;
        }
        let new = median(&ratios);
        let done = (new - step).abs() < 1e-3;
        step = new;
        if done {
            break;
        }
    }
    if step.is_finite() && step > 6.0 {
        Some(step)
    } else {
        None
    }
}

fn median(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// 一维最小二乘：vals[i] ≈ origin + idx[i]*step，返回 (origin, step)。
fn fit_axis(vals: &[f32], idx: &[f32]) -> (f32, f32) {
    let n = vals.len() as f32;
    let mi = idx.iter().sum::<f32>() / n;
    let mv = vals.iter().sum::<f32>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for k in 0..vals.len() {
        let di = idx[k] - mi;
        num += di * (vals[k] - mv);
        den += di * di;
    }
    let step = if den.abs() > 1e-6 { num / den } else { 0.0 };
    (mv - step * mi, step)
}

/// 由棋子中心 + 棋盘边界拟合 9×10 格点。
///
/// 相位靠「棋盘内居中」确定：石面比格点区每边宽约 0.4 格，两侧留白相等。
/// 这条约束消除了「整行/整列错位一格」的歧义 —— 两种解的残差完全相同。
pub fn fit_grid(centers: &[(f32, f32)], bbox: (f32, f32, f32, f32)) -> Option<Grid> {
    if centers.len() < 4 {
        return None;
    }
    let xs: Vec<f32> = centers.iter().map(|p| p.0).collect();
    let ys: Vec<f32> = centers.iter().map(|p| p.1).collect();
    let mut cw = fundamental(&xs)?;
    let mut ch = fundamental(&ys)?;

    let (bx0, by0, bx1, by1) = bbox;
    let cx0 = bx0 + ((bx1 - bx0) - 8.0 * cw) / 2.0;
    let cy0 = by0 + ((by1 - by0) - 9.0 * ch) / 2.0;

    let mut best: Option<(i32, i32, f32, f32, Vec<bool>)> = None;
    for dx in [0i32, -1, 1] {
        let ox = cx0 + dx as f32 * cw;
        if ox < bx0 - cw * 0.2 || ox + 8.0 * cw > bx1 + cw * 0.2 {
            continue;
        }
        for dy in [0i32, -1, 1] {
            let oy = cy0 + dy as f32 * ch;
            if oy < by0 - ch * 0.2 || oy + 9.0 * ch > by1 + ch * 0.2 {
                continue;
            }
            let (ix, hitx) = axis_inliers(&xs, ox, cw, 9);
            let (iy, hity) = axis_inliers(&ys, oy, ch, 10);
            let score = ix + iy;
            let mut both = Vec::with_capacity(hitx.len());
            for k in 0..hitx.len() {
                both.push(hitx[k] && hity[k]);
            }
            let improve = match &best {
                None => true,
                Some(b) => (score, -(dx.abs() + dy.abs())) > (b.0, b.1),
            };
            if improve {
                best = Some((score, -(dx.abs() + dy.abs()), ox, oy, both));
            }
        }
    }
    let (score, _, mut x0, mut y0, mut inl) = best?;
    if score < 8 {
        return None;
    }

    // 用内点精修
    for _ in 0..2 {
        let mut vx = Vec::new();
        let mut ix = Vec::new();
        let mut vy = Vec::new();
        let mut iy = Vec::new();
        for (k, (x, y)) in centers.iter().enumerate() {
            if !inl[k] {
                continue;
            }
            let c = ((x - x0) / cw).round();
            let r = ((y - y0) / ch).round();
            vx.push(*x);
            ix.push(c);
            vy.push(*y);
            iy.push(r);
        }
        if vx.len() < 4 {
            break;
        }
        let (ox, sx) = fit_axis(&vx, &ix);
        let (oy, sy) = fit_axis(&vy, &iy);
        if sx.abs() > 6.0 && sy.abs() > 6.0 {
            x0 = ox;
            cw = sx;
            y0 = oy;
            ch = sy;
        }
        let (_, hx) = axis_inliers(&xs, x0, cw, 9);
        let (_, hy) = axis_inliers(&ys, y0, ch, 10);
        for k in 0..inl.len() {
            inl[k] = hx[k] && hy[k];
        }
    }

    let g = Grid { x0, y0, cw, ch };
    Some(g)
}

/// 返回 (内点数, 每个点是否落在格点上)。
fn axis_inliers(vals: &[f32], origin: f32, step: f32, n: i32) -> (i32, Vec<bool>) {
    let mut cnt = 0;
    let mut hit = Vec::with_capacity(vals.len());
    for v in vals {
        let idx = ((v - origin) / step).round();
        let err = (v - (origin + idx * step)).abs();
        let ok = err < step * 0.30 && idx >= 0.0 && idx <= (n - 1) as f32;
        if ok {
            cnt += 1;
        }
        hit.push(ok);
    }
    (cnt, hit)
}

// ---------------------------------------------------------------------------
// 占位判定
// ---------------------------------------------------------------------------

/// 圆环内「非石面」与「金色」占比，返回 (gold_ratio, not_stone_ratio)。
///
/// 半径取 0.72~1.04 倍半格：棋子直径约等于一个格距，该环正好落在金边上。
/// 高亮/选中棋子外围有白环，金边被盖住（实测金色占比只有 0.026），
/// 所以额外用一条与颜色无关的「非石面」判据兜底。
fn ring_stats(img: &RgbImage, cx: f32, cy: f32, size: u32) -> (f32, f32) {
    let n = size.max(8) as i32;
    let half = n as f32 / 2.0;
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let r_lo = half * 0.72;
    let r_hi = half * 1.04;
    let (mut gold, mut not_stone, mut total) = (0u32, 0u32, 0u32);
    for j in 0..n {
        for i in 0..n {
            let dx = i as f32 - half;
            let dy = j as f32 - half;
            let d = (dx * dx + dy * dy).sqrt();
            if d < r_lo || d > r_hi {
                continue;
            }
            let sx = (cx - half + i as f32).round() as i32;
            let sy = (cy - half + j as f32).round() as i32;
            if sx < 0 || sy < 0 || sx >= iw || sy >= ih {
                continue;
            }
            let p = img.get_pixel(sx as u32, sy as u32);
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            if r > 130.0 && g > 95.0 && (r - b) > 40.0 && (r - g) > 3.0 {
                gold += 1;
            }
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            let sat = if mx > 1.0 { (mx - mn) / mx } else { 0.0 };
            let lum = (r + g + b) / 3.0;
            let stone = sat < 0.16 && lum > 105.0 && lum < 210.0;
            if !stone {
                not_stone += 1;
            }
            total += 1;
        }
    }
    if total == 0 {
        return (0.0, 0.0);
    }
    let t = total as f32;
    (gold as f32 / t, not_stone as f32 / t)
}

// ---------------------------------------------------------------------------
// 读盘
// ---------------------------------------------------------------------------

/// 识别一帧，返回 (格点, 屏幕方向的盘面, 每格置信度)。
///
/// 盘面方向与屏幕一致（row 0 在上）；调用方仍需 `chess::board_fix` 归一化。
pub fn read_board(img: &RgbImage, bank: &Bank) -> Option<(Grid, [[char; 9]; 10], [[f32; 9]; 10])> {
    if bank.is_empty() {
        return None;
    }
    let grid = detect_grid(img)?;
    read_with_grid(img, &grid, bank)
}

/// 只做定位（棋盘区域 + 棋子中心 + 格点），不看兵种。
///
/// 播种模板时需要「先有格点、再有模板」，所以定位与分类拆开。
pub fn detect_grid(img: &RgbImage) -> Option<Grid> {
    let bbox = detect_board_bbox(img)?;
    let centers = detect_piece_centers(img);
    let grid = fit_grid(&centers, bbox)?;
    if !grid.within(img.width(), img.height()) || !grid.square_enough() {
        return None;
    }
    Some(grid)
}

/// 已知格点时读盘。
pub fn read_with_grid(
    img: &RgbImage, grid: &Grid, bank: &Bank,
) -> Option<(Grid, [[char; 9]; 10], [[f32; 9]; 10])> {
    if bank.is_empty() {
        return None;
    }
    let cell = grid.cw.min(grid.ch);
    let win = (cell * WIN_RATIO).round().max(10.0) as u32;
    let ring_size = cell.round().max(12.0) as u32;

    let mut board = [[' '; 9]; 10];
    let mut conf = [[0f32; 9]; 10];
    for r in 0..10 {
        for c in 0..9 {
            let px = grid.x0 + c as f32 * grid.cw;
            let py = grid.y0 + r as f32 * grid.ch;
            let (gold, not_stone) = ring_stats(img, px, py, ring_size);
            if gold < RING_GOLD && not_stone < RING_NOT_STONE {
                continue;
            }
            let Some(v) = patch_vec(img, px, py, win) else {
                continue;
            };
            let (ch, score, _) = bank.best(&v);
            if let Some(ch) = ch {
                conf[r][c] = score;
                if score >= NCC_ACCEPT {
                    board[r][c] = ch;
                }
            }
        }
    }

    if camp_of(&board).is_none() {
        return None;
    }
    Some((*grid, board, conf))
}

/// 标准开局局面（用于自动播种模板）。
pub const START_BOARD: [[char; 9]; 10] = [
    ['r', 'n', 'b', 'a', 'k', 'a', 'b', 'n', 'r'],
    [' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' '],
    [' ', 'c', ' ', ' ', ' ', ' ', ' ', 'c', ' '],
    ['p', ' ', 'p', ' ', 'p', ' ', 'p', ' ', 'p'],
    [' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' '],
    [' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' '],
    ['P', ' ', 'P', ' ', 'P', ' ', 'P', ' ', 'P'],
    [' ', 'C', ' ', ' ', ' ', ' ', ' ', 'C', ' '],
    [' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' '],
    ['R', 'N', 'B', 'A', 'K', 'A', 'B', 'N', 'R'],
];

/// 判断当前帧是否是标准开局，用于决定能否自动播种模板。
///
/// 只依赖占位（金环/非石面），不需要模板库 —— 这是打破
/// 「要模板才能读盘、要读盘才能建模板」循环的唯一办法。
pub fn looks_like_start(img: &RgbImage, grid: &Grid) -> bool {
    let cell = grid.cw.min(grid.ch);
    let ring_size = cell.round().max(12.0) as u32;
    let mut occupied = [[false; 9]; 10];
    for r in 0..10 {
        for c in 0..9 {
            let px = grid.x0 + c as f32 * grid.cw;
            let py = grid.y0 + r as f32 * grid.ch;
            let (gold, not_stone) = ring_stats(img, px, py, ring_size);
            occupied[r][c] = gold >= RING_GOLD || not_stone >= RING_NOT_STONE;
        }
    }
    let mut diff = 0;
    for r in 0..10 {
        for c in 0..9 {
            let want = START_BOARD[r][c] != ' ';
            if occupied[r][c] != want {
                diff += 1;
            }
        }
    }
    diff <= 2
}

/// 阵营判定：己方（屏幕下方）是红还是黑。
pub fn camp_of(board: &[[char; 9]; 10]) -> Option<crate::chess::Camp> {
    let red_bottom = (0..9).any(|c| board[9][c] == 'K');
    let black_bottom = (0..9).any(|c| board[9][c] == 'k');
    match (red_bottom, black_bottom) {
        (true, false) => Some(crate::chess::Camp::Red),
        (false, true) => Some(crate::chess::Camp::Black),
        _ => None,
    }
}

/// RGBA → RGB（去掉 alpha，vision 全流程只用三通道）。
pub fn to_rgb(img: &RgbaImage) -> RgbImage {
    let (w, h) = (img.width(), img.height());
    RgbImage::from_fn(w, h, |x, y| {
        let p = img.get_pixel(x, y);
        xcap::image::Rgb([p[0], p[1], p[2]])
    })
}

// ---------------------------------------------------------------------------
// 全局模板库
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use xcap::image::RgbaImage;

static BANK_PATH: OnceLock<PathBuf> = OnceLock::new();
static BANK: OnceLock<Mutex<Bank>> = OnceLock::new();
/// 裁剪区内的格点。开监听时算一次即可 —— 窗口与裁剪区不变，格点也不会变。
/// 缓存它既省掉每帧的整图扫描（石面形态学闭运算很贵），
/// 也保证「读盘用的格点」与「点击用的坐标换算」用的是同一套基准。
static GRID: Mutex<Option<Grid>> = Mutex::new(None);

pub fn set_grid(grid: Option<Grid>) {
    if let Ok(mut g) = GRID.lock() {
        *g = grid;
    }
}

pub fn cached_grid() -> Option<Grid> {
    GRID.lock().ok().and_then(|g| *g)
}

/// 初始化模板库路径，并尝试从磁盘加载上次采集的模板。
///
/// 模板与皮肤绑定：换皮肤后旧模板会失配，`read_with_grid` 的 NCC 接受阈值
/// 会拒绝它们（读不出盘），此时删掉该文件、在开局重新采集即可。
pub fn init(path: PathBuf) {
    let _ = BANK_PATH.set(path.clone());
    let mut bank = Bank::new();
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Some(loaded) = Bank::from_json(&text)
    {
        tracing::info!("已加载棋子模板 {} 条：{}", loaded.len(), path.display());
        bank = loaded;
    }
    let _ = BANK.set(Mutex::new(bank));
}

pub fn bank() -> &'static Mutex<Bank> {
    BANK.get_or_init(|| Mutex::new(Bank::new()))
}

/// 把当前模板库写回磁盘（失败不影响运行）。
pub fn persist(bank: &Bank) {
    if let Some(path) = BANK_PATH.get()
        && let Some(dir) = path.parent()
    {
        let _ = std::fs::create_dir_all(dir);
        if let Err(e) = std::fs::write(path, bank.to_json()) {
            tracing::warn!("保存棋子模板失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn fundamental_recovers_uniform_spacing() {
        // 9 列，格距 59.24，缺两列 —— 仍应还原出 59.24
        let step = 59.24f32;
        let vals: Vec<f32> = [0usize, 1, 2, 4, 5, 7, 8].iter().map(|i| *i as f32 * step).collect();
        let got = fundamental(&vals).unwrap();
        assert!((got - step).abs() < 0.5, "got {got}");
    }

    #[test]
    fn fit_axis_recovers_line() {
        let idx = [0.0, 1.0, 2.0, 4.0];
        let vals = [10.0, 40.0, 70.0, 130.0];
        let (o, s) = fit_axis(&vals, &idx);
        assert!((o - 10.0).abs() < 0.01);
        assert!((s - 30.0).abs() < 0.01);
    }

    #[test]
    fn grid_geometry_checks() {
        let g = Grid { x0: 100.0, y0: 50.0, cw: 40.0, ch: 40.0 };
        assert!(g.square_enough());
        assert!(g.within(500, 600));
        let skew = Grid { x0: 100.0, y0: 50.0, cw: 40.0, ch: 70.0 };
        assert!(!skew.square_enough());
    }

    #[test]
    fn bank_json_roundtrip() {
        let mut b = Bank::new();
        b.items.push(('R', vec![0.1f32; VEC * VEC * 3]));
        b.items.push(('n', vec![-0.2f32; VEC * VEC * 3]));
        let text = b.to_json();
        let back = Bank::from_json(&text).expect("应能解析");
        assert_eq!(back.len(), 2);
        assert_eq!(back.items[0].0, 'R');
        assert_eq!(back.items[1].0, 'n');
    }
}
