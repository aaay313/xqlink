/** 构建标识（形如「自编译 2026-10-09 11:20」）。
 *
 *  由 vite.config.ts 的 define 在构建时注入，用于在界面上显示当前运行的
 *  是哪一次构建 —— 排查"改动没生效"这类问题时可以一眼确认。 */
declare const __BUILD_TAG__: string;
