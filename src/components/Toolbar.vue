<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { onMounted, onUnmounted, ref, h, computed } from "vue";

import { listening } from "../session";
import { replayFen } from "../replay";
import { useDialog } from "naive-ui";
import {
    NButton,
    NCard,
    NFlex,
    NForm,
    NFormItem,
    NInputNumber,
    NSelect,
    NDrawer,
    NDrawerContent,
    NSpace,
    NTooltip,
    NDivider,
    NEmpty,
    NScrollbar,
    NTag,
    NInput,
    NSwitch,
} from "naive-ui";

const options = [
    {
        label: "连线分析",
        value: "LinkAnaly",
        disabled: false,
    },
    {
        label: "连线对战",
        value: "LinkPlay",
        disabled: true,
    },
    {
        label: "人机对弈",
        value: "Offline",
        disabled: true,
    },
];

interface EngineConfig {
    depth: number;
    time: number;
    threads: number;
    hash: number;
    // show_wdl: number;
    chessdb_enabled: boolean;
    chessdb_timeout: number;
}

const mode = ref(options[0].value);

const config = ref<EngineConfig>({
    depth: 0,
    time: 0,
    threads: 0,
    hash: 0,
    chessdb_enabled: false,
    chessdb_timeout: 0,
});

const showEngineConfig = ref(false);
const isEngineRunning = ref(false);
/** 正在监听的目标窗口标题，用于界面提示 */
const listeningTarget = ref("");

/* ---------------------------- 棋谱 ---------------------------- */

interface MoveRecord {
    /** 中文着法，如"炮二平五" */
    chinese: string;
    /** 是否我方走出 */
    ours: boolean;
    /** 走出这步之前引擎对局面的评分（我方视角，正数对我方有利） */
    score: number;
    /** 这手走完之后的局面，点击该行时用它把棋盘还原到那一刻 */
    fen: string;
}

const showHistory = ref(false);
const history = ref<MoveRecord[]>([]);

/** 正在回放第几手（-1 表示没有在回放） */
const replayIndex = computed(() => {
    if (!replayFen.value) return -1;
    return history.value.findIndex((m) => m.fen === replayFen.value);
});

/** 点击棋谱行 → 把棋盘回放到那一手 */
function replayTo(record: MoveRecord) {
    replayFen.value = record.fen;
}

/** 退出回放，回到实时局面 */
function exitReplay() {
    replayFen.value = null;
}

/** 抽屉关闭时自动退出回放，避免棋盘一直停在历史局面 */
function onDrawerToggle(show: boolean) {
    if (!show) exitReplay();
}

/** 棋谱的纯文本形式，用于复制导出 */
const historyText = computed(() => history.value.map((m, i) => `${i + 1}. ${m.chinese}`).join("\n"));

/** 评分显示：正数带 + 号 */
function formatScore(score: number): string {
    return score > 0 ? `+${score}` : String(score);
}

/** 评分配色：看这步走出**之后**（即下一手记录）的评分变化。
 *
 *  下一手记录的 score 正是"我方走完、对方还没走"时的形势，
 *  两者相减就是**我方这步**的净效果 —— 掉分多说明这一步吃亏了。
 *  只标我方着法，对方走得好不算我们的失误。 */
function scoreClass(index: number): string {
    const cur = history.value[index];
    if (!cur.ours) return "";

    const next = history.value[index + 1];
    if (!next) return "";

    const drop = cur.score - next.score;
    if (drop >= 300) return "is-blunder";
    if (drop >= 120) return "is-worse";
    return "";
}

/** 复盘概览：当前形势 + 全盘最大的一次掉分。
 *
 *  这两条是复盘时最想知道的信息 —— 现在形势如何、哪一步亏得最多。 */
const reviewSummary = computed(() => {
    const list = history.value;
    let worst = 0;
    let worstIndex = -1;

    for (let i = 0; i < list.length - 1; i++) {
        if (!list[i].ours) continue;
        const drop = list[i].score - list[i + 1].score;
        if (drop > worst) {
            worst = drop;
            worstIndex = i;
        }
    }

    return {
        score: list.length > 0 ? list[list.length - 1].score : 0,
        worst,
        worstNo: worstIndex + 1,
        worstMove: worstIndex >= 0 ? list[worstIndex].chinese : "",
    };
});

async function loadHistory() {
    history.value = await invoke<MoveRecord[]>("game_history");
}

/** 打开棋谱抽屉。打开时拉取一次，之后靠 move 事件实时刷新 */
function openHistory() {
    void loadHistory();
    showHistory.value = true;
}

