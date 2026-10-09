<script setup lang="ts">
import { listen } from "@tauri-apps/api/event";
import { computed, nextTick, onMounted, ref, watch } from "vue";

import { hint } from "../hint";
import { replayFen } from "../replay";
import "../assets/css/chessboard.css";

interface Position {
    piece: string,
    pos: string,
}

interface Changed {
    piece: string,
    from: string,
    to: string,
    camp: string,
}

const startpos: Position[] = [
    { piece: "R", pos: "a0" },
    { piece: "N", pos: "b0" },
    { piece: "B", pos: "c0" },
    { piece: "A", pos: "d0" },
    { piece: "K", pos: "e0" },
    { piece: "A", pos: "f0" },
    { piece: "B", pos: "g0" },
    { piece: "N", pos: "h0" },
    { piece: "R", pos: "i0" },
    { piece: "C", pos: "b2" },
    { piece: "C", pos: "h2" },
    { piece: "P", pos: "a3" },
    { piece: "P", pos: "c3" },
    { piece: "P", pos: "e3" },
    { piece: "P", pos: "g3" },
    { piece: "P", pos: "i3" },
    { piece: "r", pos: "a9" },
    { piece: "n", pos: "b9" },
    { piece: "b", pos: "c9" },
    { piece: "a", pos: "d9" },
    { piece: "k", pos: "e9" },
    { piece: "a", pos: "f9" },
    { piece: "b", pos: "g9" },
    { piece: "n", pos: "h9" },
    { piece: "r", pos: "i9" },
    { piece: "c", pos: "b7" },
    { piece: "c", pos: "h7" },
    { piece: "p", pos: "a6" },
    { piece: "p", pos: "c6" },
    { piece: "p", pos: "e6" },
    { piece: "p", pos: "g6" },
    { piece: "p", pos: "i6" },
];

/* ------------------------------------------------------------------ *
 * 棋面状态：坐标 -> 棋子字符，是界面渲染的唯一数据源。
 *
 * 这样做的意义：原实现是"给 DOM 元素增删 class"，一旦某一步没被处理
 * （例如识别到走子动画中间帧）就会留下永远清不掉的残留棋子。改成整盘
 * 状态驱动后，渲染是幂等的，不可能出现残留。
 * ------------------------------------------------------------------ */
const boardMap = ref<Record<string, string>>({});

/** 走子动画时长（毫秒） */
const ANIM_MS = 170;

/**
 * 正在播放的走子动画。
 *
 * 做法是"格内滑动"而不是独立的悬浮层：先把棋子落到终点格（界面状态立刻
 * 变成权威值），再让终点格的棋子从"偏移到起点"过渡回原位。这样动画期间
 * 不会同时存在两个棋子，也不会因为动画层没收掉而留下多余棋子。
 */
const slide = ref<{ to: string; dx: number; dy: number; phase: "start" | "end" } | null>(null);
let slideTimer: number | undefined;

/* ------------------------------------------------------------------ *
 * "支招"演示动画
 *
 * 引擎推荐一步棋时，把起点格上的**真实棋子**循环滑向目标格 —— 视觉上就是
 * "这枚棋子应该这样走"。
 *
 * 两个要点：
 *  1. 滑到目标格后**不回滑**：一轮结束时直接复位（无过渡）再重新滑过去，
 *     所以看不到"往回走"的反向动作；
 *  2. 动画由 CSS animation 以 infinite 播放，**一直循环**，直到建议变化、
 *     真的走子、或轮到对方为止。
 *
 * 全程只改 transform，位移量通过 CSS 变量传给 keyframes，
 * **不动任何棋面状态**，所以不会多出棋子。
 * ------------------------------------------------------------------ */
const hintAnim = ref<{ from: string; dx: number; dy: number } | null>(null);
let hintToken = 0;

function pieceStyle(pos: string): Record<string, string> | undefined {
    const s = slide.value;
    if (s && s.to === pos) {
        return { transform: s.phase === "start" ? `translate(${s.dx}px, ${s.dy}px)` : "translate(0, 0)" };
    }
    const h = hintAnim.value;
    if (h && h.from === pos) {
        // 位移量以 CSS 变量注入，交给 @keyframes hint-move 使用
        return { "--hx": `${h.dx}px`, "--hy": `${h.dy}px` };
    }
    return undefined;
}

function pieceClasses(pos: string) {
    const s = slide.value;
    const h = hintAnim.value;
    return [
        pieceClass(pos),
        !!(s && s.to === pos) && "move-slide",
        !!(h && h.from === pos) && "hint-move",
    ];
}

function cancelHint() {
    hintToken += 1;
    hintAnim.value = null;
}

