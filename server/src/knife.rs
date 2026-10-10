//! 飞刀开局库：内置若干中国象棋「邪门布局」的飞刀着法。
//!
//! 飞刀布局的核心价值在于「出奇制胜」——不走流行开局，而是走冷门甚至看似
//! 亏损的着法，诱使对手按惯性应手、踏入陷阱。
//!
//! 飞刀分两类：
//! - **先手飞刀**（红方先手套路）：铁滑车、敢死炮、叠炮（多步），九尾龟、御驾亲征、沉宫马（单步）。
//! - **后手飞刀**（黑方应对红方第一步）：瞎眼狗（红方进三/七兵，黑方弃同路卒抢先）。
//!
//! 所有着法均为 ICCS 坐标，已用引擎验证合法。

/// 飞刀所属方：红方先手 / 黑方后手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Red,
    Black,
}

/// 一个飞刀布局。
#[derive(Debug, Clone, Copy)]
pub struct Knife {
    /// 飞刀名称（用于界面与日志展示）
    pub name: &'static str,
    /// 先手飞刀还是后手飞刀
    pub side: Side,
    /// 后手飞刀需要匹配的「对方第一步」ICCS；先手飞刀为 None
    pub trigger: Option<&'static str>,
    /// 我方飞刀着法序列（ICCS 坐标）。先手飞刀是红方连续套路，后手飞刀是单步应手。
    pub moves: &'static [&'static str],
}

/// 内置飞刀库。
pub const KNIVES: &[Knife] = &[
    // ---------- 先手飞刀（红方连续套路） ----------
    Knife { name: "铁滑车", side: Side::Red, trigger: None, moves: &["i0i1", "a0a1"] },
    Knife { name: "敢死炮", side: Side::Red, trigger: None, moves: &["h2h4", "h4b4"] },
    Knife { name: "叠炮", side: Side::Red, trigger: None, moves: &["h2h3", "b2h2"] },
    Knife { name: "九尾龟", side: Side::Red, trigger: None, moves: &["i3i4"] },
    Knife { name: "御驾亲征", side: Side::Red, trigger: None, moves: &["e0e1"] },
    Knife { name: "沉宫马", side: Side::Red, trigger: None, moves: &["g0i2"] },
    // ---------- 后手飞刀（黑方应对红方第一步） ----------
    Knife { name: "瞎眼狗", side: Side::Black, trigger: Some("g3g4"), moves: &["g6g5"] },
    Knife { name: "瞎眼狗", side: Side::Black, trigger: Some("c3c4"), moves: &["c6c5"] },
];

/// 按「红方已走序列」匹配先手飞刀，返回 (命中的飞刀, 下一步着法)。
///
/// `played` 是红方本局已走的着法序列（ICCS）。当它正好是某个飞刀套路的
/// 前缀、且还没走完时命中，返回该飞刀和「下一步」着法。
///
/// `name` 为 `"random"` 时在所有命中项里随机挑，否则按名称精确匹配。
pub fn match_red_knife(name: &str, played: &[String]) -> Option<(&'static Knife, &'static str)> {
    let matched: Vec<(&'static Knife, &'static str)> = KNIVES
        .iter()
        .filter(|k| k.side == Side::Red)
        .filter_map(|k| {
            if played.len() >= k.moves.len() {
                return None;
            }
            let is_prefix = played.iter().zip(k.moves.iter()).all(|(p, m)| p.as_str() == *m);
            if is_prefix {
                Some((k, k.moves[played.len()]))
            } else {
                None
            }
        })
        .collect();

    if matched.is_empty() {
        return None;
    }

    if name == "random" {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Some(matched[(nanos as usize) % matched.len()])
    } else {
        matched.into_iter().find(|(k, _)| k.name == name)
    }
}

/// 按「对方第一步」匹配后手飞刀，返回 (命中的飞刀, 应手着法)。
///
/// `trigger` 是对方（红方）第一步的 ICCS。`name` 为 `"random"` 时随机挑。
pub fn match_black_knife(name: &str, trigger: &str) -> Option<(&'static Knife, &'static str)> {
    let matched: Vec<(&'static Knife, &'static str)> = KNIVES
        .iter()
        .filter(|k| k.side == Side::Black)
        .filter(|k| k.trigger == Some(trigger))
        .map(|k| (k, k.moves[0]))
        .collect();

    if matched.is_empty() {
        return None;
    }

    if name == "random" {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Some(matched[(nanos as usize) % matched.len()])
    } else {
        matched.into_iter().find(|(k, _)| k.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_red_knife_first_step() {
        // 开局第一步，铁滑车命中走 i0i1
        let (k, mv) = match_red_knife("铁滑车", &[]).unwrap();
        assert_eq!(k.name, "铁滑车");
        assert_eq!(mv, "i0i1");
    }

    #[test]
    fn test_match_red_knife_second_step() {
        // 红方已走 i0i1（铁滑车第一步），下一步走 a0a1
        let (_, mv) = match_red_knife("铁滑车", &["i0i1".to_string()]).unwrap();
        assert_eq!(mv, "a0a1");
    }

    #[test]
    fn test_match_red_knife_prefix_mismatch() {
        // 红方第一步走了别的（不是铁滑车套路），不命中
        assert!(match_red_knife("铁滑车", &["h2e2".to_string()]).is_none());
    }

    #[test]
    fn test_match_black_knife() {
        assert_eq!(match_black_knife("瞎眼狗", "g3g4").unwrap().1, "g6g5");
        assert_eq!(match_black_knife("瞎眼狗", "c3c4").unwrap().1, "c6c5");
        assert!(match_black_knife("瞎眼狗", "h2e2").is_none());
    }

    #[test]
    fn test_random() {
        assert!(match_red_knife("random", &[]).is_some());
    }
}
