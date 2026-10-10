//! 飞刀开局库：内置若干中国象棋「邪门布局」的飞刀着法。
//!
//! 飞刀布局的核心价值在于「出奇制胜」——不走流行开局，而是走冷门甚至看似
//! 亏损的着法，诱使对手按惯性应手、踏入陷阱。
//!
//! 飞刀分两类：
//! - **先手飞刀**（红方先手第一步）：铁滑车、敢死炮、叠炮、九尾龟、御驾亲征、沉宫马。
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
    /// 飞刀名称（用于界面下拉与日志展示）
    pub name: &'static str,
    /// 中文着法（如「车一进一」）
    pub chinese: &'static str,
    /// 先手飞刀还是后手飞刀
    pub side: Side,
    /// 后手飞刀需要匹配的「对方第一步」ICCS；先手飞刀为 None
    pub trigger: Option<&'static str>,
    /// 我方飞刀着法（ICCS 坐标）
    pub move_: &'static str,
}

/// 内置飞刀库。
pub const KNIVES: &[Knife] = &[
    // ---------- 先手飞刀（红方第一步） ----------
    Knife { name: "铁滑车", chinese: "车一进一", side: Side::Red, trigger: None, move_: "i0i1" },
    Knife { name: "敢死炮", chinese: "炮二进二", side: Side::Red, trigger: None, move_: "h2h4" },
    Knife { name: "叠炮", chinese: "炮二进一", side: Side::Red, trigger: None, move_: "h2h3" },
    Knife { name: "九尾龟", chinese: "兵一进一", side: Side::Red, trigger: None, move_: "i3i4" },
    Knife { name: "御驾亲征", chinese: "帅五进一", side: Side::Red, trigger: None, move_: "e0e1" },
    Knife { name: "沉宫马", chinese: "相三进一", side: Side::Red, trigger: None, move_: "g0i2" },
    // ---------- 后手飞刀（黑方应对红方第一步） ----------
    Knife { name: "瞎眼狗", chinese: "卒7进1", side: Side::Black, trigger: Some("g3g4"), move_: "g6g5" },
    Knife { name: "瞎眼狗", chinese: "卒3进1", side: Side::Black, trigger: Some("c3c4"), move_: "c6c5" },
];

/// 按「所属方 + 触发着法」筛选飞刀。
///
/// - `side`：先手（红方）还是后手（黑方）。
/// - `trigger`：后手飞刀需要匹配的对方第一步；先手飞刀传 `None`。
/// - `name`：`"random"` 时在匹配项中随机挑一个，否则按名称精确匹配。
pub fn pick_knife(name: &str, side: Side, trigger: Option<&str>) -> Option<&'static Knife> {
    let matched: Vec<&Knife> = KNIVES
        .iter()
        .filter(|k| k.side == side)
        .filter(|k| match (k.trigger, trigger) {
            (Some(t), Some(a)) => t == a,
            (None, None) => true,
            _ => false,
        })
        .collect();

    if matched.is_empty() {
        return None;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);

    if name == "random" {
        Some(matched[(nanos as usize) % matched.len()])
    } else {
        matched.into_iter().find(|k| k.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_red_knife() {
        assert_eq!(pick_knife("铁滑车", Side::Red, None).unwrap().move_, "i0i1");
        assert_eq!(pick_knife("敢死炮", Side::Red, None).unwrap().move_, "h2h4");
        assert!(pick_knife("不存在的飞刀", Side::Red, None).is_none());
    }

    #[test]
    fn test_pick_black_knife_by_trigger() {
        // 红方兵三进一 → 黑方卒7进1
        assert_eq!(pick_knife("瞎眼狗", Side::Black, Some("g3g4")).unwrap().move_, "g6g5");
        // 红方兵七进一 → 黑方卒3进1
        assert_eq!(pick_knife("瞎眼狗", Side::Black, Some("c3c4")).unwrap().move_, "c6c5");
        // 对方没进兵，瞎眼狗不触发
        assert!(pick_knife("瞎眼狗", Side::Black, Some("h2e2")).is_none());
    }

    #[test]
    fn test_pick_random() {
        assert!(pick_knife("random", Side::Red, None).is_some());
    }
}