/** 复制棋谱到剪贴板 */
async function copyHistory() {
    if (history.value.length === 0) {
        dialog.warning({
            title: "还没有棋谱",
            content: "先点「启」开始监听，走几步棋后这里会自动记录。",
            positiveText: "确定",
        });
        return;
    }

    if (await writeClipboard(historyText.value)) {
        dialog.success({
            title: "已复制棋谱",
            content: `共 ${history.value.length} 步`,
            positiveText: "确定",
        });
    } else {
        dialog.error({ title: "复制失败", content: historyText.value, positiveText: "确定" });
    }
}

// 走子时实时刷新棋谱（仅在抽屉打开时）
listen("move", async () => {
    if (showHistory.value) await loadHistory();
});

/** 把棋谱导出成文件（由后端写入程序的配置目录） */
async function saveHistory() {
    if (history.value.length === 0) {
        dialog.warning({
            title: "还没有棋谱",
            content: "先点「启」开始监听，走几步棋后这里会自动记录。",
            positiveText: "确定",
        });
        return;
    }

    try {
        const stamp = new Date().toLocaleString("zh-CN", { hour12: false }).replace(/[/: ]/g, "-");
        const path = await invoke<string>("save_history", { stamp });
        dialog.success({
            title: "棋谱已保存",
            content: path,
            positiveText: "确定",
        });
    } catch (error) {
        dialog.error({ title: "保存失败", content: String(error), positiveText: "确定" });
    }
}

onMounted(async () => {
    await getEngineConfig();
    window.addEventListener("keydown", onKeydown);
});

onUnmounted(() => {
    window.removeEventListener("keydown", onKeydown);
});

/** 空格键启停监听。输入框聚焦时不响应，避免干扰输入 */
function onKeydown(event: KeyboardEvent) {
    if (event.code !== "Space") return;

    const target = event.target as HTMLElement | null;
    if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)) {
        return;
    }

    event.preventDefault();
    void toggleEngine();
}

/** 复制当前识别出的盘面（FEN）到剪贴板 */
async function copy_fen() {
    const fen = await invoke<string>("current_fen");
    if (!fen) {
        dialog.warning({
            title: "还没有可复制的局面",
            content: "请先点「启」开始监听，等识别出棋盘后再试。",
            positiveText: "确定",
        });
        return;
    }

    if (await writeClipboard(fen)) {
        dialog.success({
            title: "已复制当前局面",
            content: fen,
            positiveText: "确定",
        });
    } else {
        dialog.error({
            title: "写入剪贴板失败，可手动复制",
            content: fen,
            positiveText: "确定",
        });
    }
}

/** 写剪贴板：优先用 Clipboard API，不可用时退回旧接口 */
async function writeClipboard(text: string): Promise<boolean> {
    try {
        await navigator.clipboard.writeText(text);
        return true;
    } catch {
        const holder = document.createElement("textarea");
        holder.value = text;
        holder.style.position = "fixed";
        holder.style.opacity = "0";
        document.body.appendChild(holder);
        holder.select();
        let ok = false;
        try {
            ok = document.execCommand("copy");
        } finally {
            document.body.removeChild(holder);
        }
        return ok;
    }
}

async function stopListen() {
    await invoke("stop_listen");
    isEngineRunning.value = false;
    listeningTarget.value = "";
    listening.value = false;
}

interface Window {
    id: number;
    title: string;
    app_name: string;
    width: number;
    height: number;
}

const dialog = useDialog();

