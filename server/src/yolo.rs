use std::iter::Iterator;
use std::sync::OnceLock;

use ndarray::s;
use ndarray::Array;
use ort::inputs;
use ort::session::builder::GraphOptimizationLevel;
use xcap::image::imageops::FilterType;
use xcap::image::DynamicImage;
use xcap::image::ImageBuffer;
use xcap::image::Rgba;

static SESSION: OnceLock<ort::session::Session> = OnceLock::new();

pub const IMAGE_WIDTH: usize = 640;
pub const IMAGE_HEIGHT: usize = 640;
const CONFIDENCE_THRESHOLD: f32 = 0.7;
const IOU_THRESHOLD: f32 = 0.5;
const LABELS: [char; 15] = ['n', 'b', 'a', 'k', 'r', 'c', 'p', 'R', 'N', 'A', 'K', 'B', 'C', 'P', '0'];
const LIMIT: [usize; 15] = [2, 2, 2, 1, 2, 2, 5, 2, 2, 2, 1, 2, 2, 5, 1];

#[cfg(not(feature = "rotate"))]
const MODEL_BYTES: &[u8] = include_bytes!("../../libs/large.onnx");

#[cfg(feature = "rotate")]
const MODEL_BYTES: &[u8] = include_bytes!("../../libs/rotate.onnx");

pub fn session() -> &'static ort::session::Session {
    SESSION.get_or_init(|| {

        #[cfg(all(target_os = "windows",feature = "gpu"))]
        let eps = [
            ort::execution_providers::CUDAExecutionProvider::default().build(),
            ort::execution_providers::DirectMLExecutionProvider::default().build(),
        ];

        #[cfg(all(target_os = "windows",not(feature = "gpu")))]
        let eps = [];

        #[cfg(target_os = "macos")]
        let eps = [ort::execution_providers::CoreMLExecutionProvider::default().build()];

        #[cfg(target_os = "linux")]
        let eps = [ort::execution_providers::CUDAExecutionProvider::default().build()];

        ort::init().with_execution_providers(eps).commit().expect("init session failed with execution providers");

        ort::session::Session::builder().expect("init session failed with builder")
        .with_optimization_level(GraphOptimizationLevel::Level3).expect("init session failed with optimization level")
        .commit_from_memory(MODEL_BYTES).expect("init session failed with load model")
    })
}

pub fn predict(origin_img: ImageBuffer<Rgba<u8>, Vec<u8>>) -> ort::Result<Vec<Detection>> {
    let img =
        DynamicImage::from(origin_img).resize_exact(IMAGE_WIDTH as u32, IMAGE_HEIGHT as u32, FilterType::Triangle);

    // 按 CHW 顺序一次性写入扁平缓冲区。
    //
    // 原实现用 input[[0, c, y, x]] 逐像素赋值：每写一个元素都要做一次多维下标
    // 计算与边界检查，640×640×3 ≈ 123 万次，是预处理里最贵的一环。
    // 改成顺序写入后，编译器可以把整段循环优化成无分支的连续写。
    // 数值结果与原实现完全一致（输出张量形状不变）。
    let plane = IMAGE_WIDTH * IMAGE_HEIGHT;
    let mut buffer = vec![0f32; 3 * plane];
    let raw = img.as_rgba8().expect("resize_exact 不改变图像类型").as_raw();
    for (i, px) in raw.chunks_exact(4).enumerate() {
        buffer[i] = px[0] as f32 / 255.0;
        buffer[plane + i] = px[1] as f32 / 255.0;
        buffer[2 * plane + i] = px[2] as f32 / 255.0;
    }
    let input = Array::from_shape_vec((1, 3, IMAGE_WIDTH, IMAGE_HEIGHT), buffer)
        .expect("缓冲区长度与形状必然匹配");

    let outputs = session().run(inputs!["images" => input.view()]?)?;
    let output = outputs["output"].try_extract_tensor::<f32>()?.view().t().slice(s![.., .., 0]).t().to_owned();

    let mut detections = output
        .rows()
        .into_iter()
        .filter_map(|row| {
            let (class_id, max_prob) =
                (5..20).map(|idx| (idx - 5, row[idx])).max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap()).unwrap();

            let conf = row[4] * max_prob;
            if conf < CONFIDENCE_THRESHOLD {
                None
            } else {
                Some(Detection::new(row[0], row[1], row[2], row[3], class_id, conf))
            }
        })
        .collect();

    Ok(nms(&mut detections))
}

#[derive(Debug, Clone, Copy)]
pub struct Detection {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub confidence: f32,
    pub label: char,

    idx: usize,
    area: f32,
}

impl Detection {
    fn new(x: f32, y: f32, w: f32, h: f32, idx: usize, confidence: f32) -> Self {
        Self {
            x0: x - w / 2.0,
            x1: x + w / 2.0,
            y0: y - h / 2.0,
            y1: y + h / 2.0,
            area: w * h,
            idx,
            label: LABELS[idx],
            confidence,
        }
    }

    // 计算两个检测框的IOU(交并比)
    #[inline]
    fn iou(&self, other: &Detection) -> f32 {
        let inter_width = (self.x1.min(other.x1) - self.x0.max(other.x0)).max(0.0);
        let inter_height = (self.y1.min(other.y1) - self.y0.max(other.y0)).max(0.0);
        let intersection = inter_width * inter_height;
        intersection / (self.area + other.area - intersection)
    }
}

// 使用IOU计算去除重叠的检测框（非极大值抑制）
fn nms(detections: &mut Vec<Detection>) -> Vec<Detection> {
    detections.sort_unstable_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap());
    let mut filtered_detections = Vec::with_capacity(33);
    let mut sizemap = [0; 15];
    while let Some(current) = detections.pop() {
        // 跳过多出的部分
        if sizemap[current.idx] + 1 > LIMIT[current.idx] {
            continue;
        }
        filtered_detections.push(current);
        sizemap[current.idx] += 1;
        detections.retain(|detection| current.iou(detection) < IOU_THRESHOLD);
    }
    filtered_detections
}
