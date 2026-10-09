pub mod chessdb;
use std::fmt::Display;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
mod command;

use tracing::debug;
use tracing::trace;
use tracing::warn;

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

#[derive(Debug, serde::Serialize, Clone, serde::Deserialize, Copy)]
pub struct EngineConfig {
    pub depth: usize,
    pub time: usize,
    pub threads: usize,
    pub hash: usize,
    pub show_wdl: bool,
    pub chessdb_enabled: bool,
    pub chessdb_timeout: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { depth: 20, time: 5000, threads: 4, hash: 64, show_wdl: false, chessdb_enabled: true, chessdb_timeout: 5 }
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

    fn parse_line(&self, line: String, result: &mut QueryResult) {
        let mut iter = line.split_whitespace();
        result.source = SOURCE_ENGINE.to_string();
        loop {
            if let Some(key) = iter.next() {
                match key {
                    "depth" => {
                        result.depth = iter.next().unwrap().parse().unwrap();
                    }
                    "time" => {
                        result.time = iter.next().unwrap().parse().unwrap();
                    }
                    "score" => match iter.next().unwrap() {
                        "cp" => {
                            result.score = iter.next().unwrap().parse().unwrap();
                        }
                        "mate" => {
                            let round: isize = iter.next().unwrap().parse().unwrap();
                            result.score = if round > 0 { 30000 - round } else { -(30000 + round) };
                        }
                        _ => {}
                    },
                    "pv" => loop {
                        if let Some(pv) = iter.next() {
                            result.pvs.push(pv.to_string());
                            continue;
                        }
                        break;
                    },
                    _ => {}
                }
                continue;
            }
            break;
        }
    }

    fn bestmove(&mut self, depth: usize, time: usize) -> String {
        self.write_command(format!("go depth {} movetime {}", depth, time));
        let mut pre_line = String::new();
        loop {
            // 引擎中途退出时立刻放弃等待。
            // 原实现在读失败时 unwrap panic，若改成返回空串又会在这里无限空转。
            let Some(line) = self.read_line() else {
                warn!("引擎输出中断，放弃等待 bestmove");
                break;
            };
            if line.starts_with("bestmove") {
                trace!("{}", pre_line);
                break;
            }
            pre_line = line;
        }
        pre_line
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
                // 查询云库失败调用引擎
                self.position(fen);
                let best_line = self.bestmove(params.depth, params.time);
                self.parse_line(best_line, &mut result);
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

#[cfg(test)]
mod tests {
    use std::path;

    use tracing::info;
    use tracing::Level;

    use super::*;
    use crate::logger;

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
