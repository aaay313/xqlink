use std::collections::VecDeque;
use std::thread;
use std::time::Duration;

use tauri::async_runtime::block_on;
use tauri::AppHandle;
use tauri::Emitter as _;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;
use xcap::image::ImageBuffer;
use xcap::image::Rgba;

use crate::chess;
use crate::common;
use crate::engine::QueryResult;
use crate::listen::ListenWindow;
use crate::listen::Window;
use crate::yolo::predict;
use crate::yolo::IMAGE_HEIGHT;
use crate::yolo::IMAGE_WIDTH;
use crate::SHARED_STATE;

/// 二次确认棋盘前等待的毫秒数。
///
/// ⚠️ 这个值不能太小：被监听的象棋软件走子时有动画，动画中间帧会让识别器
/// 看到"棋子在原位置和新位置同时存在"的盘面。延时太短会让两次确认都落在
/// 动画期间并"确认成功"，从而把错误盘面提交到界面（表现为多出一个棋子）。
/// 100~150ms 能让确认跨越动画窗口，确认失败时会在下一轮自动重试。
const CONFIRM_DELAY_MS: u64 = 120;

/// 连续识别到非法棋盘多少次后重置识别上下文，避免无限空转。
const MAX_INVALID_BOARD: usize = 5;

/// 连续几次稳定性确认失败后就不再等待、直接用当前识别结果刷新界面。
/// 上限存在的意义：识别环境差时（窗口被遮挡、虚拟显示器抓图异常）盘面可能长期
/// 无法稳定，若一直等待界面就会永久停在旧盘面上不刷新，那比显示不完美的盘面更糟。
const MAX_CONFIRM_FAIL: usize = 4;

// 棋盘分析结果
struct BoardAnalysisResult {
    expect_move: chess::Changed,
    expect_board: [[char; 9]; 10],
}

// 定义不同的棋盘状态
//
// ⚠️ 状态语义统一为"最后一手由谁走出"，不要与"现在轮到谁"混用 ——
// 原实现里这两种含义在不同分支间混着用，是轮次标志与真实轮次脱节的根源。
// 各状态与轮次的对应关系见 AnalysisContext::sync_turn。
#[derive(PartialEq)]
enum ChessboardState {
    Initial,      // 初始状态，正在重建对盘面的认知
    StartPos,     // 开局盘面（尚未走过任何一手）
    OurTurn,      // 最后一手是我方走出 → 现在等对方
    OpponentTurn, // 最后一手是对方走出 → 现在轮到我方
    Idle,         // 轮次未知（无从推断），保持现状
    Invalid,      // 无效状态
}

// 分析上下文，保存分析状态和共享数据
struct AnalysisContext {
    app: AppHandle,
    window: ListenWindow,
    last_board: [[char; 9]; 10],
    expect_move: chess::Changed,
    expect_board: [[char; 9]; 10],
    invalid_change_count: usize,
    invalid_board_count: usize,
    confirm_fail_count: usize,
    /// 是否轮到我方行棋。由观测到的走子方推断：
    /// 我方走子后置 false（等待对方应手），对方走子后置 true（可以给出建议）。
    our_turn: bool,
    /// 最近一次引擎评估的分数（我方视角，正数对我方有利）。
    /// 棋谱会把每一手走出前的评分记下来，复盘时据此看形势起伏。
    last_score: isize,
    /// 上一次推送界面的盘面 FEN（仅用于避免诊断日志刷屏）
    last_ui_fen: String,
    /// 上一次识别出的视角（哪一方在屏幕下方）。它是坐标系的基准，
    /// 一旦变化，所有格子的含义会整体旋转 180°，必须整盘重来。
    last_camp: Option<chess::Camp>,
    /// 棋格级多帧投票器，用于过滤单帧偶发误判
    voter: BoardVoter,
}

unsafe impl Send for AnalysisContext {}
unsafe impl Sync for AnalysisContext {}

impl AnalysisContext {
    fn new(app: AppHandle, window: ListenWindow) -> Self {
        Self {
            app,
            // state_for_thread: state,
            window,
            last_board: [[' '; 9]; 10],
            expect_move: chess::Changed::default(),
            expect_board: [[' '; 9]; 10],
            invalid_change_count: 0,
            invalid_board_count: 0,
            confirm_fail_count: 0,
            our_turn: true,
            last_score: 0,
            last_ui_fen: String::new(),
            last_camp: None,
            voter: BoardVoter::new(),
        }
    }

    /// 更新行棋方，并在变化时通知前端。
    /// 前端收到 false 会立刻清空着法建议与高亮（避免"对方还没走就显示我方怎么走"）。
    fn set_turn(&mut self, our_turn: bool) {
        if self.our_turn != our_turn {
            self.our_turn = our_turn;
            // 轮次切换是诊断"界面卡在等待对方落子"这类问题的关键线索，故用 info。
            // 每步棋最多输出一次，量级与"分析结果"相当。
            info!("行棋方切换 → {}", if our_turn { "我方" } else { "对方" });
            let _ = self.app.emit("turn", our_turn);
        }
    }