async function startListen() {
    try {
        // 获取窗口列表
        const windows: Window[] = await invoke("list_windows");

        if (windows.length === 0) {
            dialog.warning({
                title: "警告",
                content: "没有找到可用的窗口",
                positiveText: "确定",
                showIcon: true,
            });
            return;
        }

        const selectedWindowId = ref<number | null>(null);
        const searchQuery = ref("");

        const filteredWindows = computed(() => {
            if (!searchQuery.value) return windows;
            const query = searchQuery.value.toLowerCase();
            return windows.filter(
                (w) => w.title.toLowerCase().includes(query) || w.app_name.toLowerCase().includes(query)
            );
        });

        // 显示窗口选择对话框
        dialog.info({
            title: "选择要监听的窗口",
            class: "window-select-dialog",
            content: () =>
                h(NFlex, { vertical: true, style: "gap: 16px" }, [
                    h(NInput, {
                        clearable: true,
                        placeholder: "搜索窗口...",
                        "onUpdate:value": (val) => (searchQuery.value = val),
                        style: "width: 100%",
                    }),
                    h(NScrollbar, { style: "max-height: 300px" }, [
                        filteredWindows.value.length > 0
                            ? h(
                                  NSpace,
                                  { vertical: true, size: "small" },
                                  filteredWindows.value.map((w) =>
                                      h(
                                          NCard,
                                          {
                                              hoverable: true,
                                              size: "small",
                                              bordered: true,
                                              class: selectedWindowId.value === w.id ? "selected-window" : "",
                                              onClick: () => (selectedWindowId.value = w.id),
                                          },
                                          {
                                              default: () => [
                                                  h(NFlex, { align: "center", justify: "space-between" }, [
                                                      h("div", [
                                                          h("div", { class: "window-title" }, w.title),
                                                          h("div", { class: "window-app" }, [
                                                              w.app_name,
                                                              h(
                                                                  NTag,
                                                                  {
                                                                      size: "tiny",
                                                                      type: "info",
                                                                      style: "margin-left: 8px",
                                                                  },
                                                                  { default: () => `${w.width}×${w.height}` }
                                                              ),
                                                          ]),
                                                      ]),
                                                      h(
                                                          NButton,
                                                          {
                                                              tertiary: true,
                                                              circle: true,
                                                              type:
                                                                  selectedWindowId.value === w.id
                                                                      ? "primary"
                                                                      : "default",
                                                              size: "small",
                                                          },
                                                          {
                                                              default: () =>
                                                                  selectedWindowId.value === w.id ? "✓" : "",
                                                          }
                                                      ),
                                                  ]),
                                              ],
                                          }
                                      )
                                  )
                              )
                            : h(NEmpty, { description: "没有找到匹配的窗口" }),
                    ]),
                ]),
            positiveText: "确定",
            negativeText: "取消",
            style: "max-width: 500px",
            maskClosable: false,
            onPositiveClick: async () => {
                if (!selectedWindowId.value) {
                    dialog.warning({
                        title: "提示",
                        content: "请先选择一个窗口",
                        positiveText: "确定",
                    });
                    return false; // 阻止对话框关闭
                }

                const window = windows.find((w) => w.id === selectedWindowId.value);
                if (window) {
                    try {
                        await invoke("start_listen", { target: window });
                        isEngineRunning.value = true;
                        listeningTarget.value = window.title;
                        listening.value = true;
                    } catch (error) {
                        dialog.error({
                            title: "错误",
                            content: "启动监听失败:" + String(error),
                            positiveText: "确定",
                        });
                    }
                }
            },
        });
    } catch (error) {
        console.error("启动监听失败:", error);
        dialog.error({
            title: "错误",
            content: "启动监听失败: " + String(error),
            positiveText: "确定",
        });
    }
}

async function setEngineDepth() {
    await invoke("set_engine_depth", { depth: config.value.depth });
}

async function setEngineTime() {
    await invoke("set_engine_time", { time: Math.round(config.value.time * 1000) });
}

async function setEngineThreads() {
    await invoke("set_engine_threads", { num: config.value.threads });
}

async function setEngineHash() {
    await invoke("set_engine_hash", { size: config.value.hash });
}

async function setChessdb() {
    await invoke("set_chessdb", {
        enabled: config.value.chessdb_enabled,
        timeout: config.value.chessdb_timeout,
    });
}

async function getEngineConfig() {
    let result: EngineConfig = await invoke("get_engine_config");
    config.value = {
        ...result,
        time: Number((result.time / 1000).toFixed(1)),
    };
}

async function toggleEngine() {
    isEngineRunning.value ? await stopListen() : await startListen();
}
</script>

