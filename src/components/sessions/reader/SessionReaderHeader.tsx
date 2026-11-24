import { memo, useMemo, type ReactNode, type RefObject } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowDown,
  ArrowLeft,
  ArrowUp,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  Copy,
  Download,
  Folder,
  FolderOpen,
  MoreHorizontal,
  PanelRight,
  Search,
  SlidersHorizontal,
  X,
} from "lucide-react";

import { AppGlyph } from "@/components/shell/AppGlyph";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { HoverTip } from "@/components/ui/hover-tip";
import { SegmentedControl } from "@/components/ui/segmented-control";
import { useMoneyFormat } from "@/hooks/useMoneyFormat";
import { sessionsApi } from "@/lib/api/sessions";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import type { SessionMessage, SessionMeta, TurnIndex } from "@/types";
import { SessionStatusBadges } from "../SessionStatusBadges";
import {
  buildCdResumeCommand,
  extractRevealableLocalPath,
  formatClock,
  formatShortDateTime,
  isSessionAppId,
} from "../utils";
import { useReaderT } from "./i18n";
import { PathChip } from "./PathChip";
import { formatDuration, formatTokens } from "./toolSummary";
import type { ReaderFilter, SessionTurn } from "./turns";

export const readerIconButton =
  "inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-control text-fg-2 transition-colors hover:bg-subtle hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-40";

const readerMenuItemClass = "gap-2 px-2.5 py-1.5 text-caption";

const findInputClass =
  "h-7 w-full rounded-control border border-border bg-surface pe-2 ps-7 text-caption text-fg-1 placeholder:text-fg-3 outline-none transition-colors focus:border-ring focus:ring-1 focus:ring-ring";

// ─── 页头：返回、标题、状态徽标、上一个 / 下一个、恢复、⋯ ──────────────────

export interface SessionReaderHeaderProps {
  session: SessionMeta;
  title: string;
  appName: string;
  parentTitle?: string;
  hasPrev: boolean;
  hasNext: boolean;
  canCopyMarkdown: boolean;
  includeThinking: boolean;
  onIncludeThinkingChange: (value: boolean) => void;
  onPrev: () => void;
  onNext: () => void;
  onBack: () => void;
  onCopy: (text: string, message: string) => void;
  onCopyMarkdown: () => void;
  onExportMarkdown: () => void;
  onReload: () => void;
  onDelete: () => void;
}