    /// 按状态机的最新状态统一同步"是否轮到我方行棋"。
    ///
    /// ⚠️ 轮次同步必须集中在这一处。原实现是在各分支里零散调用 set_turn 的，
    /// 只要有任意一条改变轮次的路径忘了调（例如 Initial 重建认知、
    /// 棋盘等于预期棋盘时的轮次翻转），标志就会与真实轮次脱节，而且**不会自愈**
    /// —— 表现为界面一直显示"等待对方落子"，对方走了也不恢复。
    ///
    /// 状态语义（= 最后一手由谁走出）：
    ///   OurTurn      —— 我方刚走完，现在等对方
    ///   OpponentTurn —— 对方刚走完，现在轮到我方
    ///   StartPos     —— 尚未走过任何一手；红先行，故我执红即轮到我方
    ///   Initial / Idle / Invalid —— 不携带轮次信息，保持现状
    fn sync_turn(&mut self, state: &ChessboardState, camp: &chess::Camp) {
        match state {
            ChessboardState::OurTurn => self.set_turn(false),
            ChessboardState::OpponentTurn => self.set_turn(true),
            ChessboardState::StartPos => self.set_turn(camp.eq(&chess::Camp::Red)),
            _ => {}
        }
    }

    // 检查是否需要终止分析线程
    fn should_stop(&self) -> bool {
        let state = SHARED_STATE.get().unwrap();
        state.listen_thread.lock().unwrap().is_none()
    }

    // 获取棋盘图像并分析
    fn capture_and_analyze_board(&mut self) -> Option<Capture> {
        let started = std::time::Instant::now();
        let image = self.window.capture()?;
        let captured = started.elapsed();

        let hash = image_hash(&image);
        let result = get_board(image);

        // 抓图与识别各占多少是调优最关心的指标，放 debug 级避免 INFO 下刷屏
        debug!("识别耗时 {:?}（其中抓图 {:?}）", started.elapsed(), captured);

        let prev_board = self.last_board;
        result.map(|(camp, board)| {
            // 先多帧投票过滤偶发误判，再用规则纠错兜底明显错误，
            // 最后才交给状态机做差分比较
            let voted = self.voter.push(board);
            let board = correct_with_rules(voted, prev_board);
            Capture { camp, board, hash }
        })
    }

    // 确认棋盘状态是否稳定
    fn confirm_board(&self, board: [[char; 9]; 10], hash: u64) -> bool {
        thread::sleep(Duration::from_millis(CONFIRM_DELAY_MS));

        let conf_image = match self.window.capture() {
            Some(image) => image,
            None => return false,
        };

        // 与首次抓图逐像素一致 → 识别结果必然相同，直接确认稳定。
        // 省掉的这次推理是整条链路里最贵的一步。
        if image_hash(&conf_image) == hash {
            debug!("二次抓图与首次完全一致，跳过重复识别");
            return true;
        }

        if let Some((_, conf_board)) = get_board(conf_image) {
            // 与首帧用同一套纠错，否则两边标准不一致会永远确认不上
            let conf_board = correct_with_rules(conf_board, self.last_board);
            return conf_board == board;
        }
        false
    }

    // 分析棋盘并返回结果
    fn analyze_board(&mut self, camp: &chess::Camp, board: [[char; 9]; 10]) -> Option<BoardAnalysisResult> {
        let fen = chess::board_fen(camp, board);
        let config = SHARED_STATE.get().unwrap().config.read().unwrap();
        let state = SHARED_STATE.get().unwrap();
        let mut engine = state.engine.lock().unwrap();
        let result = block_on(engine.search(&fen, &config.engine));
        result.as_ref()?;

        // 记下这次评估的分数（我方视角），棋谱会把它标在每一手上
        if let Some(engine_result) = result.as_ref() {
            self.last_score = engine_result.score;
        }

        let (expect_move, expect_board) =
            analyse(&self.app, result.unwrap(), board, self.our_turn)?;
        Some(BoardAnalysisResult { expect_move, expect_board })
    }

    // 更新UI显示
    fn update_ui(&mut self, camp: &chess::Camp, board: [[char; 9]; 10]) {
        let board_map = chess::board_map(board);
        // 诊断日志：只在盘面真正变化时记录，便于事后核对"界面显示的到底是什么"
        let fen = chess::board_fen(camp, board);
        if fen != self.last_ui_fen {
            info!("界面刷新 {} | mirror={}", fen, camp.is_black());
            self.last_ui_fen = fen;
        }
        // 缓存最新盘面给界面"复制局面"用
        if let Some(state) = SHARED_STATE.get()
            && let Ok(mut latest) = state.latest_fen.lock()
        {
            latest.clone_from(&self.last_ui_fen);
        }
        // 窗口关闭后 emit 会失败，忽略即可（避免监听线程崩溃）
        let _ = self.app.emit("mirror", camp.is_black());
        let _ = self.app.emit("position", &board_map);
    }

    // 处理移动事件（同时记入棋谱）
    fn handle_move(
        &mut self,
        changed: &chess::Changed,
        prev_board: [[char; 9]; 10],
        after_fen: String,
        ours: bool,
    ) {
        info!("走子动画 {} -> {} ({})", changed.from, changed.to, changed.piece);

        record_move(changed, prev_board, after_fen, ours, self.last_score);

        let _ = self.app.emit("move", changed);
    }

