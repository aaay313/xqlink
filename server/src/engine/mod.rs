pub mod chessdb;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
mod command;

use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::chess;

#[derive(Debug, serde::Serialize, Default, Clone)]
pub struct QueryResult {
    pub depth: usize,       // 深度
    pub score: isize,       // 得分
    pub time: usize,        // 时间
    pub pvs: Vec<String>,   // 思考(iccs)
    pub moves: Vec<String>, // 思考(chinese)
    pub state: QueryState,  // 状态
    pub source: String,     // 来源
    pub our_turn: bool,     // 是否轮到我方行棋（前端据此决定是否显示建议）
}

const SOURCE_ENGINE: &str = "引擎";

#[derive(Debug, serde::Serialize, Default, Clone, Copy)]
pub enum QueryState {
    Success,
    #[default]
    NotResult,
    InvalidBoard,
    ServerInternalError, // 内部错误
}

/// 人机棋手强度三档。映射到引擎的 Skill Level（0~20，20 为满强度）。
///
/// 弱/中档让引擎故意走出次优着法，用于陪新手对练；强档保持引擎原始满血水平。
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    Weak,
    Medium,
    #[default]
    Strong,
}

impl Strength {
    /// 三档对应的引擎 Skill Level 取值。
    pub fn skill_level(self) -> usize {
        match self {
            Strength::Weak => 6,
            Strength::Medium => 12,
            Strength::Strong => 20,
        }
    }
}

/// 行棋风格三档。引擎没有 Contempt 参数，故用 MultiPV 取多个候选着法，
/// 再在后端按「进攻性」重新排序来实现。
///
/// - Balanced：保持引擎默认，直接取评分最高的着法（MultiPV=1）。
/// - Attack：在评分接近的候选里，优先吃子、前压等主动着法。
/// - Defense：在评分接近的候选里，优先稳健、避免失子的着法。
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    #[default]
    Balanced,
    Attack,
    Defense,
}

impl Style {
    /// 该风格需要引擎返回的候选着法数量。
    pub fn multi_pv(self) -> usize {
        match self {
            Style::Balanced => 1,
            Style::Attack | Style::Defense => 5,
        }
    }
}

#[derive(Debug, serde::Serialize, Clone, serde::Deserialize)]
pub struct EngineConfig {
    pub depth: usize,
    pub time: usize,
    pub threads: usize,
    pub hash: usize,
    pub show_wdl: bool,
    pub chessdb_enabled: bool,
    pub chessdb_timeout: u64,
    /// 人机棋手强度（弱/中/强）。`#[serde(default)]` 保证旧配置文件缺字段时平滑升级。
    #[serde(default)]
    pub strength: Strength,
    /// 行棋风格（进攻/防守/均衡）。
    #[serde(default)]
    pub style: Style,
    /// 飞刀开局开关：开启后在开局第一步走预设飞刀着法。
    #[serde(default)]
    pub flying_knife: bool,
    /// 选中的飞刀名（"none" 关闭，"random" 随机）。
    #[serde(default)]
    pub knife: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            depth: 20,
            time: 5000,
            threads: 4,
            hash: 64,
            show_wdl: false,
            chessdb_enabled: true,
            chessdb_timeout: 5,
            strength: Strength::default(),
            style: Style::default(),
            flying_knife: false,
            knife: "none".to_string(),
        }
    }
}

pub struct Engine {
    stdin: Box<dyn Write>,
    stdout: Box<dyn BufRead>,
    child: std::process::Child, // 添加子进程字段
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn new(libs: &Path) -> Self {
        let nnue = libs.join("pikafish.nnue");

        // command::new 直接返回父进程侧的管道两端。
        // Windows 下不走 Stdio::piped()，原因见 command.rs 顶部注释。
        let (child, (stdin, stdout)) = command::new(libs);

        let mut eng = Engine { stdin, stdout, child };
        eng.setoption("EvalFile", nnue.display());
        eng.setoption("Sixty Move Rule", false);
        eng
    }

    pub fn reload(&mut self, libs: &Path, config: &EngineConfig) {
        // 引擎可能已经自行退出（崩溃、被系统回收），此时 kill 会返回错误。
        // 忽略它 —— 后面反正要重建一个新引擎。
        let _ = self.child.kill();
        let _ = self.child.wait();
        *self = Self::new(libs);
        self.set_hash(config.hash);
        self.set_show_wdl(config.show_wdl);
        self.set_threads(config.threads);
    }

    fn write_command<A: Display>(&mut self, args: A) {
        // 引擎退出后管道会断开，写入失败只记日志，不该让监听线程崩溃
        if let Err(err) = writeln!(self.stdin, "{}", args) {
            warn!("向引擎写入命令失败（引擎可能已退出）: {err}");
        }
        if let Err(err) = self.stdin.flush() {
            warn!("刷新引擎输入缓冲失败: {err}");
        }
        debug!("{}", args);
    }

