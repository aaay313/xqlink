import { ref } from "vue";

/**
 * 棋谱回放中的局面（FEN）。
 *
 * 非空表示正在回放：棋盘显示这一刻的历史局面，并且不再跟随实时识别
 * （否则一有新盘面就把回放画面冲掉了）。置空即可返回实时。
 */
export const replayFen = ref<string | null>(null);