    // 处理错误变化计数
    fn handle_invalid_change(
        &mut self, last_board: [[char; 9]; 10], board: [[char; 9]; 10], camp: &chess::Camp,
    ) -> ChessboardState {
        if self.invalid_change_count < 3 {
            self.invalid_change_count += 1;
            let last_fen = chess::board_fen(camp, last_board);
            let current = chess::board_fen(camp, board);
            debug!("OneChanged last {}", last_fen);
            debug!("OneChanged current {}", current);
            ChessboardState::Invalid
        } else {
            // 如果出现次数超过3次，重置为初始状态
            debug!("OneChanged count=3, reload");
            self.invalid_change_count = 0;
            ChessboardState::Initial
        }
    }
}

/// 从"上一份盘面 → 当前盘面"推断最后一手由谁走出。
///
/// 用途：识别抖动会让程序掉回 Initial 状态重建认知，而它此时并不知道该轮到谁。
/// 只有把前后两份盘面对比一下，才能把轮次找回来 —— 这是轮次标志的自愈机制。
///
/// 返回 None 表示无从推断：没有可比对的基准盘面（刚启动 / 刚被重置），
/// 或者两帧之间的差异无法解释成"一步棋"（走子动画中间帧、识别误判等）。
fn infer_mover(prev: [[char; 9]; 10], board: [[char; 9]; 10]) -> Option<chess::Camp> {
    if prev == [[' '; 9]; 10] {
        return None;
    }
    let (changed, state) = chess::board_diff(prev, board);
    match state {
        chess::BoardChangeState::Move => Some(changed.camp),
        _ => None,
    }
}

/// 图像指纹：两张图内容完全相同时返回相同的值。
///
/// 用途：二次确认盘面时，如果第二次抓图和第一次**逐像素一致**，那么识别结果
/// 必然相同 —— 可以直接判定"盘面稳定"，省掉一次完整的 ONNX 推理
/// （那是整条识别链路里最贵的一步，约占 190ms 的绝大部分）。
/// 画面静止时（对方还没落子）每次确认都能省下这一步。
///
/// 用 64 位 FNV-1a 按 8 字节步进，1MB 图像约 1ms，比一次推理快两个数量级。
fn image_hash(image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let raw = image.as_raw();
    let mut hash = OFFSET;
    let mut chunks = raw.chunks_exact(8);
    for chunk in &mut chunks {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(chunk);
        hash ^= u64::from_le_bytes(bytes);
        hash = hash.wrapping_mul(PRIME);
    }
    for &byte in chunks.remainder() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// 一次「抓图 + 识别」的完整结果
struct Capture {
    camp: chess::Camp,
    board: [[char; 9]; 10],
    /// 本次抓图的指纹，供二次确认判断画面是否变过
    hash: u64,
}

/// 一步棋的记录，用于棋谱展示与导出。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoveRecord {
    /// 中文着法，如「炮二平五」
    pub chinese: String,
    /// 是否由我方走出
    pub ours: bool,
    /// 走出这步**之前**引擎对局面的评分（我方视角，正数对我方有利）。
    /// 复盘时用它观察形势起伏 —— 相邻两步的差值就是这一段双方交手的效果。
    pub score: isize,
    /// 这手走完**之后**的盘面（FEN），供棋谱回放时把棋盘还原到那一刻。
    pub fen: String,
}

/// 把这一步追加进棋谱。
///
/// 中文着法必须用**走子前**的盘面推算 —— 记谱规则依赖棋子走之前在哪条纵线上
/// （例如「炮二平五」的「二」来自起点），用走子后的盘面会得到错误的记谱。
fn record_move(
    changed: &chess::Changed,
    prev_board: [[char; 9]; 10],
    after_fen: String,
    ours: bool,
    score: isize,
) {
    // 坐标不完整时无法记谱（例如引擎一条着法都没给出）
    if changed.from.len() != 2 || changed.to.len() != 2 {
        return;
    }

    let iccs = format!("{}{}", changed.from, changed.to);
    let chinese = chess::board_move_chinese(prev_board, &iccs);
    if chinese.is_empty() {
        return;
    }

    if let Some(state) = SHARED_STATE.get()
        && let Ok(mut history) = state.history.lock()
    {
        history.push(MoveRecord { chinese, ours, score, fen: after_fen });
    }
}

/// 每种棋子的合法数量上限（与象棋初始配置一致）
const PIECE_LIMIT: [(char, usize); 14] = [
    ('K', 1),
    ('A', 2),
    ('B', 2),
    ('N', 2),
    ('R', 2),
    ('C', 2),
    ('P', 5),
    ('k', 1),
    ('a', 2),
    ('b', 2),
    ('n', 2),
    ('r', 2),
    ('c', 2),
    ('p', 5),
];

/// 用「棋子数量上限」这条硬规则纠正识别错误。
///
/// 识别偶尔会把某一格认成别的棋子 —— 最典型的就是界面上"凭空多出一个炮"。
/// 这类错误几乎必然让某类棋子超出它的合法数量。超量的格子里，凡是上一帧
/// 不是该棋子的，就判定为误判并还原：**真实走子只影响起止两格，绝不会让
/// 棋子凭空多出来**。
///
/// 注意只在超出上限时动手，不做更激进的推断 —— 宁可少改也不要误改。
fn correct_with_rules(board: [[char; 9]; 10], prev_board: [[char; 9]; 10]) -> [[char; 9]; 10] {
    let mut result = board;
    let mut corrected = false;

    for &(piece, limit) in &PIECE_LIMIT {
        let positions: Vec<(usize, usize)> = (0..10)
            .flat_map(|y| (0..9).map(move |x| (y, x)))
            .filter(|&(y, x)| result[y][x] == piece)
            .collect();

        if positions.len() <= limit {
            continue;
        }

        let mut excess = positions.len() - limit;
        for &(y, x) in &positions {
            if excess == 0 {
                break;
            }
            // 上一帧这一格不是它 → 多半是本帧新认错的
            if prev_board[y][x] != piece {
                result[y][x] = prev_board[y][x];
                excess -= 1;
                corrected = true;
            }
        }
    }

    if corrected {
        debug!("规则纠错：还原了超出数量上限的棋子");
    }

    result
}

