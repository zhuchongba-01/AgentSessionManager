// 轻量平台检测，避免在 SSR 或无 navigator 的环境报错。
// 当前用于适配 macOS 标题栏留白与 Windows 自绘窗口控制（Linux 使用原生窗口装饰）。
export const isMac = (): boolean => {
  try {
    const ua = navigator.userAgent || "";
    const plat = (navigator.platform || "").toLowerCase();
    return /mac/i.test(ua) || plat.includes("mac");
  } catch {
    return false;
  }
};

export const isWindows = (): boolean => {
  try {
    const ua = navigator.userAgent || "";
    const plat = (navigator.platform || "").toLowerCase();
    return /windows|win32|win64/i.test(ua) || plat.startsWith("win");
  } catch {
    return false;
  }
};