export const SessionReaderHeader = memo(function SessionReaderHeader({
  session,
  title,
  appName,
  parentTitle,
  hasPrev,
  hasNext,
  canCopyMarkdown,
  includeThinking,
  onIncludeThinkingChange,
  onPrev,
  onNext,
  onBack,
  onCopy,
  onCopyMarkdown,
  onExportMarkdown,
  onReload,
  onDelete,
}: SessionReaderHeaderProps) {
  const { t } = useTranslation();
  const rt = useReaderT();
  const appId = isSessionAppId(session.providerId) ? session.providerId : null;
  const isCodex = session.providerId === "codex";
  const command = session.resumeCommand?.trim() || undefined;
  // 本应用全部 13 个供应商只要有 sourcePath 均支持安全删除
  const deletable = Boolean(session.sourcePath);
  const revealableSourcePath = extractRevealableLocalPath(session.sourcePath);
  const showStatusBadges = Boolean(
    session.archived ||
      session.residual ||
      session.cleanupPending ||
      session.parentSessionId,
  );

  const copyResume = () => {
    if (!command) return;
    onCopy(
      command,
      t("sessionManager.resumeCommandCopied", {
        defaultValue: "已复制恢复命令",
      }),
    );
  };

  const revealSource = async () => {
    if (!revealableSourcePath) return;
    try {
      await sessionsApi.revealPath(revealableSourcePath);
    } catch (error) {
      toast.error(
        t("sessionManager.revealFailed", {
          defaultValue: "无法在文件管理器中定位：{{error}}",
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    }
  };

  let resume: ReactNode;
  if (command) {
    resume = (
      <div className="flex shrink-0">
        <Button
          variant="primary"
          size="regular"
          onClick={copyResume}
          className="rounded-e-none pe-3 ps-3"
        >
          <Copy className="h-3.5 w-3.5" strokeWidth={2} />
          {t("sessionManager.copyResumeCommand", {
            defaultValue: "复制恢复命令",
          })}
        </Button>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="primary"
              size="regular"
              aria-label={t("sessionManager.moreResumeOptions", {
                defaultValue: "更多恢复方式",
              })}
              className="w-8 rounded-s-none border-s border-white/20 px-0"
            >
              <ChevronDown className="h-3.5 w-3.5" strokeWidth={2} />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            align="end"
            className="w-[300px] max-w-[calc(100vw-32px)] rounded-panel p-1 shadow-v7-md"
          >
            <DropdownMenuItem
              className={readerMenuItemClass}
              title={command}
              onSelect={copyResume}
            >
              <Copy
                className="h-3.5 w-3.5 shrink-0 text-fg-2"
                strokeWidth={1.5}
              />
              <span className="shrink-0">
                {t("sessionManager.copyResumeCommand", {
                  defaultValue: "复制恢复命令",
                })}
              </span>
              <span
                aria-hidden="true"
                className="min-w-0 flex-1 truncate text-right font-mono text-caption text-fg-3"
              >
                {command}
              </span>
            </DropdownMenuItem>
            {session.projectDir && (
              <DropdownMenuItem
                className={readerMenuItemClass}
                onSelect={() =>
                  onCopy(
                    buildCdResumeCommand(session.projectDir!, command),
                    t("sessionManager.cdResumeCopied", {
                      defaultValue: "已复制「进入目录并恢复」命令",
                    }),
                  )
                }
              >
                <Folder
                  className="h-3.5 w-3.5 shrink-0 text-fg-2"
                  strokeWidth={1.5}
                />
                {t("sessionManager.copyCdResume", {
                  defaultValue: "复制进入目录并恢复的命令",
                })}
              </DropdownMenuItem>
            )}
            {isCodex && (
              <p className="-mx-1 -mb-1 mt-1 border-t border-border px-3.5 pb-2.5 pt-2 text-caption text-fg-2">
                {t("sessionManager.codexResumeNote", {
                  defaultValue:
                    "切换过供应商后，旧会话可能无法继续：Codex 按供应商分开保存历史。",
                })}
              </p>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    );
  } else {
    resume = (
      <Button
        variant="primary"
        size="regular"
        aria-disabled={true}
        aria-describedby="session-resume-note"
        className="shrink-0 cursor-not-allowed pe-3.5 ps-3 opacity-45 hover:opacity-45"
      >
        <Copy className="h-3.5 w-3.5" strokeWidth={2} />
        {t("sessionManager.copyResumeCommand", {
          defaultValue: "复制恢复命令",
        })}
      </Button>
    );
  }

  const moreMenu = (
    <DropdownMenu>
      <HoverTip content={t("common.more", { defaultValue: "更多" })}>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            id="session-reader-more"
            aria-label={t("sessionManager.moreFor", {
              defaultValue: "{{title}} 的更多操作",
              title,
            })}
            className={cn(readerIconButton, "h-8 w-8")}
          >
            <MoreHorizontal className="h-4 w-4" strokeWidth={1.5} />
          </button>
        </DropdownMenuTrigger>
      </HoverTip>
      <DropdownMenuContent
        align="end"
        className="w-[240px] rounded-panel p-1 shadow-v7-md"
      >
        <DropdownMenuItem
          className={readerMenuItemClass}
          disabled={!canCopyMarkdown}
          onSelect={onCopyMarkdown}
        >
          {t("sessionManager.copyAsMarkdown", {
            defaultValue: "复制整段为 Markdown",
          })}
        </DropdownMenuItem>
        <DropdownMenuItem
          className={readerMenuItemClass}
          disabled={!canCopyMarkdown}
          onSelect={onExportMarkdown}
        >
          {rt("exportMarkdown")}
        </DropdownMenuItem>
        <DropdownMenuCheckboxItem
          className={cn(readerMenuItemClass, "ps-8")}
          checked={includeThinking}
          onCheckedChange={(value) => onIncludeThinkingChange(value === true)}
          onSelect={(event) => event.preventDefault()}
        >
          {rt("copyWithThinking")}
        </DropdownMenuCheckboxItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className={readerMenuItemClass}
          onSelect={() =>
            onCopy(
              session.sessionId,
              t("sessionManager.sessionIdCopied", {
                defaultValue: "已复制会话 ID",
              }),
            )
          }
        >
          {t("sessionManager.copySessionIdLabel", {
            defaultValue: "复制会话 ID",
          })}
        </DropdownMenuItem>
        {session.sourcePath && (
          <DropdownMenuItem
            className={readerMenuItemClass}
            onSelect={() =>
              onCopy(
                session.sourcePath!,
                t("sessionManager.sourcePathCopied", {
                  defaultValue: "已复制源文件路径",
                }),
              )
            }
          >
            {t("sessionManager.copySourcePathLabel", {
              defaultValue: "复制源文件路径",
            })}
          </DropdownMenuItem>
        )}
        {revealableSourcePath && (
          <DropdownMenuItem
            className={readerMenuItemClass}
            onSelect={() => void revealSource()}
          >
            <FolderOpen
              aria-hidden
              className="h-3.5 w-3.5 shrink-0 text-fg-2"
              strokeWidth={1.5}
            />
            {t("sessionManager.revealSourceFile", {
              defaultValue: "在文件管理器中定位源文件",
            })}
          </DropdownMenuItem>
        )}
        <DropdownMenuItem className={readerMenuItemClass} onSelect={onReload}>
          {t("sessionManager.reload", { defaultValue: "重新读取" })}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className={cn(
            readerMenuItemClass,
            deletable ? "text-danger-text" : "text-fg-3",
          )}
          disabled={!deletable}
          onSelect={onDelete}
        >
          {t("sessionManager.deleteSessionEllipsis", {
            defaultValue: "删除会话…",
          })}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );

  return (
    <header className="flex h-12 shrink-0 items-center justify-between gap-3 border-b border-border bg-surface px-4">
      <div className="flex min-w-0 flex-1 items-center gap-2">
        <HoverTip content={t("sessionManager.back", { defaultValue: "返回" })}>
          <button
            type="button"
            id="session-reader-back"
            aria-label={t("sessionManager.backToList", {
              defaultValue: "返回会话列表",
            })}
            onClick={onBack}
            className={readerIconButton}
          >
            <ArrowLeft className="h-4 w-4" strokeWidth={1.5} />
          </button>
        </HoverTip>
        {appId ? (
          <AppGlyph app={appId} size={16} badgeClassName="shrink-0 bg-app" />
        ) : null}
        <h1
          className="m-0 min-w-0 truncate text-section text-fg-1"
          title={title}
        >
          {title}
        </h1>
        <span className="sr-only">
          {t("sessionManager.sessionOf", {
            defaultValue: "{{app}} 的会话",
            app: appName,
          })}
        </span>
        {showStatusBadges && (
          <span className="inline-flex shrink-0 items-center gap-1.5">
            <SessionStatusBadges session={session} parentLabel={parentTitle} />
          </span>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-1.5">
        <div className="flex shrink-0 items-center">
          <HoverTip
            content={t("sessionManager.prevShort", {
              defaultValue: "上一个",
            })}
          >
            <button
              type="button"
              aria-label={t("sessionManager.prevSession", {
                defaultValue: "上一个会话",
              })}
              disabled={!hasPrev}
              onClick={onPrev}
              className={cn(readerIconButton, "h-8 w-8")}
            >
              <ChevronLeft className="h-4 w-4" strokeWidth={1.5} />
            </button>
          </HoverTip>
          <HoverTip
            content={t("sessionManager.nextShort", {
              defaultValue: "下一个",
            })}
          >
            <button
              type="button"
              aria-label={t("sessionManager.nextSession", {
                defaultValue: "下一个会话",
              })}
              disabled={!hasNext}
              onClick={onNext}
              className={cn(readerIconButton, "h-8 w-8")}
            >
              <ChevronRight className="h-4 w-4" strokeWidth={1.5} />
            </button>
          </HoverTip>
        </div>
        {resume}
        {moreMenu}
      </div>
    </header>
  );
});

// ─── 信息栏 + 阅读工具 ────────────────────────────────────────────────────

/** 「10月2日 15:25 → 17:33」+ 时长 */
const formatRange = (created?: number, last?: number) => {
  if (!created && !last) return { range: "", duration: null };
  if (!created || !last || created === last) {
    return { range: formatShortDateTime(last ?? created), duration: null };
  }
  const sameDay =
    new Date(created).toDateString() === new Date(last).toDateString();
  return {
    range: `${formatShortDateTime(created)} → ${
      sameDay ? formatClock(last) : formatShortDateTime(last)
    }`,
    duration: last > created ? formatDuration(last - created) : null,
  };
};

export interface FindState {
  open: boolean;
  query: string;
  current: number;
  total: number;
}

export interface SessionReaderToolbarProps {
  session: SessionMeta;
  failed: boolean;
  turns: SessionTurn[];
  /** 轮次索引（提问目录、次数统计） */
  turnIndex: TurnIndex[];
  messages: SessionMessage[];
  model?: string;
  progress: { loaded: number; total: number | null } | null;
  noResumeReason: string | null;
  filter: ReaderFilter;
  onFilterChange: (filter: ReaderFilter) => void;
  expandAll: boolean;
  onExpandAllChange: (value: boolean) => void;
  showInjected: boolean;
  onShowInjectedChange: (value: boolean) => void;
  outlineOpen: boolean;
  onToggleOutline: () => void;
  find: FindState;
  findInputRef: RefObject<HTMLInputElement>;
  findButtonRef: RefObject<HTMLButtonElement>;
  onOpenFind: () => void;
  onCloseFind: () => void;
  onFindQueryChange: (query: string) => void;
  onStepFind: (delta: number) => void;
  /** 导出为 Markdown 文件（和查找、显示选项放在同一行） */
  onExportMarkdown: () => void;
}

interface ComputedUsageStats {
  totalTokens: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  reasoningTokens: number;
  costUsd: number | null;
}

/** 直接从会话消息 meta 与 OpenCode stepCosts 汇总 Token 与花费 */
const computeMessageUsage = (
  messages: SessionMessage[],
  turns: SessionTurn[],
): ComputedUsageStats => {
  let inputTokens = 0;
  let outputTokens = 0;
  let cacheReadTokens = 0;
  let cacheWriteTokens = 0;
  let reasoningTokens = 0;
  let metaCostUsd = 0;
  let hasMetaCost = false;

  for (const msg of messages) {
    const meta = msg.meta;
    if (!meta) continue;
    inputTokens += meta.inputTokens ?? 0;
    outputTokens += meta.outputTokens ?? 0;
    cacheReadTokens += meta.cacheReadTokens ?? 0;
    cacheWriteTokens += meta.cacheWriteTokens ?? 0;
    reasoningTokens += meta.reasoningTokens ?? 0;
    if (typeof meta.costUsd === "number" && Number.isFinite(meta.costUsd)) {
      metaCostUsd += meta.costUsd;
      hasMetaCost = true;
    }
  }

  let stepTokens = 0;
  let stepCostUsd = 0;
  let hasStepCost = false;
  for (const turn of turns) {
    for (const cost of turn.stepCosts) {
      stepTokens += cost.tokens ?? 0;
      if (typeof cost.costUsd === "number" && Number.isFinite(cost.costUsd)) {
        stepCostUsd += cost.costUsd;
        hasStepCost = true;
      }
    }
  }

  const metaTotal =
    inputTokens +
    outputTokens +
    cacheReadTokens +
    cacheWriteTokens +
    reasoningTokens;
  const totalTokens = metaTotal > 0 ? metaTotal : stepTokens;
  const costUsd = hasMetaCost ? metaCostUsd : hasStepCost ? stepCostUsd : null;

  return {
    totalTokens,
    inputTokens,
    outputTokens,
    cacheReadTokens,
    reasoningTokens,
    costUsd,
  };
};

export const SessionReaderToolbar = memo(function SessionReaderToolbar({
  session,
  failed,
  turns,
  turnIndex,
  messages,
  model,
  progress,
  noResumeReason,
  filter,
  onFilterChange,
  expandAll,
  onExpandAllChange,
  showInjected,
  onShowInjectedChange,
  outlineOpen,
  onToggleOutline,
  find,
  findInputRef,
  findButtonRef,
  onOpenFind,
  onCloseFind,
  onFindQueryChange,
  onStepFind,
  onExportMarkdown,
}: SessionReaderToolbarProps) {
  const { t } = useTranslation();
  const rt = useReaderT();
  const { moneyCompact } = useMoneyFormat();
  const { range, duration } = formatRange(
    session.createdAt,
    session.lastActiveAt,
  );

  const messageCount = messages.length;
  const questionCount = useMemo(
    () =>
      turns.filter((turn) => Boolean(turn.question)).length || turnIndex.length,
    [turnIndex.length, turns],
  );
  const toolCount = useMemo(
    () => turnIndex.reduce((sum, item) => sum + item.stepCount, 0),
    [turnIndex],
  );
  const usage = useMemo(
    () => computeMessageUsage(messages, turns),
    [messages, turns],
  );
  const filterOptions = useMemo(
    () =>
      [
        { value: "all", label: rt("filterAll") },
        { value: "conversation", label: rt("filterChat") },
        { value: "changes", label: rt("filterChanges") },
      ] as const,
    [rt],
  );

  return (
    <div className="flex shrink-0 flex-col gap-1.5 border-b border-border py-2 pe-4 ps-6">
      <div className="flex min-h-8 flex-wrap items-center gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1 basis-72">
          {session.projectDir ? (
            <PathChip path={session.projectDir} className="max-w-full" />
          ) : (
            <span className="text-caption text-fg-2">
              {t("sessionManager.unknownDirectory", {
                defaultValue: "未知目录",
              })}
            </span>
          )}
        </div>

        {!failed && (
          <div className="flex shrink-0 items-center gap-0.5">
            <HoverTip content={rt("outlineToggle")}>
              <button
                type="button"
                aria-label={rt("outlineToggle")}
                aria-pressed={outlineOpen}
                onClick={onToggleOutline}
                className={cn(
                  readerIconButton,
                  "h-8 w-8 max-md:hidden",
                  outlineOpen && "bg-selected text-fg-1",
                )}
              >
                <PanelRight className="h-4 w-4" strokeWidth={1.5} />
              </button>
            </HoverTip>
            <SegmentedControl<ReaderFilter>
              size="compact"
              aria-label={rt("filterLabel")}
              value={filter}
              onValueChange={onFilterChange}
              options={filterOptions}
              className="mx-1"
            />
            <DropdownMenu>
              <HoverTip content={rt("viewOptions")}>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    aria-label={rt("viewOptions")}
                    className={cn(readerIconButton, "h-8 w-8")}
                  >
                    <SlidersHorizontal className="h-4 w-4" strokeWidth={1.5} />
                  </button>
                </DropdownMenuTrigger>
              </HoverTip>
              <DropdownMenuContent
                align="end"
                className="w-[220px] rounded-panel p-1 shadow-v7-md"
              >
                <DropdownMenuCheckboxItem
                  className={cn(readerMenuItemClass, "ps-8")}
                  checked={expandAll}
                  onCheckedChange={(value) => onExpandAllChange(value === true)}
                  onSelect={(event) => event.preventDefault()}
                >
                  {rt("expandAllTimelines")}
                </DropdownMenuCheckboxItem>
                <DropdownMenuCheckboxItem
                  className={cn(readerMenuItemClass, "ps-8")}
                  checked={showInjected}
                  onCheckedChange={(value) =>
                    onShowInjectedChange(value === true)
                  }
                  onSelect={(event) => event.preventDefault()}
                >
                  {rt("injectedToggle")}
                </DropdownMenuCheckboxItem>
              </DropdownMenuContent>
            </DropdownMenu>
            <HoverTip content={rt("exportMarkdown")}>
              <button
                type="button"
                aria-label={rt("exportMarkdown")}
                disabled={turns.length === 0}
                onClick={onExportMarkdown}
                className={cn(readerIconButton, "h-8 w-8")}
              >
                <Download className="h-4 w-4" strokeWidth={1.5} />
              </button>
            </HoverTip>
            {!find.open && (
              <HoverTip
                content={t("sessionManager.findShort", {
                  defaultValue: "查找",
                })}
              >
                <button
                  ref={findButtonRef}
                  type="button"
                  aria-label={t("sessionManager.findInSession", {
                    defaultValue: "在会话中查找",
                  })}
                  aria-expanded={false}
                  onClick={onOpenFind}
                  className={cn(readerIconButton, "h-8 w-8")}
                >
                  <Search className="h-4 w-4" strokeWidth={1.5} />
                </button>
              </HoverTip>
            )}
          </div>
        )}

        {find.open && !failed && (
          <div
            role="search"
            aria-label={t("sessionManager.findInSession", {
              defaultValue: "在会话中查找",
            })}
            className="flex shrink-0 items-center gap-0.5"
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.stopPropagation();
                onCloseFind();
              } else if (event.key === "Enter") {
                event.preventDefault();
                onStepFind(event.shiftKey ? -1 : 1);
              }
            }}
          >
            <div className="relative w-[156px]">
              <Search
                aria-hidden="true"
                className="pointer-events-none absolute start-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-fg-3"
                strokeWidth={1.5}
              />
              <input
                ref={findInputRef}
                type="text"
                value={find.query}
                onChange={(event) => onFindQueryChange(event.target.value)}
                aria-label={t("sessionManager.findLabel", {
                  defaultValue: "查找内容",
                })}
                placeholder={t("sessionManager.findShort", {
                  defaultValue: "查找",
                })}
                autoComplete="off"
                spellCheck={false}
                autoCorrect="off"
                autoCapitalize="off"
                className={findInputClass}
              />
            </div>
            <span
              role="status"
              className="min-w-12 text-center text-caption tabular-nums text-fg-2"
            >
              {find.query.trim() ? `${find.current} / ${find.total}` : ""}
            </span>
            <button
              type="button"
              aria-label={t("sessionManager.findPrev", {
                defaultValue: "上一个匹配",
              })}
              disabled={!find.total}
              onClick={() => onStepFind(-1)}
              className={readerIconButton}
            >
              <ArrowUp className="h-[15px] w-[15px]" strokeWidth={1.5} />
            </button>
            <button
              type="button"
              aria-label={t("sessionManager.findNext", {
                defaultValue: "下一个匹配",
              })}
              disabled={!find.total}
              onClick={() => onStepFind(1)}
              className={readerIconButton}
            >
              <ArrowDown className="h-[15px] w-[15px]" strokeWidth={1.5} />
            </button>
            <button
              type="button"
              aria-label={t("sessionManager.findClose", {
                defaultValue: "关闭查找",
              })}
              onClick={onCloseFind}
              className={readerIconButton}
            >
              <X className="h-3.5 w-3.5" strokeWidth={1.5} />
            </button>
          </div>
        )}
      </div>

      <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1 text-caption tabular-nums text-fg-2">
        {range && (
          <span className="inline-flex min-w-0 flex-wrap items-center gap-x-1.5">
            <span className="[overflow-wrap:anywhere]">{range}</span>
            {duration && (
              <span className="whitespace-nowrap">({duration})</span>
            )}
          </span>
        )}
        {!failed && progress && (
          <span role="status">
            {progress.total !== null
              ? rt("loadingProgress", {
                  loaded: progress.loaded,
                  total: progress.total,
                })
              : t("sessionManager.loadingMessages", {
                  defaultValue: "正在读取会话…",
                })}
          </span>
        )}
        {!failed && !progress && messageCount > 0 && (
          <span className="inline-flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="whitespace-nowrap">
              {rt("turnsCount", {
                turns: turnIndex.length || turns.length,
                messages: messageCount,
              })}
            </span>
            {questionCount > 0 && (
              <span className="whitespace-nowrap">
                {rt("questionsCount", { count: questionCount })}
              </span>
            )}
            {toolCount > 0 && (
              <span className="whitespace-nowrap">
                {rt("toolsCount", { count: toolCount })}
              </span>
            )}
          </span>
        )}
        {model && (
          <span className="min-w-0 font-mono [overflow-wrap:anywhere]">
            {model}
          </span>
        )}
        {!failed &&
          (usage.totalTokens > 0 ||
            (usage.costUsd !== null && usage.costUsd > 0)) && (
            <span
              className="inline-flex flex-wrap items-center gap-x-1.5 gap-y-1"
              title={
                usage.inputTokens + usage.outputTokens > 0
                  ? rt("usage.detail", {
                      input: formatTokens(usage.inputTokens),
                      output: formatTokens(usage.outputTokens),
                      cacheRead: formatTokens(usage.cacheReadTokens),
                      reasoning: formatTokens(usage.reasoningTokens),
                    })
                  : undefined
              }
            >
              {usage.totalTokens > 0 && (
                <span className="whitespace-nowrap">
                  {rt("usage.tokens", {
                    tokens: formatTokens(usage.totalTokens),
                  })}
                </span>
              )}
              {usage.costUsd !== null && usage.costUsd > 0 && (
                <span className="whitespace-nowrap">
                  {moneyCompact(usage.costUsd)}
                </span>
              )}
            </span>
          )}
      </div>

      {noResumeReason && (
        <p id="session-resume-note" className="m-0 mb-1 text-caption text-fg-2">
          {noResumeReason}
        </p>
      )}
    </div>
  );
});