/** 计算支招的位移量并交给 CSS 循环播放（不改变棋面状态） */
async function applyHint(from: string, to: string) {
    const token = ++hintToken;
    hintAnim.value = null;

    await nextTick();
    if (token !== hintToken) return;

    const fromEl = document.getElementById(from);
    const toEl = document.getElementById(to);
    if (!fromEl || !toEl) return;

    const a = fromEl.getBoundingClientRect();
    const b = toEl.getBoundingClientRect();
    const dx = b.left - a.left;
    const dy = b.top - a.top;
    if (dx === 0 && dy === 0) return;

    hintAnim.value = { from, dx, dy };
}

watch(hint, (h) => {
    if (!h) {
        cancelHint();
        return;
    }
    void applyHint(h.from, h.to);
});

function clearSelects() {
    document.querySelectorAll(".b-select").forEach(element => {
        element.classList.remove("b-select");
    });
}

/** 模板里按坐标取棋子的 class —— 整盘换新对象，天然幂等 */
function pieceClass(pos: string): string {
    const piece = boardMap.value[pos];
    return piece && piece !== " " ? `piece-${piece}` : "";
}

/** 用整盘数据替换当前棋面 */
function setPiecesOnBoard(pieces: Position[]) {
    clearSelects();
    slide.value = null; // 整盘刷新时作废进行中的走子动画

    const next: Record<string, string> = {};
    for (const item of pieces) {
        next[item.pos] = item.piece;
    }

    // 支招演示只在"起点格的棋子变了"时才失效。
    // 否则每次无关的整盘刷新都会把正在循环的动画打断重启，看着会一顿一顿的。
    const h = hintAnim.value;
    if (h && boardMap.value[h.from] !== next[h.from]) {
        cancelHint();
    }

    boardMap.value = next;
}

/**
 * 播放一步走子动画。
 *
 * 注意：这里**不修改棋面状态**。
 * 棋面状态只由后端推送的整盘 position 事件决定（后端在每次发 move 之前都会先发
 * 一次 position），move 仅用于计算动画的起止位移。这样 move 即使偶有偏差，
 * 也只会影响一次动画，绝不可能在棋盘上留下多余棋子。
 */
async function playMove(change: Changed) {
    clearSelects();
    cancelHint(); // 真的走子了，支招演示让位

    const fromId = change?.from;
    const toId = change?.to;
    if (!fromId || !toId) return;

    const fromEl = document.getElementById(fromId);
    const toEl = document.getElementById(toId);
    if (!fromEl || !toEl) return;

    const a = fromEl.getBoundingClientRect();
    const b = toEl.getBoundingClientRect();
    const dx = a.left - b.left;
    const dy = a.top - b.top;
    if (dx === 0 && dy === 0) return;

    if (slideTimer) window.clearTimeout(slideTimer);
    slide.value = { to: toId, dx, dy, phase: "start" };

    await nextTick();
    // 先偏移到起点画一帧，再回到原位以触发 CSS 过渡
    requestAnimationFrame(() => {
        if (slide.value) slide.value = { ...slide.value, phase: "end" };
    });
    slideTimer = window.setTimeout(() => {
        slide.value = null;
    }, ANIM_MS + 80);
}

onMounted(() => {
    setPiecesOnBoard(startpos);
});

const mirror = ref(false);

