import { useTranslation } from "react-i18next";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  Github,
  Minus,
  Monitor,
  Moon,
  Sparkles,
  Square,
  Sun,
  X,
} from "lucide-react";
import appIcon from "@/icons/app-icon.png";
import {
  APP_VERSION,
  GITHUB_LATEST_RELEASE_URL,
  GITHUB_REPOSITORY_URL,
  type AvailableUpdate,
} from "@/lib/githubRelease";
import { isMac, isWindows } from "@/lib/platform";
import { cn } from "@/lib/utils";
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
  theme: "light" | "dark" | "system";
  resolvedTheme: "light" | "dark";
  onSetTheme: (theme: "light" | "dark" | "system") => void;
  onOpenUrl: (url: string) => void;
}

export function AppTitleBar({
  availableUpdate,
  theme,
  resolvedTheme,
  onSetTheme,
  onOpenUrl,
}: AppTitleBarProps) {
  const { t } = useTranslation();
  const showWinControls = isWindows();

  return (
    <header
      className="asm-command-bar"
      data-tauri-drag-region
      style={isMac() ? { paddingLeft: 80 } : undefined}
    >
      {/* 品牌区：自绘标题栏，空余区域可拖拽窗口 */}
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
                <Sparkles className="size-3" aria-hidden="true" />
                <span>
                  {t("sessionManager.updateButton", {
                    defaultValue: "更新",
                  })}
                </span>
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

      <div
        className={cn(
          "asm-topbar-actions",
          showWinControls && "has-win-controls",
        )}
      >
        <div className="asm-topbar-capsule">
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className="asm-topbar-btn"
                aria-label={t("sessionManager.openProjectGitHub", {
                  defaultValue: "在 GitHub 查看项目",
                })}
                onClick={() => onOpenUrl(GITHUB_REPOSITORY_URL)}
              >
                <Github className="size-3.5" aria-hidden="true" />
                <span>GitHub</span>
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.openProjectGitHub", {
                defaultValue: "在 GitHub 查看项目",
              })}
            </TooltipContent>
          </Tooltip>

          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className={cn(
                  "asm-topbar-btn is-icon",
                  theme === "system" && "is-active",
                )}
                aria-label={t("sessionManager.followSystemTheme", {
                  defaultValue: "跟随系统外观",
                })}
                aria-pressed={theme === "system"}
                onClick={() => onSetTheme("system")}
              >
                <Monitor className="size-3.5" aria-hidden="true" />
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {theme === "system"
                ? t("sessionManager.followingSystemTheme", {
                    defaultValue: "当前已跟随系统外观",
                  })
                : t("sessionManager.followSystemTheme", {
                    defaultValue: "跟随系统外观",
                  })}
            </TooltipContent>
          </Tooltip>

          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className="asm-topbar-btn is-icon"
                aria-label={
                  resolvedTheme === "dark"
                    ? t("sessionManager.switchToLightTheme", {
                        defaultValue: "切换到明亮主题",
                      })
                    : t("sessionManager.switchToDarkTheme", {
                        defaultValue: "切换到暗色主题",
                      })
                }
                onClick={() =>
                  onSetTheme(resolvedTheme === "dark" ? "light" : "dark")
                }
              >
                {resolvedTheme === "dark" ? (
                  <Sun className="size-3.5" aria-hidden="true" />
                ) : (
                  <Moon className="size-3.5" aria-hidden="true" />
                )}
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {resolvedTheme === "dark"
                ? t("sessionManager.switchToLightTheme", {
                    defaultValue: "切换到明亮主题",
                  })
                : t("sessionManager.switchToDarkTheme", {
                    defaultValue: "切换到暗色主题",
                  })}
            </TooltipContent>
          </Tooltip>
        </div>

        {showWinControls && (
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