<template>
    <n-card class="toolbar" :bordered="false" size="small">
        <n-space vertical size="small">
            <n-flex align="center" justify="space-between">
                <n-space align="center" :size="8">
                    <span class="app-title">中国象棋</span>
                    <n-select
                        size="small"
                        v-model:value="mode"
                        :options="options"
                        :consistent-menu-width="false"
                        placeholder="选择模式"
                        class="mode-select"
                    />
                    <n-tag
                        size="small"
                        :bordered="false"
                        :type="isEngineRunning ? 'success' : 'default'"
                        class="status-tag"
                        :class="{ 'is-live': isEngineRunning }"
                    >
                        {{ isEngineRunning ? `监听中 · ${listeningTarget || "未命名窗口"}` : "空闲" }}
                    </n-tag>
                </n-space>

                <n-space>
                    <n-tooltip trigger="hover" placement="bottom">
                        <template #trigger>
                            <n-button
                                circle
                                size="small"
                                :type="isEngineRunning ? 'error' : 'primary'"
                                @click="toggleEngine"
                            >
                                {{ isEngineRunning ? "停" : "启" }}
                            </n-button>
                        </template>
                        {{ isEngineRunning ? "停止引擎" : "启动引擎" }}
                    </n-tooltip>

                    <n-tooltip trigger="hover" placement="bottom">
                        <template #trigger>
                            <n-button circle size="small" type="info" @click="showEngineConfig = true"
                                >配</n-button
                            >
                        </template>
                        引擎配置
                    </n-tooltip>

                    <n-divider vertical />

                    <n-tooltip trigger="hover" placement="bottom">
                        <template #trigger>
                            <n-button circle size="small" type="success" disabled>识</n-button>
                        </template>
                        图片识别（未实现）
                    </n-tooltip>

                    <n-tooltip trigger="hover" placement="bottom">
                        <template #trigger>
                            <n-button circle size="small" type="warning" @click="copy_fen">复</n-button>
                        </template>
                        复制当前局面（FEN）
                    </n-tooltip>

                    <n-tooltip trigger="hover" placement="bottom">
                        <template #trigger>
                            <n-button circle size="small" tertiary @click="openHistory">谱</n-button>
                        </template>
                        棋谱记录
                    </n-tooltip>
                </n-space>
            </n-flex>
        </n-space>

        <n-drawer v-model:show="showEngineConfig" :width="300" placement="right">
            <n-drawer-content title="引擎配置">
                <n-form :model="config" label-placement="left" label-width="80">
                    <n-form-item label="深度">
                        <n-input-number
                            v-model:value="config.depth"
                            button-placement="both"
                            :min="0"
                            :max="200"
                            style="width: 120px"
                            @update:value="setEngineDepth"
                        />
                    </n-form-item>
                    <n-form-item label="时间">
                        <n-input-number
                            v-model:value="config.time"
                            button-placement="both"
                            :step="0.5"
                            :precision="1"
                            :min="0"
                            :max="120"
                            style="width: 120px"
                            @update:value="setEngineTime"
                        />
                    </n-form-item>
                    <n-form-item label="线程数">
                        <n-input-number
                            v-model:value="config.threads"
                            button-placement="both"
                            :min="0"
                            :max="64"
                            style="width: 120px"
                            @update:value="setEngineThreads"
                        />
                    </n-form-item>
                    <n-form-item label="哈希表(m)">
                        <n-input-number
                            v-model:value="config.hash"
                            button-placement="both"
                            :min="32"
                            :max="102400"
                            style="width: 120px"
                            @update:value="setEngineHash"
                        />
                    </n-form-item>
                    <n-form-item label="启用云库">
                        <n-switch v-model:value="config.chessdb_enabled" @update:value="setChessdb" />
                    </n-form-item>
                    <n-form-item label="云库超时(s)">
                        <n-input-number
                            v-model:value="config.chessdb_timeout"
                            :disabled="!config.chessdb_enabled"
                            :min="1"
                            :max="60"
                            :step="1"
                            style="width: 120px"
                            @update:value="setChessdb"
                        />
                    </n-form-item>
                </n-form>
            </n-drawer-content>
        </n-drawer>

        <!-- 棋谱：自动记录每一步走子，可复制导出 -->
        <n-drawer v-model:show="showHistory" :width="280" placement="right" @update:show="onDrawerToggle">
            <n-drawer-content>
                <template #header>
                    <n-flex align="center" justify="space-between" style="width: 100%">
                        <span>棋谱（{{ history.length }} 步）</span>
                        <n-space :size="6">
                            <n-button size="tiny" tertiary @click="copyHistory">复制</n-button>
                            <n-button size="tiny" tertiary @click="saveHistory">保存</n-button>
                        </n-space>
                    </n-flex>
                </template>
                <n-empty v-if="history.length === 0" description="还没有记录到走子" />
                <template v-else>
                    <div v-if="replayFen" class="replay-banner">
                        <span>正在回放第 {{ replayIndex + 1 }} 手，棋盘显示的是那一刻的局面</span>
                        <n-button size="tiny" tertiary @click="exitReplay">返回实时</n-button>
                    </div>
                    <div class="history-summary">
                        <n-flex justify="space-between" align="center">
                            <span class="summary-label">当前评分</span>
                            <span class="summary-score">{{ formatScore(reviewSummary.score) }}</span>
                        </n-flex>
                        <div v-if="reviewSummary.worstMove" class="summary-detail">
                            最大失误：第 {{ reviewSummary.worstNo }} 手「{{ reviewSummary.worstMove }}」，掉了
                            {{ reviewSummary.worst }} 分
                        </div>
                        <div v-else class="summary-detail">暂时没有明显掉分</div>
                    </div>
                    <n-scrollbar style="max-height: 360px">
                        <n-flex vertical :size="0">
                            <div
                                v-for="(m, i) in history"
                                :key="i"
                                class="history-row"
                                :class="{ 'is-replaying': replayFen === m.fen }"
                                @click="replayTo(m)"
                            >
                                <span class="history-index">{{ i + 1 }}</span>
                                <span class="history-move" :class="{ 'is-ours': m.ours }">{{ m.chinese }}</span>
                                <span class="history-score" :class="scoreClass(i)">{{ formatScore(m.score) }}</span>
                            </div>
                        </n-flex>
                    </n-scrollbar>
                    <div class="history-hint">
                        点任意一手可把棋盘回放到那一刻。评分正数对我方有利，带色的是掉分较多的我方着法。
                    </div>
                </template>
            </n-drawer-content>
        </n-drawer>
    </n-card>
