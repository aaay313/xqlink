use std::time::Duration;

use super::QueryResult;
use super::QueryState;

/// ChessDB 云库 —— 目前规模最大的中国象棋局面库（上亿局面，由分布式引擎自对弈生成）。
///
/// 接口选择：
/// - `action=queryall`：返回该局面**所有**已知着法，带 `rank`（走法评级）、
///   `note`（`!` / `*` 等标记）与 `winrate`（胜率）。这是选着法该用的接口。
/// - `action=querypv`：只返回一条主变，但带 `depth`。原实现只用这个，
///   等于把数据库降级成"一条推荐"。
///
/// 现在两个都取：queryall 决定候选着法的排序，querypv 补 depth 供界面显示。
const URL: &str = "http://www.chessdb.cn/chessdb.php";
const REFER: &str = "https://www.chessdb.cn/query/";
const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/60.0.3112.113 Safari/537.36";
const SOURCE_CHESSDB: &str = "云库";

/// 云库返回的一条着法。
#[derive(Debug, Clone, Default)]
pub struct BookMove {
    /// ICCS 着法，如 `h2e2`
    pub iccs: String,
    /// 分值（云库口径，正数对红方有利）
    pub score: i32,
    /// 走法评级：越高越可信（2 以上通常带 `!`）
    pub rank: i32,
    /// 云库备注，如 `! (44-02)`
    pub note: String,
    /// 胜率（百分比）；0 表示未给出
    pub winrate: f32,
}

async fn get(action: &str, fen: &str, timeout: u64) -> Option<String> {
    let resp = reqwest::Client::new()
        .get(URL)
        .header("User-Agent", AGENT)
        .header("Referer", REFER)
        .query(&[("action", action), ("board", fen)])
        .timeout(Duration::from_secs(timeout))
        .send()
        .await;
    match resp {
        Ok(r) => match r.text().await {
            Ok(t) => Some(t.trim_end_matches('\0').trim().to_string()),
            Err(e) => {
                tracing::warn!("云库读取响应失败: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("云库请求失败: {e}");
            None
        }
    }
}

/// 解析 `queryall` 的返回：`move:c3c4,score:1,rank:2,note:! (44-02),winrate:50.08|...`
fn parse_all(text: &str) -> Vec<BookMove> {
    let mut out = Vec::new();
    for part in text.split('|') {
        let mut mv = BookMove::default();
        for field in part.split(',') {
            let field = field.trim();
            if let Some(v) = field.strip_prefix("move:") {
                mv.iccs = v.trim().to_string();
            } else if let Some(v) = field.strip_prefix("score:") {
                mv.score = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = field.strip_prefix("rank:") {
                mv.rank = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = field.strip_prefix("note:") {
                mv.note = v.trim().to_string();
            } else if let Some(v) = field.strip_prefix("winrate:") {
                mv.winrate = v.trim().parse().unwrap_or(0.0);
            }
        }
        if !mv.iccs.is_empty() {
            out.push(mv);
        }
    }
    // 评级优先，其次分值绝对值 —— 云库里 rank 才是"这步是否可信"的主信号
    out.sort_by(|a, b| b.rank.cmp(&a.rank).then(b.score.abs().cmp(&a.score.abs())));
    out
}

pub async fn query(fen: &str, timeout: u64) -> QueryResult {
    let mut records = QueryResult::default();

    let Some(all) = get("queryall", fen, timeout).await else {
        records.state = QueryState::ServerInternalError;
        return records;
    };
    match all.as_str() {
        "" | "unknown" => {
            records.state = QueryState::NotResult;
            return records;
        }
        "invalid board" | "checkmate" | "stalemate" => {
            records.state = QueryState::InvalidBoard;
            return records;
        }
        _ => {}
    }

    let moves = parse_all(&all);
    if moves.is_empty() {
        records.state = QueryState::NotResult;
        return records;
    }

    records.pvs = moves.iter().take(5).map(|m| m.iccs.clone()).collect();
    records.score = moves[0].score as isize;
    records.state = QueryState::Success;
    records.source = SOURCE_CHESSDB.to_string();

    // 补 depth：queryall 不返回搜索深度，界面要显示它，多花一次请求换取准确信息。
    if let Some(pv) = get("querypv", fen, timeout).await
        && !pv.is_empty()
        && pv != "unknown"
    {
        for pair in pv.split(',') {
            if let Some(v) = pair.strip_prefix("depth:") {
                records.depth = v.trim().parse().unwrap_or(0);
            }
        }
    }

    tracing::debug!(
        "云库命中 {} 条候选，首选 {}（rank {} note {} 胜率 {:.2}）",
        moves.len(),
        moves[0].iccs,
        moves[0].rank,
        moves[0].note,
        moves[0].winrate
    );
    records
}

/// 只取候选着法（供需要完整排序信息的调用方使用）。
pub async fn query_moves(fen: &str, timeout: u64) -> Vec<BookMove> {
    match get("queryall", fen, timeout).await {
        Some(text) if text != "unknown" && !text.is_empty() => parse_all(&text),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_orders_by_rank() {
        let text = "move:c3c4,score:1,rank:2,note:! (44-02),winrate:50.08|\
                    move:b0c2,score:0,rank:1,note:* (43-01),winrate:50.00|\
                    move:h2e2,score:-30,rank:1,note:* (45-02),winrate:49.00";
        let moves = parse_all(text);
        assert_eq!(moves.len(), 3);
        assert_eq!(moves[0].iccs, "c3c4");
        assert_eq!(moves[0].rank, 2);
        assert!((moves[0].winrate - 50.08).abs() < 0.01);
        assert_eq!(moves[0].note, "! (44-02)");
    }

    #[test]
    fn parse_all_ignores_garbage() {
        assert!(parse_all("unknown").is_empty());
        assert!(parse_all("").is_empty());
        assert!(parse_all("score:1,rank:2").is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_query() {
        let result = query("2baka3/9/n3b4/r1p1nr2p/4R4/2PR5/4P3P/4B1N2/4A4/2B1KA3 w", 10).await;
        println!("{:?}", result);
    }
}
