<script setup lang="ts">
import { NConfigProvider, NDialogProvider, NDivider, NLayoutFooter, darkTheme } from "naive-ui";
import { onMounted, onUnmounted, ref } from "vue";
import Analyse from "./components/Analyse.vue";
import Chessboard from "./components/Chessboard.vue";
import Toolbar from "./components/Toolbar.vue";

/* ------------------------------------------------------------------ *
 * 窗口自适应
 *
 * 界面按 660×580 设计。窗口可以自由缩放，内容整体等比适配，
 * 既不会因窗口变小而被截断，也不会因窗口变大而显得空旷。
 *
 * 用整体缩放而不是重写响应式布局：棋盘、分析面板内部都有基于固定
 * 像素的定位与尺寸，逐个改成弹性布局风险高、收益低；而 transform
 * 缩放会建立新的 containing block，内部原有的绝对定位依然相对舞台。
 * ------------------------------------------------------------------ */
const DESIGN_W = 660;
const DESIGN_H = 580;
const scale = ref(1);

/** 构建标识，构建时由 vite 注入。显示在页脚，用于确认当前运行的是哪一次构建 */
const buildTag = __BUILD_TAG__;

function updateScale() {
    const ratio = Math.min(window.innerWidth / DESIGN_W, window.innerHeight / DESIGN_H);
    // 下限 0.55 保证还能看清；上限 3 避免在超大屏上撑得夸张
    scale.value = Math.min(3, Math.max(0.55, ratio));
}

onMounted(() => {
    updateScale();
    window.addEventListener("resize", updateScale);
});

onUnmounted(() => {
    window.removeEventListener("resize", updateScale);
});
</script>

<template>
    <div class="app-viewport">
        <!-- 深色主题：整体配色交给 naive-ui，木色棋盘自然成为视觉焦点 -->
        <n-config-provider :theme="darkTheme">
            <div class="app-stage" :style="{ transform: `translate(-50%, -50%) scale(${scale})` }">
                <n-dialog-provider>
                    <!-- 最上面 -->
                    <Toolbar />

                    <n-divider class="spliter-toolbar" />

                    <!-- 下方左侧 -->
                    <Chessboard />

                    <n-divider vertical class="spliter-middle" />

                    <!-- 下方右侧 -->
                    <Analyse />

                    <n-layout-footer class="footer" position="absolute">
                        <span class="credit">
                            原项目
                            <a href="https://github.com/atopx/chessboard.git">atopx</a>
                            · 修改版 by
                            <a href="https://github.com/aaay313/xqlink">aaay313</a>
                        </span>
                        <span class="build-tag">{{ buildTag }}</span>
                    </n-layout-footer>
                </n-dialog-provider>
            </div>
        </n-config-provider>
    </div>
</template>

<style scoped>
/* 页面底色与 naive-ui 深色主题的 bodyColor 一致，避免边缘露白 */
:global(html),
:global(body) {
    margin: 0;
    padding: 0;
    overflow: hidden;
    background-color: #101014;
    color-scheme: dark;
}

/* 视口铺满窗口，超出的部分裁掉 */
.app-viewport {
    position: relative;
    width: 100vw;
    height: 100vh;
    overflow: hidden;
    background-color: #101014;
}

/* 舞台保持设计尺寸，居中并按 scale 等比缩放 */
.app-stage {
    position: absolute;
    top: 50%;
    left: 50%;
    width: 660px;
    height: 580px;
    transform-origin: center center;
}

.footer {
    left: 13px;
    bottom: 5px;
    font-size: x-small;
}

.footer a {
    color: #6b7280;
    text-decoration: none;
}

.footer a:hover {
    color: #9ca3af;
}

/* 署名：原项目 + 修改版作者。原署名必须保留（Apache-2.0） */
.credit {
    color: #6b7280;
}

/* 构建标识：与页脚同行，颜色更淡，不抢视线 */
.build-tag {
    margin-left: 10px;
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 10px;
    color: #9ca3af;
    background: rgba(128, 128, 128, 0.18);
}

.spliter-toolbar {
    position: absolute;
    width: 630px;
    top: 60px;
    left: 10px;
    z-index: 1;
}

.spliter-middle {
    position: absolute;
    left: 398px;
    top: 100px;
    height: 425px;
    z-index: 1;
}
</style>
