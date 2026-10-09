<script setup lang="ts">
import { listen } from '@tauri-apps/api/event';
import { LogInst, NCard, NDivider, NFlex, NLog, NProgress, NTag, NText } from 'naive-ui';
import { computed, ref } from 'vue';

import { clearHint, setHint } from '../hint';
import { listening } from '../session';

interface Analyse {
    depth: number,   // 深度
    score: number,   // 得分
    time: number,    // 时间
    pvs: string[],   // 思考(iccs)
    moves: string[], // 思考(chinese)
    state: string,   // 状态
    source: string,  // 来源
    our_turn: boolean, // 是否轮到我方行棋
}

const logs = ref<string[]>([])
const best = ref({
    move: "----",
    depth: 0,
    score: 0,
    time: 0,
})

/** 把引擎评分换算成胜率（%）。
 *
 *  用 Elo 期望胜率公式近似：W = 1 / (1 + 10^(-分/400))。
 *  象棋里 100 分约等于多一个兵，400 分差对应约九成胜率，量级合理；
 *  绝杀类分数会被夹到 0/100 附近，不会出现离谱数值。 */
const winRate = computed(() => {
    const clamped = Math.max(-4000, Math.min(4000, best.value.score));
    return Math.round((1 / (1 + Math.pow(10, -clamped / 400))) * 100);
});

/** 胜率配色：明显优势偏绿、明显劣势偏红、均势用主色 */
const winRateColor = computed(() => {
    const r = winRate.value;
    if (r >= 60) return "#18a058";
    if (r <= 40) return "#d03050";
    return "#2080f0";
});

/** 是否已经算到杀棋。
 *  Pikafish 用接近 ±30000 的分数表示杀棋，正常局面不会到这么高。 */
const mateState = computed(() => {
    const score = best.value.score;
    if (score >= 29000) return "win";
    if (score <= -29000) return "lose";
    return "";
});

/** 正分显示带 + 号，一眼看出优劣 */
function formatScore(score: number): string {
    return score > 0 ? `+${score}` : String(score);
}

/** 是否轮到我方行棋。false 时说明正等对方落子，不显示建议。 */
const ourTurn = ref(true);
const waiting = computed(() => !ourTurn.value);

const logInstRef = ref<LogInst | null>(null)

/** 识别视角：true 表示黑方在屏幕下方（即你执黑）。
 *  显示出来便于一眼判断识别方向有没有反。 */
const mirror = ref(false)

listen('mirror', async (event) => {
    mirror.value = event.payload as boolean
})

function clearSelects() {
    document.querySelectorAll(".b-select").forEach(element => {
        element.classList.remove("b-select")
    })
}

function resetBest() {
    best.value = { move: "----", depth: 0, score: 0, time: 0 };
    clearSelects();
    clearHint();
}

// 后端观测到行棋方变化时通知：false 表示我方刚走完、轮到对方
listen('turn', async (event) => {
    ourTurn.value = event.payload as boolean;
    if (!ourTurn.value) {
        // 立刻清空建议与棋盘高亮，避免"对方还没走就显示我方怎么走"
        resetBest();
    }
})

listen('analyse', async (event) => {
    let data = event.payload as Analyse;

    // 只有轮到我方行棋时才展示建议
    if (!data.our_turn) {
        clearSelects();
        clearHint();
        return;
    }

    // 后端既然推来"轮到我方"的分析结果，就以它为准复位轮次状态。
    // 这样即使 turn 事件因故丢失，界面也不会卡在"等待对方落子"。
    ourTurn.value = true;

    let mvs = data.moves.join(" ");
    logs.value.push(`<${data.source}> ${mvs}`)
    // 滚动到最新
    if (logs.value.length > 18) {
        if (logs.value.length > 128) {
            // 清理很早以前的数据
            logs.value.shift();
        }
        logInstRef.value?.scrollTo({ position: 'bottom', silent: true })
    }
    best.value.move = data.moves[0];
    best.value.depth = data.depth;
    best.value.score = data.score;
    best.value.time = data.time;

    // 高亮建议着法的起止格，并让棋子做一次"支招"演示动画
    clearSelects();
    let pv = data.pvs[0];
    if (pv && pv.length >= 4) {
        let from = pv.substring(0, 2);
        let to = pv.substring(2, 4)
        document.getElementById(from)?.classList.add("b-select");
        document.getElementById(to)?.classList.add("b-select");
        setHint(from, to);
    } else {
        clearHint();
    }
})

</script>

<template>
    <n-card :bordered="false" class="textlog">
        <template #header>
            <n-flex align="center" justify="space-between">
                <span>局面分析</span>
                <n-tag size="tiny" :bordered="false" :type="listening ? 'info' : 'default'">
                    {{ listening ? (mirror ? "黑方视角" : "红方视角") : "未监听" }}
                </n-tag>
            </n-flex>
        </template>
        <n-text v-if="!listening" type="info" class="analyse-wait">
            点上方「启」开始分析
        </n-text>
        <n-text v-else-if="waiting" type="info" class="analyse-wait" strong>
            等待对方落子…
        </n-text>
        <template v-else>
            <n-flex justify="space-between" align="end">
                <n-text type="info" class="analyse-title" strong>
                    {{ best.move }}
                </n-text>
                <n-text type="error">{{ formatScore(best.score) }}</n-text>
            </n-flex>
            <n-flex justify="space-between" align="center">
                <n-space :size="6" align="center">
                    <n-text class="analyse-meta" :style="{ color: winRateColor, fontWeight: 500 }">
                        胜率 {{ winRate }}%
                    </n-text>
                    <n-tag v-if="mateState === 'win'" type="success" size="tiny" :bordered="false" strong>绝杀</n-tag>
                    <n-tag v-else-if="mateState === 'lose'" type="error" size="tiny" :bordered="false" strong>
                        被绝杀
                    </n-tag>
                </n-space>
                <n-text depth="3" class="analyse-meta">深度 {{ best.depth }} · {{ best.time }}ms</n-text>
            </n-flex>
            <n-progress
                class="win-bar"
                type="line"
                :percentage="winRate"
                :height="6"
                :show-indicator="false"
                :color="winRateColor"
            />
        </template>
        <n-divider />
        <n-log class="analyse-log" :rows=17 ref="logInst" :line-height="1.5" :lines="logs" :font-size="10" />
    </n-card>
</template>

<style scoped>
.analyse-title {
    font-size: x-large;
}

.analyse-wait {
    font-size: medium;
    opacity: 0.75;
}

/* 次要信息行：胜率 / 深度 / 耗时 */
.analyse-meta {
    font-size: 12px;
}

/* 胜率进度条：比纯文字更直观 */
.win-bar {
    margin-top: 2px;
}

.textlog {
    width: 260px;
    height: 440px;
    left: 400px;
    top: 0px;
    border-radius: 10px;
    /* 深色主题下用较重的投影把面板从背景里托起来 */
    box-shadow: 0 4px 18px rgba(0, 0, 0, 0.55);
}
</style>
