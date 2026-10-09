import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

/** 用编译时刻生成构建标识，注入前端并显示在界面上。
 *  这样"当前跑的是哪一次构建"一眼可辨，不用再靠比对文件哈希。 */
function buildTag(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `自编译 ${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [vue()],

  define: {
    __BUILD_TAG__: JSON.stringify(buildTag()),
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ["**/server/**"],
    },
  },
}));
