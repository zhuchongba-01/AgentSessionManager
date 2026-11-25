import { FileText, FolderOpen } from "lucide-react";
import { memo, useCallback, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { HoverTip } from "@/components/ui/hover-tip";
import { sessionsApi } from "@/lib/api/sessions";
import { copyText } from "@/lib/clipboard";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";
import { highlightQueryText as highlightText } from "../utils";
import { useReaderContext } from "./context";

const MAX_DISPLAY_LENGTH = 48;

/** 把 `file://` URL 转成本地路径；不是 file URL 时原样返回 */
export const fileUrlToPath = (value: string) => {
  if (!/^file:\/\//i.test(value)) return value;
  try {
    const url = new URL(value);
    const pathname = decodeURIComponent(url.pathname);
    // file:///C:/foo → C:/foo，去掉盘符前多余的斜杠
    const local = /^\/[A-Za-z]:[\\/]/.test(pathname)
      ? pathname.slice(1)
      : pathname;
    return url.host ? `//${url.host}${local}` : local;
  } catch {
    return value.replace(/^file:\/\//i, "");
  }
};

/** 剥离末尾 `:line(:col)?` 或 `#L123(-L145)?`，保留 Windows 盘符 `C:\` */
export const stripPathLocationSuffix = (value: string) =>
  value
    .replace(/#L\d+(?:-L?\d+)?$/i, "")
    .replace(/(?!^[A-Za-z]:):\d+(?::\d+)?$/, "");

/** 解析用于文件管理器定位的磁盘路径（支持基于 projectDir 解析相对路径） */
export const resolveRevealTargetPath = (
  rawPath: string,
  projectDir?: string,
): string => {
  const clean = stripPathLocationSuffix(fileUrlToPath(rawPath).trim());
  if (!clean) return rawPath;
  const isAbsolute =
    clean === "~" ||
    clean.startsWith("/") ||
    clean.startsWith("~/") ||
    clean.startsWith("~\\") ||
    clean.startsWith("\\\\") ||
    /^[A-Za-z]:[\\/]/.test(clean);
  if (isAbsolute || !projectDir?.trim()) return clean;

  const base = projectDir.trim().replace(/[\\/]+$/, "");
  const sep = /^[A-Za-z]:\\|^\\\\/.test(base) ? "\\" : "/";
  const rel = clean.replace(/^\.[\\/]/, "").replace(/[\\/]+/g, sep);
  return `${base}${sep}${rel}`;
};

/**
 * 缩短显示用路径：位于 projectDir 下时显示相对路径，位于用户目录下时
 * 换成 `~`，仍然过长则只保留最后三段。
 */
export const shortenPath = (path: string, projectDir?: string) => {
  const normalized = path.replace(/\\/g, "/");
  let display = normalized;

  const root = projectDir?.replace(/\\/g, "/").replace(/\/+$/, "");
  const isWinRoot = Boolean(root && /^[A-Za-z]:\//.test(root));
  if (
    root &&
    (isWinRoot
      ? normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)
      : normalized.startsWith(`${root}/`))
  ) {
    display = normalized.slice(root.length + 1);
  } else {
    const home =
      /^(\/Users\/[^/]+|\/home\/[^/]+|[A-Za-z]:\/Users\/[^/]+)\//.exec(
        normalized,
      );
    if (home) display = `~/${normalized.slice(home[0].length)}`;
  }

  if (display.length <= MAX_DISPLAY_LENGTH) return display;
  const segments = display.split("/").filter(Boolean);
  return segments.length > 3 ? `…/${segments.slice(-3).join("/")}` : display;
};

/** 默认的「在文件管理器中定位」：调用后端 `reveal_session_path` 命令 */
export const revealSessionPath = async (path: string) => {
  await sessionsApi.revealPath(path);
};

export interface PathChipProps {
  /** 本地路径（`file://` URL 或绝对/项目内相对路径均可） */
  path: string;
  /** 会话所在项目目录，用来把路径显示成相对路径并辅助定位 */
  projectDir?: string;
  /** 自定义显示内容；不传时显示缩短后的路径 */
  label?: ReactNode;
  searchQuery?: string;
  /** 自定义定位回调；不传时调用 `sessionsApi.revealPath` */
  onReveal?: (path: string) => void;
  className?: string;
}

/**
 * 本地路径胶囊：`[📄 rel/or/short/path | 📂]`，悬停看全路径，点击复制，
 * 右侧文件夹按钮在系统文件管理器中定位。不做任何文件读取。
 */
export const PathChip = memo(function PathChip({
  path,
  projectDir,
  label,
  searchQuery,
  onReveal,
  className,
}: PathChipProps) {
  const { t } = useTranslation();
  const ctx = useReaderContext();
  const effectiveProjectDir = projectDir ?? ctx.projectDir;
  const effectiveSearchQuery = searchQuery ?? ctx.searchQuery;
  const effectiveReveal = onReveal ?? ctx.onRevealPath;
  const localPath = fileUrlToPath(path);
  const revealTarget = resolveRevealTargetPath(localPath, effectiveProjectDir);
  const display = shortenPath(localPath, effectiveProjectDir);

  const handleCopy = useCallback(async () => {
    const ok = await copyText(localPath);
    if (ok) {
      toast.success(
        t("sessionManager.pathCopied", {
          defaultValue: "已复制路径 {{path}}",
          path: localPath,
        }),
      );
    } else {
      toast.error(t("sessionManager.copyFailed", { defaultValue: "复制失败" }));
    }
  }, [localPath, t]);

  const handleReveal = useCallback(async () => {
    if (effectiveReveal) {
      effectiveReveal(revealTarget);
      return;
    }
    try {
      await revealSessionPath(revealTarget);
    } catch (error) {
      toast.error(
        t("sessionManager.revealFailed", {
          defaultValue: "无法在文件管理器中定位：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    }
  }, [effectiveReveal, revealTarget, t]);

  const copyLabel = t("sessionManager.reader.path.copy", {
    defaultValue: "复制路径",
  });
  const revealLabel = t("sessionManager.reader.path.reveal", {
    defaultValue: "在文件管理器中定位",
  });

  return (
    <span
      data-path-chip=""
      className={cn(
        "inline-flex max-w-full items-center rounded-control border border-border bg-subtle align-baseline text-caption text-fg-1",
        className,
      )}
    >
      <button
        type="button"
        title={localPath}
        aria-label={`${copyLabel}: ${localPath}`}
        onClick={() => void handleCopy()}
        className="inline-flex min-w-0 items-center gap-1 rounded-s-control py-px pe-1 ps-1.5 font-mono transition-colors hover:bg-selected focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <FileText aria-hidden className="size-3 shrink-0 text-fg-3" />
        <span className="truncate">
          {label ??
            (effectiveSearchQuery
              ? highlightText(display, effectiveSearchQuery)
              : display)}
        </span>
      </button>
      <HoverTip content={revealLabel}>
        <button
          type="button"
          aria-label={revealLabel}
          onClick={() => void handleReveal()}
          className="inline-flex h-full shrink-0 items-center rounded-e-control border-s border-border px-1 py-0.5 text-fg-3 transition-colors hover:bg-selected hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <FolderOpen aria-hidden className="size-3" />
        </button>
      </HoverTip>
    </span>
  );
});
