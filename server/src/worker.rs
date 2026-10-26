use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
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

use crate::auto;
use crate::chess;
use crate::clock;
use crate::common;
use crate::engine::EngineConfig;
use crate::engine::QueryResult;
use crate::engine::QueryState;
use crate::knife;
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
    /// 自动走棋的尝试记录。
    ///
    /// 不能用"一次走完就永久记住这个盘面"来去重：若那次点击没生效，
    /// 盘面不会变化，状态机就不再分析，于是该局面被永久跳过 ——
    /// 表现就是"开了自动下棋但有时不走"。改按"时间窗 + 重试次数"控制。
    auto_attempt: Option<AutoAttempt>,
    /// 本把是否已经执行过自动认输。防止同一把里反复点认输。
    resigned_this_game: bool,
    /// 上一次尝试自动认输的时间。失败后限流重试，避免每帧狂点。
    last_resign_try: Option<std::time::Instant>,
    /// 本次监听是否已处理过「开始即投降」。每次点「开始分析」只处理一次。
    resign_on_start_checked: bool,
    /// 「我方走后」盘面的出现次数（长将防护用，哈希 -> 次数）
    seen_boards: HashMap<u64, u32>,
    /// 「轮到我方」盘面的出现次数。>0 说明这个局面已经循环过至少一圈
    turn_boards: HashMap<u64, u32>,
    /// 后台消息通道已确认无效 → 跳过（降级后置位）。
    message_input_broken: bool,
    /// 触摸注入通道已确认无效 → 跳过（降级后置位）。
    touch_broken: bool,
    /// 当前通道连续失败过的**不同**棋盘数。
    ///
    /// 不能用「同一手棋重试 3 次失败」来判通道死刑：JJ 的落子偶发被忽略
    /// （动画/焦点时序），同一手 3 次失败很常见。只有连续多手棋、多个不同
    /// 局面都走不动，才能断定这个通道真的不通。
    auto_fail_boards: u32,
    /// 上一次已计入失败统计的棋盘（同一棋盘只计一次）
    last_fail_board: Option<u64>,
}

/// 一次自动走棋尝试的状态。
#[derive(Clone, Copy)]
struct AutoAttempt {
    /// 尝试时的盘面（用于判断是否还是同一局面）
    board: [[char; 9]; 10],
    /// 本次使用的输入通道
    input: auto::InputMode,
    /// 针对该盘面已尝试的次数
    tries: u32,
    /// 上次尝试的时间
    at: std::time::Instant,
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
            auto_attempt: None,
            resigned_this_game: false,
            last_resign_try: None,
            resign_on_start_checked: false,
            seen_boards: HashMap::new(),
            turn_boards: HashMap::new(),
            message_input_broken: false,
            touch_broken: false,
            auto_fail_boards: 0,
            last_fail_board: None,
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

        // 刷新窗口几何：自动点击换算屏幕坐标、标定认输按钮都要用，
        // 窗口被移动/缩放后必须跟着更新。
        self.refresh_auto_frame();

        // 抓图与识别各占多少是调优最关心的指标，放 debug 级避免 INFO 下刷屏
        debug!("识别耗时 {:?}（其中抓图 {:?}）", started.elapsed(), captured);