const wrappedItems = computed(() => {
    if (mirror.value) {
        return [{ id: 'i0' }, { id: 'h0' }, { id: 'g0' }, { id: 'f0' }, { id: 'e0' }, { id: 'd0' }, { id: 'c0' }, { id: 'b0' }, { id: 'a0' }, { id: 'i1' }, { id: 'h1' }, { id: 'g1' }, { id: 'f1' }, { id: 'e1' }, { id: 'd1' }, { id: 'c1' }, { id: 'b1' }, { id: 'a1' }, { id: 'i2' }, { id: 'h2' }, { id: 'g2' }, { id: 'f2' }, { id: 'e2' }, { id: 'd2' }, { id: 'c2' }, { id: 'b2' }, { id: 'a2' }, { id: 'i3' }, { id: 'h3' }, { id: 'g3' }, { id: 'f3' }, { id: 'e3' }, { id: 'd3' }, { id: 'c3' }, { id: 'b3' }, { id: 'a3' }, { id: 'i4' }, { id: 'h4' }, { id: 'g4' }, { id: 'f4' }, { id: 'e4' }, { id: 'd4' }, { id: 'c4' }, { id: 'b4' }, { id: 'a4' }, { id: 'i5' }, { id: 'h5' }, { id: 'g5' }, { id: 'f5' }, { id: 'e5' }, { id: 'd5' }, { id: 'c5' }, { id: 'b5' }, { id: 'a5' }, { id: 'i6' }, { id: 'h6' }, { id: 'g6' }, { id: 'f6' }, { id: 'e6' }, { id: 'd6' }, { id: 'c6' }, { id: 'b6' }, { id: 'a6' }, { id: 'i7' }, { id: 'h7' }, { id: 'g7' }, { id: 'f7' }, { id: 'e7' }, { id: 'd7' }, { id: 'c7' }, { id: 'b7' }, { id: 'a7' }, { id: 'i8' }, { id: 'h8' }, { id: 'g8' }, { id: 'f8' }, { id: 'e8' }, { id: 'd8' }, { id: 'c8' }, { id: 'b8' }, { id: 'a8' }, { id: 'i9' }, { id: 'h9' }, { id: 'g9' }, { id: 'f9' }, { id: 'e9' }, { id: 'd9' }, { id: 'c9' }, { id: 'b9' }, { id: 'a9' }];

    } else {
        return [{ id: 'a9' }, { id: 'b9' }, { id: 'c9' }, { id: 'd9' }, { id: 'e9' }, { id: 'f9' }, { id: 'g9' }, { id: 'h9' }, { id: 'i9' }, { id: 'a8' }, { id: 'b8' }, { id: 'c8' }, { id: 'd8' }, { id: 'e8' }, { id: 'f8' }, { id: 'g8' }, { id: 'h8' }, { id: 'i8' }, { id: 'a7' }, { id: 'b7' }, { id: 'c7' }, { id: 'd7' }, { id: 'e7' }, { id: 'f7' }, { id: 'g7' }, { id: 'h7' }, { id: 'i7' }, { id: 'a6' }, { id: 'b6' }, { id: 'c6' }, { id: 'd6' }, { id: 'e6' }, { id: 'f6' }, { id: 'g6' }, { id: 'h6' }, { id: 'i6' }, { id: 'a5' }, { id: 'b5' }, { id: 'c5' }, { id: 'd5' }, { id: 'e5' }, { id: 'f5' }, { id: 'g5' }, { id: 'h5' }, { id: 'i5' }, { id: 'a4' }, { id: 'b4' }, { id: 'c4' }, { id: 'd4' }, { id: 'e4' }, { id: 'f4' }, { id: 'g4' }, { id: 'h4' }, { id: 'i4' }, { id: 'a3' }, { id: 'b3' }, { id: 'c3' }, { id: 'd3' }, { id: 'e3' }, { id: 'f3' }, { id: 'g3' }, { id: 'h3' }, { id: 'i3' }, { id: 'a2' }, { id: 'b2' }, { id: 'c2' }, { id: 'd2' }, { id: 'e2' }, { id: 'f2' }, { id: 'g2' }, { id: 'h2' }, { id: 'i2' }, { id: 'a1' }, { id: 'b1' }, { id: 'c1' }, { id: 'd1' }, { id: 'e1' }, { id: 'f1' }, { id: 'g1' }, { id: 'h1' }, { id: 'i1' }, { id: 'a0' }, { id: 'b0' }, { id: 'c0' }, { id: 'd0' }, { id: 'e0' }, { id: 'f0' }, { id: 'g0' }, { id: 'h0' }, { id: 'i0' }];
    }
});

listen('mirror', async (event) => {
    mirror.value = event.payload as boolean;
})

/* ------------------------------------------------------------------ *
 * 棋谱回放
 *
 * 从棋谱里点某一步时，后端记录的 FEN 会被送到这里还原成棋盘。
 * 回放期间不接收实时盘面与走子动画，否则历史画面立刻被冲掉。
 * ------------------------------------------------------------------ */

/** 把 FEN 还原成棋子列表。
 *
 *  FEN 第一段是 10 行（第 9 行 → 第 0 行），每行 9 列；数字表示连续空格，
 *  字母表示棋子。坐标命名与后端的 BOARD_MAP 保持一致：第 9 行是 a9..i9。 */
function fenToPositions(fen: string): Position[] {
    const rows = fen.split(" ")[0].split("/");
    const files = "abcdefghi";
    const pieces: Position[] = [];

    rows.forEach((row, rowIndex) => {
        let col = 0;
        for (const ch of row) {
            if (ch >= "1" && ch <= "9") {
                col += Number(ch);
                continue;
            }
            if (col > 8) break;
            pieces.push({ piece: ch, pos: `${files[col]}${9 - rowIndex}` });
            col += 1;
        }
    });

    return pieces;
}

watch(replayFen, (fen) => {
    if (!fen) return;
    setPiecesOnBoard(fenToPositions(fen));
});

listen('position', async (event) => {
    // 回放中不覆盖棋盘
    if (replayFen.value) return;
    let pos = event.payload as Position[];
    setPiecesOnBoard(pos);
})

listen('move', async (event) => {
    // 回放中不播实时走子动画
    if (replayFen.value) return;
    let change = event.payload as Changed;
    await playMove(change);
});

</script>

<template>
    <div id="chessboard">
        <div v-for="(item, _) in wrappedItems" :key="item.id" :id="item.id" class="piece-wrap"><span
                class="piece" :class="pieceClasses(item.id)" :style="pieceStyle(item.id)"></span></div>
    </div>
</template>
