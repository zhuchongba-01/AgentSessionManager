// 轻量平台检测，避免在 SSR 或无 navigator 的环境报错。
// 前端唯一依赖平台分支的行为是"恢复会话"：仅 macOS 支持唤起终端，
// 其他平台回退为复制 resume 命令；窗口拖拽由原生标题栏承担，
// 不再需要前身项目自定义标题栏的拖拽区域检测。
export const isMac = (): boolean => {
  try {
    const ua = navigator.userAgent || "";
    const plat = (navigator.platform || "").toLowerCase();
    return /mac/i.test(ua) || plat.includes("mac");
  } catch {
    return false;
  }
};