    pub fn set_show_wdl(&mut self, open: bool) { self.setoption("UCI_ShowWDL", open); }

    pub fn set_threads(&mut self, num: usize) { self.setoption("Threads", num); }

    pub fn set_hash(&mut self, size: usize) { self.setoption("Hash", size); }

    pub fn set_skill_level(&mut self, level: usize) { self.setoption("Skill Level", level); }

    pub fn set_multi_pv(&mut self, n: usize) { self.setoption("MultiPV", n); }

    pub fn setoption<T: Display>(&mut self, name: &str, value: T) {
        self.write_command(format!("setoption name {} value {}", name, value))
    }

    pub fn position(&mut self, fen: &str) { self.write_command(format!("position fen {}", fen)) }

    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            // 读到 0 字节即 EOF：引擎进程已经退出
            Ok(0) => None,
            Ok(_) => {
                trace!("line::{}", line);
                Some(line.trim().to_string())
            }
            Err(err) => {
                warn!("读取引擎输出失败（引擎可能已退出）: {err}");
                None
            }
        }
    }

    /// 发送 go 命令并收集引擎返回的候选着法（info 行）。
    ///
    /// 引擎每加深一层都会为每个 multipv 编号输出一行 info，这里只保留
    /// 「达到的最大深度」那一组，返回按 multipv 升序排列的 info 行。
    fn bestmove(&mut self, depth: usize, time: usize, multi_pv: usize) -> Vec<String> {
        self.write_command(format!("go depth {} movetime {}", depth, time));
        let mut latest: HashMap<usize, String> = HashMap::new();
        let mut max_depth = 0usize;
        loop {
            // 引擎中途退出时立刻放弃等待。
            // 原实现在读失败时 unwrap panic，若改成返回空串又会在这里无限空转。
            let Some(line) = self.read_line() else {
                warn!("引擎输出中断，放弃等待 bestmove");
                break;
            };
            if line.starts_with("bestmove") {
                break;
            }
            if line.starts_with("info") && line.contains(" pv ") {
                let (d, mpv) = parse_depth_multipv(&line);
                if d >= max_depth {
                    if d > max_depth {
                        max_depth = d;
                        latest.clear();
                    }
                    latest.insert(mpv, line);
                }
            }
        }
        debug!("引擎候选 {} 条（深度 {}）", latest.len(), max_depth);
        let mut lines: Vec<String> = latest.into_values().collect();
        lines.sort_by_key(|l| parse_depth_multipv(l).1);
        // 防御性截断：即使引擎异常多给了候选，也只取请求的数量
        lines.truncate(multi_pv);
        lines
    }

    pub async fn search(&mut self, fen: &str, params: &EngineConfig) -> Option<QueryResult> {
        let mut result = if params.chessdb_enabled {
            // 先查询云库
            chessdb::query(fen, params.chessdb_timeout).await
        } else {
            QueryResult::default()
        };

        match result.state {
            QueryState::Success => Some(result),
            QueryState::InvalidBoard => None,
            QueryState::ServerInternalError | QueryState::NotResult => {
                // 查询云库失败，调用引擎
                self.position(fen);
                // 每次搜索前按当前配置设置强度（Skill Level）与风格（MultiPV），
                // 用户改动立即生效，无需重启引擎。
                self.set_skill_level(params.strength.skill_level());
                self.set_multi_pv(params.style.multi_pv());

                let lines = self.bestmove(params.depth, params.time, params.style.multi_pv());
                let mut candidates = parse_candidates(&lines);
                reorder_candidates(&mut candidates, params.style, chess::fen_to_board(fen));

                if candidates.is_empty() {
                    // 引擎一条候选都没给出（初始化未完成、搜索被中断等）
                    result.state = QueryState::NotResult;
                } else {
                    if let Some(best) = candidates.first().cloned() {
                        result.depth = best.depth;
                        result.score = best.score;
                        result.time = best.time;
                    }
                    result.pvs = candidates.into_iter().map(|c| c.iccs).collect();
                    result.source = SOURCE_ENGINE.to_string();
                }
                Some(result)
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.write_command("quit");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 从 info 行里提取 (搜索深度, multipv 编号)。
fn parse_depth_multipv(line: &str) -> (usize, usize) {
    let mut depth = 0usize;
    let mut mpv = 1usize;
    let mut iter = line.split_whitespace();
    while let Some(key) = iter.next() {
        match key {
            "depth" => depth = iter.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "multipv" => mpv = iter.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            _ => {}
        }
    }
    (depth, mpv)
}

/// 引擎返回的一个候选着法。
#[derive(Debug, Clone)]
struct Candidate {
    score: isize,
    iccs: String,
    depth: usize,
    time: usize,
}

/// 把多条 info 行解析成候选着法列表（保持引擎给出的 multipv 顺序）。
///
/// 只取每个候选的 `pv` 首个着法（候选着法本身），其余 pv token 是后续思考线，
/// 这里不需要。
fn parse_candidates(lines: &[String]) -> Vec<Candidate> {
    lines.iter().filter_map(|line| {
        let mut score = 0isize;
        let mut iccs = String::new();
        let mut depth = 0usize;
        let mut time = 0usize;

        let mut iter = line.split_whitespace();
        while let Some(key) = iter.next() {
            match key {
                "depth" => depth = iter.next()?.parse().ok()?,
                "time" => time = iter.next()?.parse().ok()?,
                "score" => match iter.next()? {
                    "cp" => score = iter.next()?.parse().ok()?,
                    "mate" => {
                        let round: isize = iter.next()?.parse().ok()?;
                        score = if round > 0 { 30000 - round } else { -(30000 + round) };
                    }
                    _ => {}
                },
                "pv" => {
                    iccs = iter.next()?.to_string();
                    break;
                }
                _ => {}
            }
        }

        if iccs.is_empty() { None } else { Some(Candidate { score, iccs, depth, time }) }
    }).collect()
}

/// 按风格重排候选着法。
///
/// - Balanced：保持引擎原有顺序（评分最高的在前），直接返回。
/// - Attack：在「与最优着法评分差距不大」的窗口内，优先进攻性高的着法。
/// - Defense：窗口内优先进攻性低（稳健）的着法。
///
/// 窗口外的候选（明显劣着）保持引擎原顺序、排在窗口内候选之后 —— 无论多"进攻"，
/// 也不该让引擎去送子。
fn reorder_candidates(candidates: &mut Vec<Candidate>, style: Style, board: [[char; 9]; 10]) {
    if candidates.is_empty() || style == Style::Balanced {
        return;
    }

    // 与最优着法评分差距在该范围内都算"接近"，可以按风格微调
    const WINDOW: isize = 120;
    let best_score = candidates[0].score;
    let in_window: Vec<bool> = candidates.iter().map(|c| best_score - c.score <= WINDOW).collect();
    let attacks: Vec<i32> = candidates.iter().map(|c| chess::move_attack_score(board, &c.iccs)).collect();

    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&i| {
        // 窗口内的候选排前面（0），窗口外的排后面（1）
        let rank = usize::from(!in_window[i]);
        // 统一用 i32 作排序键：进攻档取负（进攻性越高键越小、越靠前），
        // 防守档取正（进攻性越低键越小、越靠前）。
        let key = match style {
            Style::Attack => -attacks[i],
            Style::Defense => attacks[i],
            Style::Balanced => 0,
        };
        (rank, key)
    });

    let mut reordered = Vec::with_capacity(candidates.len());
    for i in order {
        reordered.push(candidates[i].clone());
    }
    *candidates = reordered;
}

#[cfg(test)]
mod tests {
    use std::path;

    use tracing::info;
    use tracing::Level;

    use super::*;
    use crate::logger;

    #[test]
    fn test_reorder_by_style() {
        // 红车 a0，黑卒 a4：a0a4 吃卒（进攻性 100），a0a1 不吃子不过河（进攻性 0）
        let mut board = [[' '; 9]; 10];
        board[9][0] = 'R';
        board[5][0] = 'p';
        let mk = |iccs: &str, score: isize| Candidate { score, iccs: iccs.to_string(), depth: 10, time: 100 };

        // 进攻：吃子候选应排前
        let mut c = vec![mk("a0a1", 30), mk("a0a4", 30)];
        reorder_candidates(&mut c, Style::Attack, board);
        assert_eq!(c[0].iccs, "a0a4");

        // 防守：稳健（不吃子）候选应排前
        let mut c = vec![mk("a0a1", 30), mk("a0a4", 30)];
        reorder_candidates(&mut c, Style::Defense, board);
        assert_eq!(c[0].iccs, "a0a1");

        // 均衡：保持引擎原有顺序
        let mut c = vec![mk("a0a1", 30), mk("a0a4", 30)];
        reorder_candidates(&mut c, Style::Balanced, board);
        assert_eq!(c[0].iccs, "a0a1");
    }

    #[tokio::test]
    async fn test_query() {
        logger::init_tracer(Level::TRACE, &std::path::PathBuf::from("."));
        let fen = "rnbakabnr/9/1c5c1/p1p1p1p1p/9/9/P1P1P1P1P/1C2C4/9/RNBAKABNR b";
        let result = chessdb::query(fen, 10).await;
        info!("{:?}", result);
    }

    #[tokio::test]
    async fn test_engine() {
        logger::init_tracer(Level::TRACE, &std::path::PathBuf::from("."));
        let fen = "4k4/9/6r2/9/9/9/9/9/4A4/4K4 w";
        let libs = path::PathBuf::from("/Users/atopx/script/chessboard/libs");
        let mut eng = Engine::new(&libs);
        let cfg = EngineConfig { chessdb_enabled: false, ..Default::default() };
        let records = eng.search(fen, &cfg).await;
        info!("{:?}", records);
    }
}