/// 棋格级多帧投票器。
///
/// 单帧识别偶尔会在某一格出错（把兵认成炮之类）。对每个格子收集最近几帧的
/// 取值、取多数，可以把这种偶发错误过滤掉。
///
/// 关键设计：**平票时以最新一帧为准**。真实走子会让某两格各得 1 票（旧值与新值
/// 各半），此时取最新值，所以走子不会被推迟；而单帧误判在 3 帧窗口里是 1 票
/// 对 2 票，会被多数票推翻。
struct BoardVoter {
    frames: VecDeque<[[char; 9]; 10]>,
}

impl BoardVoter {
    /// 参与投票的帧数
    const WINDOW: usize = 3;

    fn new() -> Self {
        Self { frames: VecDeque::with_capacity(Self::WINDOW) }
    }

    /// 送入新一帧，返回投票后的盘面
    fn push(&mut self, board: [[char; 9]; 10]) -> [[char; 9]; 10] {
        self.frames.push_back(board);
        if self.frames.len() > Self::WINDOW {
            self.frames.pop_front();
        }

        // 只有一帧时无从投票
        if self.frames.len() < 2 {
            return board;
        }

        let latest = *self.frames.back().unwrap();
        let mut voted = [[' '; 9]; 10];
        for y in 0..10 {
            for x in 0..9 {
                voted[y][x] = self.vote_cell(y, x, latest[y][x]);
            }
        }
        voted
    }

    /// 单格投票：票多者胜；票数相同则采用最新一帧的值
    fn vote_cell(&self, y: usize, x: usize, latest: char) -> char {
        let mut best = latest;
        let mut best_count = self.frames.iter().filter(|f| f[y][x] == latest).count();

        for frame in self.frames.iter().rev() {
            let candidate = frame[y][x];
            if candidate == best {
                continue;
            }
            let count = self.frames.iter().filter(|f| f[y][x] == candidate).count();
            // 严格多于才替换 —— 平票时保留最新值
            if count > best_count {
                best = candidate;
                best_count = count;
            }
        }

        best
    }
}

pub fn get_board(image: ImageBuffer<Rgba<u8>, Vec<u8>>) -> Option<(chess::Camp, [[char; 9]; 10])> {
    // 推理失败（模型与运行时不匹配、内存不足等）不该让整个程序崩掉 ——
    // 丢掉这一帧、下一轮重试即可。原实现用的是 unwrap。
    let data = match predict(image) {
        Ok(data) => data,
        Err(err) => {
            warn!("识别推理失败，跳过本帧: {err}");
            return None;
        }
    };
    if let Ok((camp, mut board)) = common::detections_to_board(&data) {
        chess::board_fix(&camp, &mut board);
        Some((camp, board))
    } else {
        None
    }
}

pub fn analyse(
    app: &AppHandle, mut result: QueryResult, board: [[char; 9]; 10], our_turn: bool,
) -> Option<(chess::Changed, [[char; 9]; 10])> {
    // 引擎有可能一条着法都没返回（初始化未完成、搜索被中断等），
    // 此时强行取第一条会 panic。返回 None 让调用方保留上一次的预期盘面。
    let best_pv = result.pvs.first()?;
    let best_move = chess::board_move_chinese(board, best_pv);
    let expect_board = chess::board_move(board, best_pv);
    let expect_move = chess::Changed::from_pv(best_pv, board);

    let mut tmp_board = expect_board;
    result.moves.push(best_move);
    for pv in result.pvs.iter().skip(1).take(3) {
        let mv = chess::board_move_chinese(tmp_board, pv);
        result.moves.push(mv);
        tmp_board = chess::board_move(tmp_board, pv);
    }
    // 把结果发送给前端（带上行棋方信息，前端据此决定是否显示建议）
    result.our_turn = our_turn;
    info!("分析结果 {:?}", result);
    // 窗口可能已经被关闭，emit 失败不该让监听线程崩溃
    let _ = app.emit("analyse", result);

    // 返回一个预期move和预期board
    Some((expect_move, expect_board))
}