        let prev_board = self.last_board;
        result.map(|(camp, board, conf)| {
            // 易混类别低置信度兜底（纠正"把兵认成马"这类同格换类误判），
            // 再做多帧投票过滤偶发误判，最后用规则纠错兜底明显错误
            let board = stabilize_confusable(board, conf, prev_board);
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

        if let Some((_, conf_board, conf)) = get_board(conf_image) {
            // 与首帧用同一套处理（兜底 + 纠错），否则两边标准不一致会永远确认不上
            let conf_board = stabilize_confusable(conf_board, conf, self.last_board);
            let conf_board = correct_with_rules(conf_board, self.last_board);
            return conf_board == board;
        }
        false
    }

    // 分析棋盘并返回结果
    fn analyze_board(&mut self, camp: &chess::Camp, board: [[char; 9]; 10]) -> Option<BoardAnalysisResult> {
        let fen = chess::board_fen(camp, board);
        let config = SHARED_STATE.get().unwrap().config.read().unwrap();

        // 飞刀开局优先：命中则直接用飞刀着法，否则交给引擎
        let mut result = match self.match_knife(camp, board, &config.engine) {
            Some(knife_result) => Some(knife_result),
            None => {
                let state = SHARED_STATE.get().unwrap();
                let mut engine = state.engine.lock().unwrap();
                block_on(engine.search(&fen, &config.engine))
            }
        };
        result.as_ref()?;

        // 记下这次评估的分数（我方视角），棋谱会把它标在每一手上
        if let Some(engine_result) = result.as_ref() {
            self.last_score = engine_result.score;
        }

        // -- 长将防护 ----------------------------------------------------
        // JJ 对长将（反复同一局面将军）会判负。当「轮到我方」的局面已经
        // 出现过（说明在循环，将不死对方），把会导致「已出现过盘面」的
        // 候选着法排到后面（稳定排序，未走过的着法保持引擎原优先级），
        // 优先打破循环 —— 覆盖"同一位置连续将不超过 5 步"的规则。
        let h0 = board_key(&board);
        let repeated = self.turn_boards.get(&h0).copied().unwrap_or(0) > 0;
        if repeated
            && let Some(r) = result.as_mut()
            && r.pvs.len() > 1
        {
            r.pvs.sort_by_key(|pv| {
                let eb = chess::board_move(board, pv);
                self.seen_boards.get(&board_key(&eb)).copied().unwrap_or(0)
            });
            info!("检测到重复局面（长将风险），已优先选择未走过的候选着法");
        }

        let (expect_move, expect_board) =
            analyse(&self.app, result.unwrap(), board, self.our_turn)?;

        // 登记走后的盘面与本局面的出现次数；防抖表过大时清空重建
        *self.seen_boards.entry(board_key(&expect_board)).or_insert(0) += 1;
        *self.turn_boards.entry(h0).or_insert(0) += 1;
        if self.seen_boards.len() > 1024 {
            self.seen_boards.clear();
            self.turn_boards.clear();
        }

        // 自动走棋：把引擎建议的着法直接点在棋盘上（若已开启）
        self.try_auto_move(camp, board, &expect_move);

        Some(BoardAnalysisResult { expect_move, expect_board })
    }

    /// 构造当前窗口的屏幕坐标映射（自动点击用）。
    fn screen_map(&self) -> Option<auto::ScreenMap> {
        let (x, y, w, h) = self.window.board_bound()?;
        Some(auto::ScreenMap {
            origin: self.window.client_origin(),
            scale: self.window.scale_factor(),
            board_rect: (x, y, w, h),
            hwnd: self.window.hwnd(),
        })
    }

    /// 刷新共享状态里的窗口几何 —— 标定「认输」按钮位置时要做坐标换算，
    /// 窗口被移动/缩放后必须跟着更新。
    fn refresh_auto_frame(&self) {
        if let Some(state) = SHARED_STATE.get()
            && let Ok(mut frame) = state.auto_frame.lock()
        {
            *frame = Some(auto::AutoFrame {
                origin: self.window.client_origin(),
                scale: self.window.scale_factor(),
                hwnd: self.window.hwnd(),
            });
        }
    }

    /// 自动走棋：把引擎建议的着法换算成屏幕坐标并输入。
    ///
    /// 重试策略（关键）：状态机只在**盘面变化时**才重新分析，所以一旦某次点击
    /// 没生效（窗口消息不被响应、窗口失焦等），盘面不会变化，就不会再触发本函数
    /// —— 表现为"开了自动下棋但有时不走"。因此这里按时间窗允许重试：
    /// - 同一盘面每隔 `RETRY_INTERVAL` 才允许再试一次（避免分析被多次调用时连点）
    /// - 同一通道连续试 3 次无进展 → 自动降级为系统输入
    fn try_auto_move(
        &mut self,
        camp: &chess::Camp,
        board: [[char; 9]; 10],
        expect: &chess::Changed,
    ) {
        const RETRY_INTERVAL: Duration = Duration::from_millis(1500);
        /// 同一棋盘重试到第几次就认定「这一手没走成」
        const DOWNGRADE_AFTER: u32 = 3;
        /// 连续多少个不同局面走不成，才判定当前通道不可用并降级
        const FAIL_BOARDS: u32 = 3;

        let (enabled, mode, configured_input) = {
            let config = SHARED_STATE.get().unwrap().config.read().unwrap();
            (
                config.auto.auto_play,
                config.auto.play_mode,
                config.auto.input_mode,
            )
        };
        if !enabled {
            return;
        }
        if expect.from.is_empty() || expect.to.is_empty() {
            return;
        }

        // 决定本次用哪个通道、是否到重试时机
        let now = std::time::Instant::now();
        let (tries, mut effective) = match self.auto_attempt {
            Some(att) if att.board == board => {
                if now.duration_since(att.at) < RETRY_INTERVAL {
                    return; // 距离上次尝试太近，等下一轮
                }
                let tries = att.tries + 1;
                let mut eff = att.input;

                // 同一棋盘的重试超过阈值 → 把这**一个棋盘**计入失败，
                // 但不立刻换通道（偶发忽略很常见）。
                if tries >= DOWNGRADE_AFTER {
                    let key = board_key(&board);
                    if self.last_fail_board != Some(key) {
                        self.last_fail_board = Some(key);
                        self.auto_fail_boards += 1;
                    }
                    // 连续 FAIL_BOARDS 个不同局面都走不动 → 才换下一级通道
                    if self.auto_fail_boards >= FAIL_BOARDS {
                        self.auto_fail_boards = 0;
                        if matches!(eff, auto::InputMode::Auto | auto::InputMode::Message) {
                            self.message_input_broken = true;
                            eff = if self.touch_broken {
                                warn!("后台点击在 {} 个局面均无效，改用系统输入", FAIL_BOARDS);
                                auto::InputMode::SendInput
                            } else {
                                warn!("后台点击在 {} 个局面均无效，改用触摸注入", FAIL_BOARDS);
                                auto::InputMode::Touch
                            };
                        } else if eff == auto::InputMode::Touch {
                            self.touch_broken = true;
                            warn!("触摸注入在 {} 个局面均无效，改用系统输入", FAIL_BOARDS);
                            eff = auto::InputMode::SendInput;
                        }
                    }
                }
                (tries, eff)
            }
            _ => {
                // 盘面变了 → 说明上一次自动走棋已生效（或对方已走），
                // 清空失败计数，让通道保持在当前级别。
                self.auto_fail_boards = 0;
                self.last_fail_board = None;
                (0, configured_input)
            }
        };
        // 已确认无效的通道直接跳过：消息 -> 触摸 -> 系统输入
        if matches!(effective, auto::InputMode::Auto | auto::InputMode::Message)
            && self.message_input_broken
        {
            effective = if self.touch_broken {
                auto::InputMode::SendInput
            } else {
                auto::InputMode::Touch
            };
        }
        if effective == auto::InputMode::Touch && self.touch_broken {
            effective = auto::InputMode::SendInput;
        }

        let Some(map) = self.screen_map() else { return; };
        let Some((from_row, from_col)) = parse_iccs_pos(&expect.from) else { return; };
        let Some((to_row, to_col)) = parse_iccs_pos(&expect.to) else { return; };

        // 记录本次尝试（先记，失败也能在下一轮按时间窗重试）
        self.auto_attempt = Some(AutoAttempt {
            board,
            input: effective,
            tries,
            at: now,
        });

        // 系统输入与触摸注入都必须让目标窗口在前台（否则点击落在别的窗口上，
        // 表现为"有时不走"）；窗口消息直接发给目标窗口，不必抢焦点。
        // 抢焦点后留 120ms 激活稳定窗，消除首击落空。
        if effective != auto::InputMode::Message {
            auto::focus_window(self.window.hwnd());
            thread::sleep(Duration::from_millis(120));
        }

        // 找一个与本次走子无关的空格，用于点击前清除残留选中态
        let clear = empty_cell(&board, (from_row, from_col), (to_row, to_col));
        let ok = auto::play_move(
            &map,
            camp,
            (from_row, from_col),
            (to_row, to_col),
            mode,
            effective,
            clear,
        );
        if ok {
            info!(
                "自动走棋（第 {} 次，{:?}）: {} -> {}",
                tries + 1,
                effective,
                expect.from,
                expect.to
            );
        } else {
            warn!("自动走棋输入失败: {} -> {}", expect.from, expect.to);
        }
    }

    /// 自动走棋兜底：盘面长时间没变化时按时间窗重试一次。
    ///
    /// 必须由「棋盘未发生变化」的分支调用 —— 那正是点击没生效后程序停住的地方。
    fn retry_auto_move(&mut self, camp: &chess::Camp, board: [[char; 9]; 10]) {
        if !self.our_turn {
            return;
        }
        let expect = self.expect_move.clone();
        self.try_auto_move(camp, board, &expect);
    }

    /// 本把是否需要自动认输：认输额度尚未用完。
    ///
    /// 额度语义："最多自动认输 N 把"。设成 3 时前 3 把各认输一次，
    /// 用满后自动认输停用，后续棋局正常自动下。
    fn should_resign_now(&self) -> bool {
        let config = SHARED_STATE.get().unwrap().config.read().unwrap();
        let limit = config.auto.auto_resign_limit;
        let count = config.auto.resign_count;
        if limit == 0 {
            debug!("自动认输未启用（认输额度为 0）");
            return false;
        }
        if count >= limit {
            debug!("自动认输额度已用满（{}/{}），不再认输", count, limit);
            return false;
        }
        if config.auto.resign_menu_point.is_none() {
            debug!("自动认输缺少「认输①入口」标定点");
        }
        true
    }

    /// 执行自动认输。
    ///
    /// JJ 象棋的退出/投降是**三步**，缺一步就停在中间：
    /// 1. 点底部工具栏的入口展开菜单
    /// 2. 点菜单里的「退出 / 投降」
    /// 3. 确认弹窗「退出将判负，确定要退出吗？」里点「退出」
    ///
    /// 对局结束、结算界面出现后，若开启了「自动开始下一局」再点一次。
    ///
    /// 返回 true 表示三步点击都已发出（投降大概率成功）；失败返回 false，
    /// 并通过 `auto_note` 事件把原因推到界面 —— 文件日志不可靠，
    /// 界面必须能看到失败原因，否则用户无从排查。
    fn do_resign(&mut self) -> bool {
        /// 结算动画与「开始下一局」按钮出现的等待时间。
        const SETTLE_WAIT: Duration = Duration::from_millis(3000);
        /// 展开菜单到菜单项可点的等待时间。
        const MENU_WAIT: Duration = Duration::from_millis(400);
        /// 点菜单项到确认弹窗出现的等待时间。
        const DIALOG_WAIT: Duration = Duration::from_millis(600);

        let state = SHARED_STATE.get().unwrap();
        let (limit, count, menu_pt, confirm_pt, dialog_pt, next_pt, auto_next) = {
            let config = state.config.read().unwrap();
            (
                config.auto.auto_resign_limit,
                config.auto.resign_count,
                config.auto.resign_menu_point,
                config.auto.resign_confirm_point,
                config.auto.resign_dialog_point,
                config.auto.next_game_point,
                config.auto.auto_next_game,
            )
        };
        let Some(map) = self.screen_map() else { return false; };
        let hwnd = self.window.hwnd();

        // 失败原因必须让用户看得见：文件日志会丢，事件不会。
        let note = |msg: &str| {
            warn!("{}", msg);
            let _ = self.app.emit("auto_note", msg.to_string());
        };

        // 标定存的是客户区坐标 → 交给系统换算成屏幕坐标。
        // 与 ScreenToClient 严格互逆，不受 DPI 感知状态影响。
        let to_screen = |p: (i32, i32)| -> Option<(i32, i32)> {
            auto::client_to_screen(hwnd, p.0, p.1)
        };

        // 第一步：展开底部工具菜单
        let Some(menu) = menu_pt else {
            note("自动认输未执行：尚未标定「认输①入口」，请到自动面板逐项标定三个点位");
            return false;
        };
        let Some((mx, my)) = to_screen(menu) else {
            note("自动认输未执行：认输①坐标换算失败（窗口句柄无效）");
            return false;
        };
        auto::focus_window(self.window.hwnd());
        if !auto::click_at(mx, my) {
            note("自动认输未执行：认输①（展开菜单）点击失败");
            return false;
        }
        thread::sleep(MENU_WAIT);

        // 第二步：点菜单里的「退出 / 投降」
        let Some(confirm) = confirm_pt else {
            note("自动认输停在第 1 步：尚未标定「认输②菜单项」，JJ 菜单已展开，请手动点「退出/投降」");
            return false;
        };
        let Some((cx, cy)) = to_screen(confirm) else {
            note("自动认输停在第 1 步：认输②坐标换算失败");
            return false;
        };
        if !auto::click_at(cx, cy) {
            note("自动认输停在第 1 步：认输②（菜单项）点击失败");
            return false;
        }

        // 第三步：确认弹窗点「退出」（弹窗文案："退出将判负，确定要退出吗？"）
        if let Some(dialog) = dialog_pt {
            thread::sleep(DIALOG_WAIT);
            if let Some((dx, dy)) = to_screen(dialog) {
                if !auto::click_at(dx, dy) {
                    note("自动认输停在第 2 步：认输③（确认弹窗「退出」）点击失败");
                }
            }
        } else {
            note("自动认输停在第 2 步：尚未标定「认输③确认退出」，JJ 会停在确认弹窗上");
        }

        // 记录一次认输额度
        {
            let mut config = state.config.write().unwrap();
            config.auto.resign_count = count + 1;
            config.save();
        }
        let _ = self.app.emit(
            "auto_note",
            format!("已自动认输（{}/{} 把）", count + 1, limit),
        );
        info!("已自动认输（{}/{} 把）", count + 1, limit);

        // 第四步（可选）：结算界面出现后开始下一局
        if auto_next {
            match next_pt {
                Some(p) => {
                    thread::sleep(SETTLE_WAIT);
                    if let Some((x, y)) = to_screen(p) {
                        auto::focus_window(self.window.hwnd());
                        if auto::click_at(x, y) {
                            info!("已自动开始下一局");
                        } else {
                            note("「开始下一局」点击失败");
                        }
                    }
                }
                None => note("已开启自动开下一局，但尚未标定「开始下一局」按钮"),
            }
        }
        true
    }

    /// 自动认输的统一入口：限流 + 更新本把认输标记。
    ///
    /// 失败后 10 秒内不重试 —— 否则缺标定时会每帧狂点 JJ 菜单。
    fn try_resign(&mut self) {
        if let Some(t) = self.last_resign_try
            && t.elapsed() < Duration::from_secs(10)
        {
            return;
        }
        self.last_resign_try = Some(std::time::Instant::now());
        if self.do_resign() {
            self.resigned_this_game = true;
        }
    }

    /// 飞刀开局匹配：命中时返回飞刀着法的分析结果，否则返回 None 交给引擎。
    ///
    /// - 先手飞刀（我方执红）：红方已走序列匹配某飞刀套路的前缀，走套路下一步。
    /// - 后手飞刀（我方执黑）：对方（红方）第一步命中某飞刀的 trigger 时，走对应应手。
    fn match_knife(
        &self,
        camp: &chess::Camp,
        _board: [[char; 9]; 10],
        config: &EngineConfig,
    ) -> Option<QueryResult> {
        if !config.flying_knife {
            return None;
        }

        let (name, next_move) = match camp {
            chess::Camp::Red => {
                // 先手飞刀：按红方已走序列匹配套路前缀
                let played = self.our_move_iccs();
                let (k, mv) = knife::match_red_knife(&config.knife, &played)?;
                (k.name, mv)
            }
            chess::Camp::Black => {
                // 后手飞刀：匹配红方第一步
                let trigger = self.opponent_first_move_iccs()?;
                let (k, mv) = knife::match_black_knife(&config.knife, &trigger)?;
                (k.name, mv)
            }
            chess::Camp::None => return None,
        };

        let mut result = QueryResult::default();
        result.pvs = vec![next_move.to_string()];
        result.source = format!("飞刀·{name}");
        result.state = QueryState::Success;
        Some(result)
    }

    /// 读取并清空用户手动指定的轮次（界面「轮到我方/对方行棋」按钮）。
    /// 返回 Some 时优先于自动判断，且只生效一次。
    fn take_turn_override(&self) -> Option<bool> {
        SHARED_STATE.get()?.turn_override.lock().ok()?.take()
    }

    /// 用头像读秒判断轮到谁行棋（判定原理见 clock 模块）。
    ///
    /// 正在行棋一方的头像上有倒计时读秒圈（暗红圆环+白色数字），等待方没有。
    /// 返回 Some(true)=轮到我方、Some(false)=轮到对方、None=无法判断。
    fn detect_turn_by_clock(&self) -> Option<bool> {
        let bound = self.window.board_bound()?;
        let img = self.window.capture_full()?;
        let verdict = clock::detect_turn(&img, bound);
        if let Some(t) = verdict {
            info!("读秒判定轮次 → {}", if t { "我方" } else { "对方" });
        }
        verdict
    }

    /// 我方（红方）本局已走的着法序列（ICCS），用于先手飞刀套路的逐步匹配。
    fn our_move_iccs(&self) -> Vec<String> {
        let Some(history) = SHARED_STATE.get().and_then(|s| s.history.lock().ok()) else {
            return Vec::new();
        };
        history.iter().filter(|m| m.ours).map(|m| m.iccs.clone()).collect()
    }

    /// 取「对方（红方）第一步」的 ICCS。我方执黑时，棋谱里第一条非我方记录即对方第一步。
    fn opponent_first_move_iccs(&self) -> Option<String> {
        let history = SHARED_STATE.get()?.history.lock().ok()?;
        history.iter().find(|m| !m.ours).map(|m| m.iccs.clone())
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
    /// 着法的 ICCS 坐标，如「h2e2」；供飞刀开局匹配「对方第一步」用
    pub iccs: String,
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
        history.push(MoveRecord { chinese, iccs, ours, score, fen: after_fen });
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

/// 易混类别低置信度时的「上一帧兜底」。
///
/// 实测模型在若干类别上存在"对峙样本"（如兵 P 的第二候选是马 N，
/// cls_p 0.69/0.17；象 b 的第二候选是马 n，0.88/0.30）—— 这类检测的
/// conf 仍可能超过入盘阈值，取最大值就可能取错，表现为"把兵识别成马"。
///
/// 纠错规则（只覆盖「同格同方换类」这一种误分类形态）：
///   上一帧该格有棋子、与本帧识别为同一方（同为红方或同为黑方）、但类别不同，
///   且本帧的类别置信度偏低 → 判为误分类，保留上一帧的值。
///
/// 走子（空格→棋子）与吃子（跨方换子）的形状是"有无变化"或"跨方变化"，
/// 不会命中此规则，因此不影响正常行棋。
fn stabilize_confusable(
    board: [[char; 9]; 10], conf: [[f32; 9]; 10], prev_board: [[char; 9]; 10],
) -> [[char; 9]; 10] {
    // 类别置信度低于此值视为"分类犹豫"。实测正常识别 cls_p > 0.9，
    // 对峙样本落在 0.65~0.90 区间。
    const CLS_CONF_THRESHOLD: f32 = 0.85;

    let mut result = board;
    let mut corrected = 0u32;

    for y in 0..10 {
        for x in 0..9 {
            let (cur, old) = (board[y][x], prev_board[y][x]);
            if cur == ' ' || old == ' ' || cur == old {
                continue;
            }
            if conf[y][x] >= CLS_CONF_THRESHOLD {
                continue; // 分类够自信，不动
            }
            // 同方判定：大写 = 红方，小写 = 黑方
            if cur.is_ascii_uppercase() == old.is_ascii_uppercase() {
                result[y][x] = old;
                corrected += 1;
            }
        }
    }

    if corrected > 0 {
        debug!("易混类别兜底：还原了 {corrected} 个低置信度换类格子");
    }

    result
}

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

/// ICCS 位置（如 "h2"）转成「红方在下」视角的棋盘坐标 (row, col)。
///
/// row 是**红方在下**视角的行号（0 = 黑方底线），与 `chess::Move::new` 一致；
/// 换算到屏幕坐标时的视角翻转由 `auto::ScreenMap::board_to_screen` 负责。
fn parse_iccs_pos(pos: &str) -> Option<(usize, usize)> {
    let mut cs = pos.chars();
    let col = cs.next()? as usize - 97; // 'a'..'i' → 0..8
    let digit = cs.next()? as usize; // '0'..'9' → 48..57
    let row = 57usize.checked_sub(digit)?; // '0'→9（底部），'9'→0（顶部）
    Some((row, col))
}

/// 识别一帧图像，返回 (阵营, 棋盘, 每格类别置信度)。
///
/// 类别置信度供调用方做「易混类别低置信度兜底」（见 stabilize_confusable）。
pub fn get_board(
    image: ImageBuffer<Rgba<u8>, Vec<u8>>,
) -> Option<(chess::Camp, [[char; 9]; 10], [[f32; 9]; 10])> {
    // 推理失败（模型与运行时不匹配、内存不足等）不该让整个程序崩掉 ——
    // 丢掉这一帧、下一轮重试即可。原实现用的是 unwrap。
    let mut data = match predict(image.clone()) {
        Ok(data) => data,
        Err(err) => {
            warn!("识别推理失败，跳过本帧: {err}");
            return None;
        }
    };

    // 原图未检出棋盘时，用反相图补一个棋盘框（深色皮肤适配，见 invert_image）。
    // 棋子仍取原图检测结果：反相后棋子样式（蓝底白字）同样偏离训练分布，
    // 实测同一局面原图重建 22 子、反相仅 12 子 —— 故反相只借棋盘框，不借棋子。
    if !data.iter().any(|d| d.label == '0') {
        if let Ok(inverted) = predict(common::invert_image(&image)) {
            if let Some(board_det) = inverted.into_iter().find(|d| d.label == '0') {
                trace!("原图未检出棋盘，借反相图的棋盘框（深色皮肤）");
                data.push(board_det);
            }
        }
    }

    if let Ok((camp, mut board, conf)) = common::detections_to_board(&data) {
        chess::board_fix(&camp, &mut board);
        Some((camp, board, conf))
    } else {
        None
    }
}

/// 找一个与本次着法无关的空格，用于点击前清除残留选中态。
///
/// 从棋盘中部向外找：中部空格在绝大多数局面下都存在，且离两端着法远，
/// 误点导致的意外走子概率最低（空格本身不可走，点了只是取消选中）。
fn empty_cell(board: &[[char; 9]; 10], from: (usize, usize), to: (usize, usize)) -> Option<(usize, usize)> {
    let mut best: Option<((usize, usize), i32)> = None;
    for r in 0..10 {
        for c in 0..9 {
            if board[r][c] != ' ' || (r, c) == from || (r, c) == to {
                continue;
            }
            // 距离取到 from / to 的最小值，越大越安全
            let d = |a: (usize, usize), b: (usize, usize)| -> i32 {
                ((a.0 as i32 - b.0 as i32).abs()) + ((a.1 as i32 - b.1 as i32).abs())
            };
            let score = d((r, c), from).min(d((r, c), to));
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some(((r, c), score));
            }
        }
    }
    best.map(|(p, _)| p)
}

/// 盘面哈希：长将防护用（同一盘面出现次数统计）。
fn board_key(board: &[[char; 9]; 10]) -> u64 {
    let mut h = DefaultHasher::new();
    board.hash(&mut h);
    h.finish()
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

    // pvs 现在是「候选着法列表」：每个候选都是对同一局面的并列选择，
    // 中文着法必须都基于**原始盘面**推算，而不是逐个连续执行。
    result.moves.push(best_move);
    for pv in result.pvs.iter().skip(1).take(3) {
        let mv = chess::board_move_chinese(board, pv);
        result.moves.push(mv);
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

        // 用户手动切换轮次（界面「轮到我方 / 对方行棋」按钮）优先于自动判断。
        // 切到「轮到我方」时立刻用最近一次识别到的盘面分析并给出建议，
        // 用户点完马上能看到着法，不必等下一手。
        if let Some(manual) = context.take_turn_override() {
            let next_state = if manual {
                info!("手动切换轮次 → 轮到我方，用最近盘面分析");
                context.set_turn(true);
                if let Some(camp) = context.last_camp {
                    if camp != chess::Camp::None && context.last_board != [[' '; 9]; 10] {
                        if let Some(result) = context.analyze_board(&camp, context.last_board) {
                            context.expect_move = result.expect_move;
                            context.expect_board = result.expect_board;
                        }
                    }
                }
                ChessboardState::OpponentTurn
            } else {
                info!("手动切换轮次 → 等待对方落子");
                context.set_turn(false);
                ChessboardState::OurTurn
            };
            current_state = next_state;
            last_elapsed = started.elapsed();
            continue;
        }

        // 界面请求的「立即认输」：用于验证标定点，或中途启动监听时手动触发。
        // 必须在抓图之前处理 —— do_resign 会依次点击并有等待，不该夹在识别中间。
        let resign_requested = SHARED_STATE
            .get()
            .map(|s| {
                let mut flag = s.resign_request.lock().unwrap();
                let v = *flag;
                *flag = false;
                v
            })
            .unwrap_or(false);
        if resign_requested {
            info!("收到「立即认输」请求");
            if context.do_resign() {
                context.resigned_this_game = true;
            }
        }

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

                    // 开始分析就投降：认输额度还有剩余时，直接执行一次，
                    // 不必等识别到新开局，也不必手点「立即认输」。
                    // 每次「开始分析」只处理一次，投降后继续走正常流程。
                    if !context.resign_on_start_checked {
                        context.resign_on_start_checked = true;
                        if context.should_resign_now() {
                            info!("开始分析时认输额度仍有剩余 → 立即自动投降");
                            context.try_resign();
                            context.resigned_this_game = true;
                            // 投降后盘面会变化，回到 Initial 重新识别
                            continue;
                        }
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
                                // 对方先手时，对方这一步就是「这一把的开局之后」——
                                // 此时认输按钮可用，执行认输。
                                // 我方先手已在 StartPos 分支认过，这里不重复。
                                if !context.resigned_this_game
                                    && chess::Camp::Black.eq(&camp)
                                    && context.should_resign_now()
                                {
                                    context.try_resign();
                                }
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

                                // 走子后轮次：头像读秒优先（正在行棋一方头像上有读秒圈，
                                // 比盘面推断可靠，盘面推断会被识别误差带偏）；
                                // 读秒不可用时退回按走子方推断。
                                let our_turn = context
                                    .detect_turn_by_clock()
                                    .unwrap_or_else(|| !camp.eq(&changed.camp));
                                context.set_turn(our_turn);
                                if our_turn {
                                    // 对方刚走完，轮到我方，需要分析
                                    debug!("对方移动, {} -> {}, 需要分析", changed.from, changed.to);
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                } else {
                                    // 我方刚走完：轮到对方，清空建议
                                    debug!("我方移动, {} -> {}, 跳过分析", changed.from, changed.to);
                                    ChessboardState::OurTurn
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

                        // 新的一局开始：重置"本把是否已认输"
                        context.resigned_this_game = false;
                        context.seen_boards.clear();
                        context.turn_boards.clear();
                        // 我方执红 = 我方先手，棋局刚开局就能认输，直接认输
                        if context.should_resign_now() {
                            context.try_resign();
                        }

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
                    // 新的一局开始：重置"本把是否已认输"
                    // 认输要等对方走出第一步（此时认输按钮才可用），见 StartPos→Move 分支
                    if context.last_board != board {
                        context.resigned_this_game = false;
                        context.seen_boards.clear();
                        context.turn_boards.clear();
                    }
                    context.last_board = board;
                    context.update_ui(&camp, board);
                    ChessboardState::StartPos
                }
            }

            ChessboardState::OurTurn | ChessboardState::OpponentTurn | ChessboardState::Idle => {
                // 判断棋盘是否未发生变化
                if board == context.last_board {
                    // 兜底重试自动走棋：点击没生效时盘面不会变，
                    // 这里是唯一还能再试一次的时机（内部按时间窗限流）。
                    context.retry_auto_move(&camp, board);
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

                                // 走子后轮次：头像读秒优先（正在行棋一方头像上有读秒圈，
                                // 比盘面推断可靠，盘面推断会被识别误差带偏）；
                                // 读秒不可用时退回按走子方推断。
                                let our_turn = context
                                    .detect_turn_by_clock()
                                    .unwrap_or_else(|| !camp.eq(&changed.camp));
                                context.set_turn(our_turn);
                                if our_turn {
                                    // 对方刚走完，轮到我方，需要分析
                                    debug!("对方移动, {} -> {}, 需要分析", changed.from, changed.to);
                                    if let Some(result) = context.analyze_board(&camp, board) {
                                        context.expect_move = result.expect_move;
                                        context.expect_board = result.expect_board;
                                    }
                                    ChessboardState::OpponentTurn
                                } else {
                                    // 我方刚走完：轮到对方，清空建议
                                    debug!("我方移动, {} -> {}, 跳过分析", changed.from, changed.to);
                                    ChessboardState::OurTurn
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

    let mut detections = predict(image.clone()).map_err(|e| format!("识别失败: {e}"))?;

    // 原图未检出棋盘时尝试反相：深色皮肤棋盘（暗底亮线）不在模型训练分布内，
    // 反相后变回"亮底暗线"即可检出（实测置信度 0→0.95）。
    // 没有这一步时，深色皮肤会让「启」直接失败在"未识别到棋盘"。
    if !detections.iter().any(|d| d.label == '0') {
        if let Ok(inverted) = predict(common::invert_image(&image)) {
            if inverted.iter().any(|d| d.label == '0') {
                info!("原图未检出棋盘，反相后检出（深色棋盘皮肤）");
                detections = inverted;
            }
        }
    }

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
    use super::stabilize_confusable;
    use crate::chess;
    use xcap::image::ImageBuffer;
    use xcap::image::Rgba;

    fn empty() -> [[char; 9]; 10] {
        [[' '; 9]; 10]
    }

    fn conf_map(v: f32) -> [[f32; 9]; 10] {
        [[v; 9]; 10]
    }

    #[test]
    fn stabilize_keeps_prev_when_same_side_low_confidence() {
        // 实际是红兵，本帧低置信度认成红马 → 应保留红兵（用户反馈的"兵→马"）
        let mut prev = empty();
        prev[5][4] = 'P';
        let mut board = prev;
        board[5][4] = 'N';
        let out = stabilize_confusable(board, conf_map(0.69), prev);
        assert_eq!(out[5][4], 'P', "同格同方换类且低置信度，应还原为上一帧");
    }

    #[test]
    fn stabilize_trusts_model_when_confidence_high() {
        // 高置信度的换类（cls_p >= 0.85）不拦截 —— 宁可信任模型也不要误改
        let mut prev = empty();
        prev[5][4] = 'P';
        let mut board = prev;
        board[5][4] = 'N';
        let out = stabilize_confusable(board, conf_map(0.95), prev);
        assert_eq!(out[5][4], 'N', "高置信度换类应保留本帧识别");
    }

    #[test]
    fn stabilize_ignores_normal_moves() {
        // 走子：上一帧空格 → 本帧有子。即便低置信度也不能拦（否则永远走不动）
        let mut prev = empty();
        prev[5][4] = ' ';
        let mut board = prev;
        board[4][4] = 'P'; // 红兵前进一格
        let out = stabilize_confusable(board, conf_map(0.6), prev);
        assert_eq!(out[4][4], 'P', "空格→棋子 不应被兜底拦截");
    }

    #[test]
    fn stabilize_ignores_captures() {
        // 吃子：上一帧是黑卒 → 本帧红马落位（跨方换子）。低置信度也不能拦
        let mut prev = empty();
        prev[5][4] = 'p';
        let mut board = prev;
        board[5][4] = 'N';
        let out = stabilize_confusable(board, conf_map(0.6), prev);
        assert_eq!(out[5][4], 'N', "跨方换子 不应被兜底拦截");
    }

    #[test]
    fn stabilize_ignores_unchanged_cells() {
        // 类别相同：无需处理
        let mut prev = empty();
        prev[5][4] = 'P';
        let board = prev;
        let out = stabilize_confusable(board, conf_map(0.5), prev);
        assert_eq!(out[5][4], 'P');
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