</template>

<style scoped>
.toolbar {
    width: 100%;
    padding: 8px;
    border-radius: 8px;
}

.mode-select {
    width: 96px;
}

/* 界面标题 */
.app-title {
    font-size: 14px;
    font-weight: 500;
    color: #e5e7eb;
    letter-spacing: 0.5px;
    white-space: nowrap;
}

/* 监听状态标签：目标窗口名可能很长，超出省略 */
.status-tag {
    max-width: 190px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}

/* 正在监听时做呼吸效果，一眼看出程序在工作 */
.status-tag.is-live {
    animation: status-breathe 2.2s ease-in-out infinite;
}

@keyframes status-breathe {
    0%,
    100% {
        opacity: 1;
    }
    50% {
        opacity: 0.5;
    }
}

/* 棋谱条目（可点击回放） */
.history-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 3px 6px;
    border-radius: 4px;
    font-size: 13px;
    cursor: pointer;
    transition: background-color 0.15s;
}

.history-row:hover {
    background: rgba(128, 128, 128, 0.14);
}

/* 当前正在回放的那一手 */
.history-row.is-replaying {
    background: rgba(99, 179, 255, 0.2);
}

/* 回放提示条 */
.replay-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 10px;
    padding: 8px 10px;
    border-radius: 8px;
    font-size: 11px;
    line-height: 1.4;
    color: #f0c674;
    background: rgba(240, 198, 116, 0.14);
}

.history-index {
    width: 28px;
    flex: none;
    text-align: right;
    font-size: 12px;
    color: #6b7280;
}

.history-move {
    color: #d1d5db;
}

/* 我方着法高亮，便于在棋谱里分辨彼此 */
.history-move.is-ours {
    color: #63b3ff;
    font-weight: 500;
}

.history-score {
    margin-left: auto;
    font-size: 12px;
    color: #6b7280;
    font-variant-numeric: tabular-nums;
}

/* 掉分明显：橙色提示 */
.history-score.is-worse {
    color: #e8954a;
}

/* 掉分严重：红色告警，复盘时一眼定位 */
.history-score.is-blunder {
    color: #e5484d;
    font-weight: 500;
}

.history-hint {
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid rgba(128, 128, 128, 0.2);
    font-size: 11px;
    line-height: 1.5;
    color: #6b7280;
}

/* 复盘概览 */
.history-summary {
    margin-bottom: 10px;
    padding: 10px 12px;
    border-radius: 8px;
    background: rgba(128, 128, 128, 0.12);
}

.summary-label {
    font-size: 12px;
    color: #9ca3af;
}

.summary-score {
    font-size: 18px;
    font-weight: 500;
    color: #e5e7eb;
    font-variant-numeric: tabular-nums;
}

.summary-detail {
    margin-top: 6px;
    font-size: 11px;
    line-height: 1.5;
    color: #9ca3af;
}

:deep(.n-button) {
    display: flex;
    align-items: center;
    justify-content: center;
    min-width: 36px;
    height: 36px;
    transition: all 0.3s;
}

:deep(.n-button:hover) {
    transform: translateY(-2px);
    box-shadow: 0 3px 12px rgba(0, 0, 0, 0.5);
}

:deep(.window-title) {
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 360px;
}

:deep(.window-app) {
    font-size: 12px;
    color: #999;
    margin-top: 4px;
    display: flex;
    align-items: center;
}

:deep(.selected-window) {
    border-color: var(--primary-color) !important;
    background-color: rgba(var(--primary-color-rgb), 0.05);
}
</style>