// 处理循环逻辑的主函数
fn process_analysis_loop(mut context: AnalysisContext) {
    let mut current_state = ChessboardState::Initial;
    // 上一轮的实际工作时长，用于把轮询间隔扣掉
    let mut last_elapsed = Duration::ZERO;

    loop {
        // 检查是否需要停止监听
        if context.should_stop() {
            debug!("listen stopped");
            break;
        }

        // 获取等待间隔
        let interval = SHARED_STATE.get().unwrap().config.read().unwrap().timer_interval;
        // 扣掉上一轮的工作耗时：一轮的真实周期应当是 max(间隔, 工作时长)，
        // 而不是「间隔 + 工作时长」。识别本身就要 150~200ms，远大于间隔，
        // 原实现等于每轮白白多等一个间隔。
        thread::sleep(Duration::from_millis(interval).saturating_sub(last_elapsed));

        let started = std::time::Instant::now();

        // 捕获并分析棋盘
        let capture = match context.capture_and_analyze_board() {
            Some(capture) => capture,
            None => {
                last_elapsed = started.elapsed();
                continue;
            }
        };

        let (camp, board) = (capture.camp, capture.board);
        trace!("{:?} {:?}", camp, board);

        // 视角一致性：camp 表示"哪一方的将在屏幕下方"，它是所有格子坐标的基准。
        // 一旦视角翻转，同一个坐标字符串指向的物理位置会整体旋转 180°，
        // 此时若继续用历史盘面做差分，棋子就会落到完全错误的格子上。
        if camp != chess::Camp::None {
            if let Some(prev) = context.last_camp {
                if prev != chess::Camp::None && prev != camp {
                    warn!(
                        "识别视角发生变化 {:?} -> {:?}，丢弃历史并整盘重新确认",
                        prev, camp
                    );
                    context.last_board = [[' '; 9]; 10];
                    context.invalid_change_count = 0;
                    context.invalid_board_count = 0;
                    context.confirm_fail_count = 0;
                    context.last_camp = Some(camp);
                    current_state = ChessboardState::Initial;
                    last_elapsed = started.elapsed();
                    continue;
                }
            }
            context.last_camp = Some(camp);
        }

        // 根据不同状态处理棋盘
        let next_state = match current_state {
            ChessboardState::Initial => {
                // 先确认盘面稳定（隔 CONFIRM_DELAY_MS 再识别一次并要求一致），
                // 用于过滤被监听软件走子动画的中间帧 —— 那种帧会同时出现
                // "起点和终点都有棋子"的画面，直接提交就会在界面上留下多余棋子。
                //
                // 但确认不能无限等待：识别环境差时（窗口被遮挡、虚拟显示器抓图异常）
                // 盘面可能长期不稳定，那样界面会永久停在旧盘面上不刷新。
                // 因此超过 MAX_CONFIRM_FAIL 次就放弃等待，直接用当前识别结果刷新界面。
                let stable = context.confirm_board(board, capture.hash);
                if stable {
                    context.confirm_fail_count = 0;
                } else {
                    context.confirm_fail_count += 1;
                    let confirm_interval =
                        SHARED_STATE.get().unwrap().config.read().unwrap().confirm_interval;
                    thread::sleep(Duration::from_millis(confirm_interval));
                }

                if !stable && context.confirm_fail_count <= MAX_CONFIRM_FAIL {
                    debug!(
                        "盘面尚未稳定({}/{}), 等待下一轮确认",
                        context.confirm_fail_count, MAX_CONFIRM_FAIL
                    );
                    ChessboardState::Initial
                } else {
                    if !stable {
                        warn!(
                            "盘面连续 {} 次确认失败，改用当前识别结果刷新界面",
                            context.confirm_fail_count
                        );
                        context.confirm_fail_count = 0;
                    }

                    // 在覆盖 last_board 之前留存上一份盘面：
                    // 它是重建轮次的唯一依据（见 infer_mover）
                    let prev_board = context.last_board;

                    // 始终刷新界面：界面必须跟随识别结果，不能冻结在旧盘面上
                    context.update_ui(&camp, board);
                    context.last_board = board;

                    // 合法性检查只决定"能不能分析"，不决定"能不能显示"
                    let valid = chess::board_check(board);
                    if valid {
                        context.invalid_board_count = 0;
                    } else {
                        context.invalid_board_count += 1;
                        let debug_fen = chess::board_fen(&camp, board);
                        if context.invalid_board_count > MAX_INVALID_BOARD {
                            warn!(
                                "盘面连续 {} 次识别非法，重置识别上下文: {}",
                                context.invalid_board_count, debug_fen
                            );
                            context.invalid_board_count = 0;
                            context.invalid_change_count = 0;
                        } else {
                            debug!("盘面识别非法(仍会显示): {}", debug_fen);
                        }
                    }

                    // 重建轮次：能走进 Initial 就说明此前的状态已经不可信，必须重新
                    // 判断现在轮到谁。原实现直接沿用 here 之前的 our_turn，一旦此前的
                    // 值已经错了（例如对方走子只被识别成 1 处变化而掉进 Initial），
                    // 界面就会永久停在"等待对方落子"，对方走了也不恢复。
                    let next_state = if chess::startpos(board) {
                        // 开局：红先行
                        context.set_turn(camp.eq(&chess::Camp::Red));
                        ChessboardState::StartPos
                    } else if let Some(mover) = infer_mover(prev_board, board) {
                        // 看得出最后一手是谁走的 → 当前轮次随之确定
                        if camp.eq(&mover) {
                            context.set_turn(false);
                            ChessboardState::OurTurn
                        } else {
                            context.set_turn(true);
                            ChessboardState::OpponentTurn
                        }
                    } else {
                        // 无从推断（没有基准盘面，或变化解释不成一步棋）：
                        // 保持现有轮次，交给下一次观测到走子时纠正
                        debug!("无法从盘面推断行棋方，保持当前轮次");
                        ChessboardState::Idle
                    };

                    // 只有合法盘面且轮到我方行棋才分析并推送建议。
                    // 必须放在 set_turn 之后 —— analyse 要带上正确的 our_turn，
                    // 前端据此决定是否展示建议。
                    if valid && context.our_turn {
                        if let Some(result) = context.analyze_board(&camp, board) {
                            context.expect_move = result.expect_move;
                            context.expect_board = result.expect_board;
                        }
                    } else if !context.our_turn {
                        debug!("当前非我方行棋，跳过分析");
                    }

                    next_state
                }
            }

            ChessboardState::StartPos => {
                // 判断棋盘是否仍然是初始棋盘
                if !chess::startpos(board) {
                    // 不再是初始棋盘，处理正常的棋局变化
                    if board == context.last_board {
                        ChessboardState::StartPos // 没有变化
                    } else {
                        // 有变化，更新UI并分析
                        let (changed, board_state) = chess::board_diff(context.last_board, board);

                        match board_state {
                            chess::BoardChangeState::Move => {
                                // 走子前的盘面 —— 记谱要用它推算中文着法
                                let prev_board = context.last_board;
                                context.last_board = board;
                                // 先把权威整盘推给前端（前端 state 只认这个），
                                // 再推 move 仅用于播放走子动画。
                                // 这样即便某一帧的 move 坐标有偏差，也只影响一次动画，
                                // 不会把错误状态永久留在界面上。
                                context.update_ui(&camp, board);
                                // 走子后的 FEN，记进棋谱供回放还原到这一刻
                                let after_fen = chess::board_fen(&camp, board);
                                context.handle_move(&changed, prev_board, after_fen, camp.eq(&changed.camp));

                                if camp.eq(&changed.camp) {
                                    // 我方移动：轮到对方，清空建议
                                    debug!("我方移动, {} -> {}, 跳过分析", changed.from, changed.to);
                                    context.set_turn(false);
                                    ChessboardState::OurTurn
                                } else {
                                    // 对方移动，需要分析
                                    debug!("对方移动, {} -> {}, 需要分析", changed.from, changed.to);
                                    context.set_turn(true);
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                }
                            }
                            chess::BoardChangeState::One => {
                                context.handle_invalid_change(context.last_board, board, &camp)
                            }
                            chess::BoardChangeState::Unknown => {
                                // 变化无法解释（通常是走子动画中间帧），
                                // 不直接画到界面，交给 Initial 分支做稳定性确认。
                                debug!("棋局变化未知，重置上下文");
                                ChessboardState::Initial
                            }
                        }
                    }
                } else if chess::Camp::Red.eq(&camp) {
                    // 开局且我方执红 → 红先行，轮到我方
                    context.set_turn(true);

                    if context.last_board == board {
                        // 防止重复分析
                        ChessboardState::StartPos
                    } else {
                        // 设置前端棋盘
                        context.last_board = board;
                        context.update_ui(&camp, board);

                        // 调用引擎查询
                        if let Some(result) = context.analyze_board(&camp, board) {
                            context.expect_move = result.expect_move;
                            context.expect_board = result.expect_board;
                        }

                        // 仍处在开局阶段，统一用 StartPos 表达（轮次由 sync_turn 推出）
                        ChessboardState::StartPos
                    }
                } else {
                    // 开局且我方执黑 → 红先行，等对方先走
                    debug!("对方先手，跳过分析");
                    context.set_turn(false);
                    context.last_board = board;
                    context.update_ui(&camp, board);
                    ChessboardState::StartPos
                }
            }

            ChessboardState::OurTurn | ChessboardState::OpponentTurn | ChessboardState::Idle => {
                // 判断棋盘是否未发生变化
                if board == context.last_board {
                    debug!("棋盘未发生变化，跳过分析");
                    current_state // 保持当前状态
                } else if board == context.expect_board {
                    // 符合预期棋盘：说明走出的正是引擎预测的那步棋
                    debug!("棋盘为预期棋盘，跳过分析");
                    let expect_move = context.expect_move.clone();
                    let expect_board = context.expect_board;
                    // 走子前的盘面 —— 记谱要用它推算中文着法
                    let prev_board = context.last_board;
                    context.last_board = expect_board;

                    // ⚠️ 这里也必须先把权威整盘推给前端，再推 move。
                    // 否则前端会拿"可能已经漂移的"旧状态去执行这一步 ——
                    // 典型后果是拖动中间帧留下的棋子没被清掉，界面上变成两个棋子。
                    context.update_ui(&camp, board);
                    // 走子后的 FEN，记进棋谱供回放还原到这一刻
                    let after_fen = chess::board_fen(&camp, board);
                    context.handle_move(&expect_move, prev_board, after_fen, camp.eq(&expect_move.camp));

                    // 更换下一个行动方。
                    // 命中这里说明刚有人走出了预测的那一步棋，因此轮次必须翻转：
                    // 我方走完 → 等对方；对方走完 → 轮到我方。
                    // 翻转结果由循环末尾的 sync_turn 统一落地成 our_turn ——
                    // 原实现只翻转状态却不同步轮次，是把界面卡死在
                    // "等待对方落子"的另一条元凶路径。
                    if current_state == ChessboardState::OurTurn {
                        ChessboardState::OpponentTurn
                    } else {
                        ChessboardState::OurTurn
                    }
                } else {
                    // 确认棋盘变化是否稳定
                    if !context.confirm_board(board, capture.hash) {
                        debug!("棋盘延迟确认失败");
                        let confirm_interval = SHARED_STATE.get().unwrap().config.read().unwrap().confirm_interval;
                        thread::sleep(Duration::from_millis(confirm_interval));
                        current_state // 保持当前状态
                    } else if !chess::board_check(board) {
                        // 识别出的棋盘非法（例如黑卒出现在不可能的行）
                        let debug_fen = chess::board_fen(&camp, board);
                        context.invalid_board_count += 1;
                        if context.invalid_board_count > MAX_INVALID_BOARD {
                            // 连续多次非法说明识别已失准，重置上下文而不是无限空转。
                            // 原实现只保持 current_state，会永久卡死在无效盘上
                            //（实测每 0.58s 空转一次，不再产生任何分析结果）。
                            warn!(
                                "棋盘连续 {} 次识别无效，重置识别上下文: {}",
                                context.invalid_board_count, debug_fen
                            );
                            context.invalid_board_count = 0;
                            context.invalid_change_count = 0;
                            context.last_board = [[' '; 9]; 10];
                            ChessboardState::Initial
                        } else {
                            debug!("棋盘识别无效: {}", debug_fen);
                            current_state // 保持当前状态
                        }
                    } else {
                        context.invalid_board_count = 0; // 本轮识别有效，计数清零
                        // 处理正常棋盘变化
                        let (changed, board_state) = chess::board_diff(context.last_board, board);

                        match board_state {
                            chess::BoardChangeState::Move => {
                                // 走子前的盘面 —— 记谱要用它推算中文着法
                                let prev_board = context.last_board;
                                context.last_board = board;
                                // 先把权威整盘推给前端（前端 state 只认这个），
                                // 再推 move 仅用于播放走子动画。
                                // 这样即便某一帧的 move 坐标有偏差，也只影响一次动画，
                                // 不会把错误状态永久留在界面上。
                                context.update_ui(&camp, board);
                                // 走子后的 FEN，记进棋谱供回放还原到这一刻
                                let after_fen = chess::board_fen(&camp, board);
                                context.handle_move(&changed, prev_board, after_fen, camp.eq(&changed.camp));

                                if camp.eq(&changed.camp) {
                                    // 我方移动：轮到对方，清空建议
                                    debug!("我方移动, {} -> {}, 跳过分析", changed.from, changed.to);
                                    context.set_turn(false);
                                    ChessboardState::OurTurn
                                } else {
                                    // 对方移动，需要分析
                                    debug!("对方移动, {} -> {}, 需要分析", changed.from, changed.to);
                                    context.set_turn(true);
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                }
                            }
                            chess::BoardChangeState::One => {
                                context.handle_invalid_change(context.last_board, board, &camp)
                            }
                            chess::BoardChangeState::Unknown => {
                                // 变化无法解释（通常是走子动画中间帧），
                                // 不直接画到界面，交给 Initial 分支做稳定性确认。
                                debug!("棋局变化未知，重置上下文");
                                ChessboardState::Initial
                            }
                        }
                    }
                }
            }

            ChessboardState::Invalid => {
                // 复位到初始状态，等待下一次有效的变化
                ChessboardState::Initial
            }
        };

        // 统一同步行棋方。
        // 所有会改变轮次的状态转换都必须经过这里 —— 只要有一条路径漏了，
        // 轮次标志就会与真实轮次脱节且不会自愈（界面永久"等待对方落子"）。
        context.sync_turn(&next_state, &camp);
        current_state = next_state;

        // 记录本轮耗时，下一轮据此扣减等待时间
        last_elapsed = started.elapsed();
    }
}

