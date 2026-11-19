import { useTranslation } from "react-i18next";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Minus, Square, X } from "lucide-react";
import appIcon from "@/icons/app-icon.png";
import {
  APP_VERSION,
  GITHUB_LATEST_RELEASE_URL,
  type AvailableUpdate,
} from "@/lib/githubRelease";
import { isMac } from "@/lib/platform";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

// 自定义标题栏的窗口控制；浏览器/测试环境（无 Tauri IPC）静默忽略。
// “关闭”与托盘行为一致：隐藏到托盘而不是退出（见 lib.rs CloseRequested）。
const windowAction = (action: "minimize" | "toggleMaximize" | "close") => {
  try {
    const appWindow = getCurrentWindow();
    if (action === "minimize") void appWindow.minimize();
    else if (action === "toggleMaximize") void appWindow.toggleMaximize();
    else void appWindow.close();
  } catch {
    // 非 Tauri 环境
  }
};

interface AppTitleBarProps {
  availableUpdate: AvailableUpdate | null;
  onOpenUrl: (url: string) => void;
}

export function AppTitleBar({ availableUpdate, onOpenUrl }: AppTitleBarProps) {
  const { t } = useTranslation();

  return (
    <header
      className="asm-command-bar"
      data-tauri-drag-region
      style={isMac() ? { paddingLeft: 80 } : undefined}
    >
      {/* 品牌区：即标题栏（自绘），空余区域可拖拽窗口 */}
      <div className="asm-topbar-brand" data-tauri-drag-region>
        <img src={appIcon} alt="" className="asm-app-icon" />
        <span className="asm-app-name" data-tauri-drag-region>
          Agent会话管理
        </span>
        <span className="asm-app-version" data-tauri-drag-region>
          v{APP_VERSION}
        </span>
        {availableUpdate && (
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className="asm-update-button"
                aria-label={t("sessionManager.updateAvailable", {
                  defaultValue: "发现新版本 v{{version}}",
                  version: availableUpdate.version,
                })}
                onClick={() => void onOpenUrl(GITHUB_LATEST_RELEASE_URL)}
              >
                {t("sessionManager.updateButton", {
                  defaultValue: "更新",
                })}
              </button>
            </TooltipTrigger>
            <TooltipContent
              className="asm-update-tooltip"
              side="bottom"
              align="start"
            >
              <strong className="asm-update-tooltip-title">
                {t("sessionManager.updateTooltip", {
                  defaultValue: "发现新版本 v{{version}}",
                  version: availableUpdate.version,
                })}
              </strong>
              <span className="asm-update-tooltip-notes">
                {availableUpdate.notes ||
                  t("sessionManager.updateNotesUnavailable", {
                    defaultValue: "该版本暂未提供更新说明",
                  })}
              </span>
            </TooltipContent>
          </Tooltip>
        )}
      </div>
      <div className="asm-topbar-actions">
        {!isMac() && (
          <div className="asm-window-controls">
            <button
              type="button"
              aria-label={t("sessionManager.minimizeWindow", {
                defaultValue: "最小化",
              })}
              onClick={() => windowAction("minimize")}
            >
              <Minus className="size-3.5" />
            </button>
            <button
              type="button"
              aria-label={t("sessionManager.maximizeWindow", {
                defaultValue: "最大化",
              })}
              onClick={() => windowAction("toggleMaximize")}
            >
              <Square className="size-3" />
            </button>
            <button
              type="button"
              aria-label={t("sessionManager.closeWindow", {
                defaultValue: "关闭",
              })}
              className="close"
              onClick={() => windowAction("close")}
            >
              <X className="size-4" />
            </button>
          </div>
        )}
      </div>
    </header>
  );
}
