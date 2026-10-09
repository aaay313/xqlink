import { ref } from "vue";

/**
 * 当前推荐的着法，用于棋盘上的"支招演示动画"。
 *
 * Analyse.vue 在轮到我方并拿到引擎建议时写入；Chessboard.vue 监听它，
 * 对起点格上的**真实棋子**施加位移，让它循环滑向目标格 —— 视觉上就是
 * "这枚棋子应该这样走"。全程不修改任何棋面状态，因此不会多出棋子。
 */
export const hint = ref<{ from: string; to: string; seq: number } | null>(null);

let seq = 0;

/**
 * 设置支招着法。
 *
 * 着法与当前相同时直接返回：动画本身是无限循环的，重复写入只会白白打断它
 * （识别抖动时引擎会连着推好几次同样的建议）。
 */
export function setHint(from: string, to: string) {
    const cur = hint.value;
    if (cur && cur.from === from && cur.to === to) return;
    seq += 1;
    hint.value = { from, to, seq };
}

/** 清除支招（例如轮到对方、或建议失效） */
export function clearHint() {
    if (hint.value !== null) hint.value = null;
}