// 初始化Tauri的command处理
#[tauri::command]
pub async fn start_listen(app: AppHandle, target: Window) -> Result<(), String> {
    trace!("start_listen");
    if SHARED_STATE.get().unwrap().listen_thread.try_lock().is_err() {
        error!("current listen thread is running, please stop it first");
        return Err("已经在监听中".to_string());
    }

    // 初始化监听窗口模块
    let mut window = ListenWindow::new(&target, IMAGE_WIDTH, IMAGE_HEIGHT)
        .ok_or("找不到目标窗口，可能已经被关闭")?;
    let image = window.capture().ok_or("抓取窗口图像失败")?;

    let image_h = image.height();
    let image_w = image.width();

    let detections = predict(image).map_err(|e| format!("识别失败: {e}"))?;

    match common::detections_bound(image_w, image_h, &detections) {
        Ok((x, y, w, h)) => {
            window.set_sub_bound(x, y, w, h); // 设置窗口边界
        }
        Err(e) => {
            return Err(e); // 未识别到棋盘
        }
    }

    // 新一轮监听 = 新的一局，清空上一局的棋谱
    if let Some(state) = SHARED_STATE.get()
        && let Ok(mut history) = state.history.lock()
    {
        history.clear();
    }

    // 创建分析上下文
    let context = AnalysisContext::new(app.clone(), window);

    // 启动后台线程进行截图和处理
    let listen_thread = thread::spawn(move || {
        trace!("into thread");
        process_analysis_loop(context);
    });

    SHARED_STATE.get().unwrap().listen_thread.lock().unwrap().replace(listen_thread);

    Ok(())
}

