// 轻量平台检测，避免在 SSR 或无 navigator 的环境报错。
// 当前用于适配 macOS 标题栏留白与窗口控制；窗口拖拽由原生标题栏承担。
export const isMac = (): boolean => {
  try {
    const ua = navigator.userAgent || "";
    const plat = (navigator.platform || "").toLowerCase();
    return /mac/i.test(ua) || plat.includes("mac");
  } catch {
    return false;
  }
};