#[tauri::command]
pub fn stop_listen() {
    info!("stop listen");
    let shared_state = SHARED_STATE.get().unwrap();
    if let Ok(mut state) = shared_state.listen_thread.lock()
        && let Some(listen_thread) = state.take() {
            // 释放锁，停止后台线程
            debug!("释放锁，停止后台线程");
            drop(state);
            listen_thread.join().unwrap();
        }
    debug!("stoped");
}

#[cfg(test)]
mod tests {
    use super::BoardVoter;
    use super::correct_with_rules;
    use super::image_hash;
    use super::infer_mover;
    use crate::chess;
    use xcap::image::ImageBuffer;
    use xcap::image::Rgba;

    fn empty() -> [[char; 9]; 10] {
        [[' '; 9]; 10]
    }

    #[test]
    fn infer_mover_without_baseline_returns_none() {
        // 没有可以比对的基准盘面（刚启动、或刚因识别失准被重置）时无从推断
        assert_eq!(infer_mover(empty(), empty()), None);
    }

    #[test]
    fn infer_mover_detects_black_move() {
        let mut prev = empty();
        prev[0][0] = 'r'; // a9 上的黑车
        let mut board = prev;
        board[0][0] = ' ';
        board[1][0] = 'r'; // 走到 a8

        assert_eq!(infer_mover(prev, board), Some(chess::Camp::Black));
    }

    #[test]
    fn infer_mover_detects_red_move() {
        let mut prev = empty();
        prev[9][4] = 'K'; // e0 上的红帅
        let mut board = prev;
        board[9][4] = ' ';
        board[8][4] = 'K'; // 走到 e1

        assert_eq!(infer_mover(prev, board), Some(chess::Camp::Red));
    }

    #[test]
    fn infer_mover_ignores_single_square_change() {
        // 凭空多出一枚棋子（走子动画中间帧、或误判）解释不成一步棋
        let mut prev = empty();
        prev[0][0] = 'r';
        let mut board = prev;
        board[9][8] = 'R';

        assert_eq!(infer_mover(prev, board), None);
    }

    #[test]
    fn infer_mover_ignores_unexplainable_change() {
        // 起点与终点对不上（diff 归类为 One）时同样不予推断
        let mut prev = empty();
        prev[0][0] = 'r';
        prev[0][8] = 'r';
        let mut board = prev;
        board[0][0] = ' ';

        assert_eq!(infer_mover(prev, board), None);
    }

    #[test]
    fn image_hash_matches_identical_content_and_differs_otherwise() {
        // 指纹用于判定"二次抓图是否与首次完全一致"：一致就跳过重复推理。
        // 万一它对不同内容给出相同指纹，就会错误地跳过确认 —— 所以必须验证。
        let a = ImageBuffer::from_pixel(4, 4, Rgba([1u8, 2, 3, 255]));
        let b = ImageBuffer::from_pixel(4, 4, Rgba([1u8, 2, 3, 255]));
        let c = ImageBuffer::from_pixel(4, 4, Rgba([1u8, 2, 4, 255]));

        assert_eq!(image_hash(&a), image_hash(&b), "内容相同必须得到相同指纹");
        assert_ne!(image_hash(&a), image_hash(&c), "内容不同必须得到不同指纹");
    }

    #[test]
    fn rules_remove_extra_piece() {
        // 红方只有两个炮，识别却多出一个 —— 应被还原（这正是"凭空多出炮"的场景）
        let mut prev = empty();
        prev[2][1] = 'C';
        prev[2][7] = 'C';
        prev[9][4] = 'K';

        let mut board = prev;
        board[5][3] = 'C';

        let fixed = correct_with_rules(board, prev);
        assert_eq!(fixed[5][3], ' ', "多出来的炮应被还原");
        assert_eq!(fixed[2][1], 'C', "原有棋子不该被动");
        assert_eq!(fixed[2][7], 'C', "原有棋子不该被动");
    }

    #[test]
    fn rules_leave_legal_board_untouched() {
        // 数量未超限时，任何改动都不该发生（哪怕棋子确实移动过）
        let mut prev = empty();
        prev[2][1] = 'C';

        let mut board = prev;
        board[2][1] = ' ';
        board[3][1] = 'C';

        assert_eq!(correct_with_rules(board, prev), board);
    }

    #[test]
    fn voter_follows_real_move_without_delay() {
        // 真实走子时新旧值各得一票，平票取最新 —— 走子必须立刻生效
        let mut voter = BoardVoter::new();
        let mut before = empty();
        before[9][4] = 'K';

        let mut after = before;
        after[9][4] = ' ';
        after[8][4] = 'K';

        assert_eq!(voter.push(before)[9][4], 'K');

        let voted = voter.push(after);
        assert_eq!(voted[8][4], 'K', "走子应立刻生效，不能被投票推迟");
        assert_eq!(voted[9][4], ' ', "起点应立刻清空");
    }

    #[test]
    fn voter_rejects_single_frame_glitch() {
        // 稳定局面里突然冒出一枚棋子，单帧误判应被多数票推翻
        let mut voter = BoardVoter::new();
        let mut stable = empty();
        stable[9][4] = 'K';

        voter.push(stable);
        voter.push(stable);

        let mut glitch = stable;
        glitch[5][3] = 'C';

        assert_eq!(voter.push(glitch)[5][3], ' ', "单帧误判应被多数票过滤");
    }
}
