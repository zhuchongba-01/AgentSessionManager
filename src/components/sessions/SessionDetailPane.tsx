import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ChevronDown,
  ChevronUp,
  Clock,
  FileText,
  FolderOpen,
  Github,
  Hash,
  MessageSquare,
  Monitor,
  Moon,
  RefreshCw,
  Search,
  Sun,
  Trash2,
  Wrench,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { GITHUB_REPOSITORY_URL } from "@/lib/githubRelease";
import { cn } from "@/lib/utils";
import type { SessionMessage, SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import { AgentIcon } from "./AgentIcon";
import { EMPTY_HIGHLIGHT_TERMS } from "./highlightText";
import { SessionMessageItem } from "./SessionMessageItem";
import { SessionMinimap } from "./SessionMinimap";
import { SessionStatusBadges } from "./SessionStatusBadges";
import {
  buildUserMessageToc,
  formatMessageTimestamp,
  formatSessionTitle,
  getBaseName,
  getProviderIconName,
  getProviderLabel,
  resolveParentLabel,
  shouldShowSessionMinimap,
} from "./utils";

interface SessionDetailPaneProps {
  selectedSession: SessionMeta | null;
  selectedKey: string | null;
  isCodexSession: boolean;
  selectedSessionChildCount: number;
  sessionTitles: Map<string, string>;
  visibleMessages: SessionMessage[];
  isLoadingMessages: boolean;
  isFetchingMessages: boolean;
  messageRevision: number;
  messagesError: unknown;
  onRefetchMessages: () => void;
  onLoadFullContent: (message: SessionMessage) => Promise<string>;
  hasBatchSelection: boolean;
  selectedDeletableCount: number;
  isDeleting: boolean;
  onBatchDelete: () => void;
  onSingleDelete: (session: SessionMeta) => void;
  theme: "light" | "dark" | "system";
  resolvedTheme: "light" | "dark";
  onSetTheme: (theme: "light" | "dark" | "system") => void;
  onOpenUrl: (url: string) => void;
  onCopy: (text: string, successMessage: string) => void;
  onCopyMessage: (content: string) => void;
  onCopyCode: (code: string) => void;
}

export function SessionDetailPane({
  selectedSession,
  selectedKey,
  isCodexSession,
  selectedSessionChildCount,
  sessionTitles,
  visibleMessages,
  isLoadingMessages,
  isFetchingMessages,
  messageRevision,
  messagesError,
  onRefetchMessages,
  onLoadFullContent,
  hasBatchSelection,
  selectedDeletableCount,
  isDeleting,
  onBatchDelete,
  onSingleDelete,
  theme,
  resolvedTheme,
  onSetTheme,
  onOpenUrl,
  onCopy,
  onCopyMessage,
  onCopyCode,
}: SessionDetailPaneProps) {
  const { t } = useTranslation();
  const scrollContainerRef = useRef<HTMLDivElement | null>(null);
  const highlightTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const detailSearchInputRef = useRef<HTMLInputElement | null>(null);
  const [activeMessageIndex, setActiveMessageIndex] = useState<number | null>(
    null,
  );
  const [hideToolMessages, setHideToolMessages] = useState(false);
  const [detailSearchOpen, setDetailSearchOpen] = useState(false);
  const [detailSearchQuery, setDetailSearchQuery] = useState("");
  const [activeMatchCursor, setActiveMatchCursor] = useState(0);

  const toolMessageCount = useMemo(
    () =>
      visibleMessages.filter((message) => {
        const role = message.role.toLowerCase();
        return role !== "user" && role !== "assistant";
      }).length,
    [visibleMessages],
  );

  const displayedMessages = useMemo(
    () =>
      hideToolMessages
        ? visibleMessages.filter((message) => {
            const role = message.role.toLowerCase();
            return role === "user" || role === "assistant";
          })
        : visibleMessages,
    [hideToolMessages, visibleMessages],
  );

  const detailSearchTerms = useMemo(
    () =>
      detailSearchOpen
        ? detailSearchQuery
            .trim()
            .toLocaleLowerCase()
            .split(/\s+/)
            .filter(Boolean)
        : EMPTY_HIGHLIGHT_TERMS,
    [detailSearchOpen, detailSearchQuery],
  );

  const matchedMessageIndexes = useMemo(() => {
    if (detailSearchTerms.length === 0) return [];
    const matches: number[] = [];
    displayedMessages.forEach((message, index) => {
      const content = message.content.toLocaleLowerCase();
      if (detailSearchTerms.every((term) => content.includes(term))) {
        matches.push(index);
      }
    });
    return matches;
  }, [detailSearchTerms, displayedMessages]);

  const safeMatchCursor =
    matchedMessageIndexes.length > 0
      ? Math.min(activeMatchCursor, matchedMessageIndexes.length - 1)
      : 0;

  const virtualizer = useVirtualizer({
    count: displayedMessages.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: () => 120,
    overscan: 5,
    gap: 18,
  });

  const viewportStart = virtualizer.scrollOffset ?? 0;
  const viewportEnd =
    viewportStart + Math.max(0, virtualizer.scrollRect?.height ?? 0);
  const viewportMessageIndexes = virtualizer
    .getVirtualItems()
    .filter(
      (item) =>
        viewportEnd > viewportStart &&
        item.end > viewportStart &&
        item.start < viewportEnd,
    )
    .map((item) => item.index);
  const virtualizedContentHeight = virtualizer.getTotalSize();

  const clearHighlightTimer = useCallback(() => {
    if (highlightTimerRef.current !== null) {
      clearTimeout(highlightTimerRef.current);
      highlightTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    clearHighlightTimer();
    setActiveMessageIndex(null);
    setDetailSearchOpen(false);
    setDetailSearchQuery("");
    setActiveMatchCursor(0);
    if (scrollContainerRef.current) {
      scrollContainerRef.current.scrollTop = 0;
    }
  }, [selectedKey, clearHighlightTimer]);

  useEffect(() => clearHighlightTimer, [clearHighlightTimer]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (
        selectedSession &&
        (event.metaKey || event.ctrlKey) &&
        event.key.toLowerCase() === "f"
      ) {
        event.preventDefault();
        setDetailSearchOpen(true);
        detailSearchInputRef.current?.focus();
        detailSearchInputRef.current?.select();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [selectedSession]);

  useEffect(() => {
    if (!detailSearchOpen) return;
    const frame = requestAnimationFrame(() => {
      detailSearchInputRef.current?.focus();
      detailSearchInputRef.current?.select();
    });
    return () => cancelAnimationFrame(frame);
  }, [detailSearchOpen]);

  // 提取用户消息用于目录。下标空间是「可见消息」，与虚拟列表、缩略导航一致。
  const userMessagesToc = useMemo(
    () => buildUserMessageToc(displayedMessages, isCodexSession),
    [displayedMessages, isCodexSession],
  );
  const minimapMessageIndexes = useMemo(
    () => userMessagesToc.map((item) => item.index),
    [userMessagesToc],
  );

  const scrollToMessage = useCallback(
    (index: number) => {
      virtualizer.scrollToIndex(index, { align: "center", behavior: "smooth" });
      clearHighlightTimer();
      setActiveMessageIndex(index);
      highlightTimerRef.current = setTimeout(() => {
        setActiveMessageIndex(null);
        highlightTimerRef.current = null;
      }, 2000);
    },
    [virtualizer, clearHighlightTimer],
  );

  const navigateMatch = useCallback(
    (delta: number) => {
      if (matchedMessageIndexes.length === 0) return;
      const count = matchedMessageIndexes.length;
      const nextCursor = (safeMatchCursor + delta + count) % count;
      setActiveMatchCursor(nextCursor);
      scrollToMessage(matchedMessageIndexes[nextCursor]);
    },
    [matchedMessageIndexes, safeMatchCursor, scrollToMessage],
  );

  const closeDetailSearch = useCallback(() => {
    setDetailSearchOpen(false);
    setDetailSearchQuery("");
    setActiveMatchCursor(0);
  }, []);

  // 缩略导航的显示条件只看「有没有用户提问」：此前用过的像素高度、正文字数两个
  // 判据都让用户看到「有时候有、有时候没有」——判据越多，越容易出现该出现却没出现。
  const showSessionMinimap = shouldShowSessionMinimap(
    minimapMessageIndexes.length,
  );

  return (
    <div className="asm-detail">
      <div className="asm-detail-toolbar">
        {selectedSession ? (
          <>
            <AgentIcon
              icon={getProviderIconName(selectedSession.providerId)}
              name={getProviderLabel(selectedSession.providerId, t)}
              size={16}
            />
            <span className="asm-detail-provider">
              {getProviderLabel(selectedSession.providerId, t)}
            </span>
          </>
        ) : (
          <span className="asm-detail-provider">
            {t("sessionManager.sessionDetail", {
              defaultValue: "会话详情",
            })}
          </span>
        )}
        <span className="asm-toolbar-spacer" />
        {selectedSession && toolMessageCount > 0 && (
          <button
            type="button"
            className="asm-detail-tool"
            aria-pressed={hideToolMessages}
            onClick={() => {
              clearHighlightTimer();
              setActiveMessageIndex(null);
              setHideToolMessages((prev) => !prev);
            }}
          >
            <Wrench className="size-3.5" aria-hidden="true" />
            <span>
              {hideToolMessages
                ? t("sessionManager.showTools", {
                    defaultValue: "显示工具 ({{count}})",
                    count: toolMessageCount,
                  })
                : t("sessionManager.hideTools", {
                    defaultValue: "隐藏工具 ({{count}})",
                    count: toolMessageCount,
                  })}
            </span>
          </button>
        )}
        {selectedSession && (
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className={cn(
                  "asm-detail-icon-tool",
                  detailSearchOpen && "is-active",
                )}
                aria-label={t("sessionManager.searchInSession", {
                  defaultValue: "在当前会话中搜索",
                })}
                aria-pressed={detailSearchOpen}
                onClick={() => {
                  if (detailSearchOpen) {
                    closeDetailSearch();
                  } else {
                    setDetailSearchOpen(true);
                  }
                }}
              >
                <Search className="size-4" />
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.searchInSession", {
                defaultValue: "在当前会话中搜索",
              })}
            </TooltipContent>
          </Tooltip>
        )}
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className="asm-detail-tool"
              aria-label={t("sessionManager.openProjectGitHub", {
                defaultValue: "在 GitHub 查看项目",
              })}
              onClick={() => onOpenUrl(GITHUB_REPOSITORY_URL)}
            >
              <Github className="size-4" />
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
                "asm-detail-icon-tool",
                theme === "system" && "is-active",
              )}
              aria-label={t("sessionManager.followSystemTheme", {
                defaultValue: "跟随系统外观",
              })}
              aria-pressed={theme === "system"}
              onClick={() => onSetTheme("system")}
            >
              <Monitor className="size-4" />
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
              className="asm-detail-icon-tool"
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
                <Sun className="size-4" />
              ) : (
                <Moon className="size-4" />
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
      {!selectedSession ? (
        <div className="flex-1 flex flex-col items-center justify-center text-muted-foreground p-8">
          <MessageSquare className="size-12 mb-3 opacity-30" />
          <p className="text-sm">{t("sessionManager.selectSession")}</p>
        </div>
      ) : (
        <>
          {/* 详情头部 */}
          <div className="asm-detail-head">
            <div className="asm-detail-titlerow">
              <h2 className="asm-detail-title">
                {formatSessionTitle(selectedSession)}
              </h2>
              <SessionStatusBadges
                session={selectedSession}
                parentLabel={resolveParentLabel(selectedSession, sessionTitles)}
              />

              {/* 操作按钮组 */}
              <div className="asm-detail-actions">
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-8 gap-1.5 rounded-md px-3 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
                      // 批量管理模式下只要有选中项，这个按钮就必须和
                      // 「批量删除」同义：否则用户多选之后点这里，只会
                      // 删掉右侧当前打开的那一条（多选被静默忽略）。
                      onClick={() =>
                        hasBatchSelection
                          ? onBatchDelete()
                          : onSingleDelete(selectedSession)
                      }
                      disabled={
                        hasBatchSelection
                          ? isDeleting || selectedDeletableCount === 0
                          : !selectedSession.sourcePath || isDeleting
                      }
                    >
                      <Trash2 className="size-3.5" />
                      <span>
                        {isDeleting
                          ? t("sessionManager.deleting", {
                              defaultValue: "删除中...",
                            })
                          : hasBatchSelection
                            ? t("sessionManager.deleteSelectedCount", {
                                defaultValue: "删除已勾选的 {{count}} 条",
                                count: selectedDeletableCount,
                              })
                            : selectedSession.cleanupPending
                              ? t("sessionManager.continueCleanup", {
                                  defaultValue: "继续清理",
                                })
                              : selectedSessionChildCount > 0
                                ? t("sessionManager.deleteThisWithChildren", {
                                    defaultValue:
                                      "删除此会话及其 {{count}} 个子会话",
                                    count: selectedSessionChildCount,
                                  })
                                : t("sessionManager.deleteThisOnly", {
                                    defaultValue: "仅删除此会话",
                                  })}
                      </span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>
                    {hasBatchSelection
                      ? t("sessionManager.deleteSelectedTooltip", {
                          defaultValue:
                            "永久删除已勾选的 {{count}} 条本地会话记录",
                          count: selectedDeletableCount,
                        })
                      : selectedSessionChildCount > 0
                        ? t("sessionManager.deleteWithChildrenTooltip", {
                            defaultValue:
                              "永久删除右侧这一条会话及其 {{count}} 个子会话；范围可以在确认框里改",
                            count: selectedSessionChildCount,
                          })
                        : t("sessionManager.deleteTooltip", {
                            defaultValue: "仅永久删除右侧这一条会话记录",
                          })}
                  </TooltipContent>
                </Tooltip>
              </div>
            </div>

            {/* 时间与目录：大标题下方 */}
            <div className="asm-detail-meta">
              <span className="asm-chip">
                <Clock className="size-3" />
                {formatMessageTimestamp(
                  selectedSession.lastActiveAt ?? selectedSession.createdAt,
                )}
              </span>
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={() =>
                      onCopy(
                        selectedSession.sessionId,
                        t("sessionManager.sessionIdCopied", {
                          defaultValue: "已复制会话 ID",
                        }),
                      )
                    }
                    className="asm-chip asm-chip-mono"
                  >
                    <Hash className="size-3 shrink-0" />
                    <span className="asm-chip-truncate asm-chip-id">
                      {selectedSession.sessionId}
                    </span>
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom" className="max-w-xs">
                  <p className="font-mono text-xs break-all">
                    {selectedSession.sessionId}
                  </p>
                  <p className="text-muted-foreground mt-1">
                    {t("sessionManager.clickToCopySessionId", {
                      defaultValue: "点击复制会话 ID",
                    })}
                  </p>
                </TooltipContent>
              </Tooltip>
              {selectedSession.projectDir && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={() =>
                        onCopy(
                          selectedSession.projectDir!,
                          t("sessionManager.projectDirCopied", {
                            defaultValue: "目录已复制",
                          }),
                        )
                      }
                      className="asm-chip asm-chip-mono"
                    >
                      <FolderOpen className="size-3" />
                      <span className="asm-chip-truncate">
                        {selectedSession.projectName ||
                          getBaseName(selectedSession.projectDir)}
                      </span>
                    </button>
                  </TooltipTrigger>
                  <TooltipContent side="bottom" className="max-w-xs">
                    <p className="font-mono text-xs break-all">
                      {selectedSession.projectDir}
                    </p>
                    <p className="text-muted-foreground mt-1">
                      {t("sessionManager.clickToCopyPath")}
                    </p>
                  </TooltipContent>
                </Tooltip>
              )}
              {selectedSession.sourcePath && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={() =>
                        onCopy(
                          selectedSession.sourcePath!,
                          t("sessionManager.sourcePathCopied", {
                            defaultValue: "原始文件已复制",
                          }),
                        )
                      }
                      className="asm-chip asm-chip-mono asm-chip-source"
                    >
                      <FileText className="size-3 shrink-0" />
                      <span className="asm-chip-truncate">
                        {getBaseName(selectedSession.sourcePath)}
                      </span>
                    </button>
                  </TooltipTrigger>
                  <TooltipContent side="bottom" className="max-w-xs">
                    <p className="font-mono text-xs break-all">
                      {selectedSession.sourcePath}
                    </p>
                    <p className="text-muted-foreground mt-1">
                      {t("sessionManager.clickToCopyPath")}
                    </p>
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
          </div>

          {detailSearchOpen && (
            <div className="asm-detail-search-bar">
              <Search
                className="size-3.5 shrink-0 text-muted-foreground"
                aria-hidden="true"
              />
              <input
                ref={detailSearchInputRef}
                type="search"
                aria-label={t("sessionManager.searchInSession", {
                  defaultValue: "在当前会话中搜索",
                })}
                placeholder={t("sessionManager.searchInSessionPlaceholder", {
                  defaultValue: "在当前会话中搜索...",
                })}
                value={detailSearchQuery}
                onChange={(event) => {
                  setDetailSearchQuery(event.target.value);
                  setActiveMatchCursor(0);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    closeDetailSearch();
                    return;
                  }
                  if (
                    event.key === "Enter" &&
                    matchedMessageIndexes.length > 0
                  ) {
                    event.preventDefault();
                    if (event.shiftKey) {
                      navigateMatch(-1);
                    } else if (
                      activeMessageIndex ===
                      matchedMessageIndexes[safeMatchCursor]
                    ) {
                      navigateMatch(1);
                    } else {
                      scrollToMessage(matchedMessageIndexes[safeMatchCursor]);
                    }
                  }
                }}
              />
              {detailSearchTerms.length > 0 && (
                <span className="asm-detail-search-count">
                  {matchedMessageIndexes.length > 0
                    ? `${safeMatchCursor + 1} / ${matchedMessageIndexes.length}`
                    : "0 / 0"}
                </span>
              )}
              <button
                type="button"
                aria-label={t("sessionManager.prevMatch", {
                  defaultValue: "上一处匹配",
                })}
                disabled={matchedMessageIndexes.length === 0}
                onClick={() => navigateMatch(-1)}
              >
                <ChevronUp className="size-3.5" aria-hidden="true" />
              </button>
              <button
                type="button"
                aria-label={t("sessionManager.nextMatch", {
                  defaultValue: "下一处匹配",
                })}
                disabled={matchedMessageIndexes.length === 0}
                onClick={() => navigateMatch(1)}
              >
                <ChevronDown className="size-3.5" aria-hidden="true" />
              </button>
              <button
                type="button"
                aria-label={t("sessionManager.closeSessionSearch", {
                  defaultValue: "关闭会话内搜索",
                })}
                onClick={closeDetailSearch}
              >
                <X className="size-3.5" aria-hidden="true" />
              </button>
            </div>
          )}

          {/* 消息列表 + 目录 */}
          <div className="asm-detail-content">
            {showSessionMinimap && (
              <SessionMinimap
                messages={displayedMessages}
                messageIndexes={minimapMessageIndexes}
                visibleMessageIndexes={viewportMessageIndexes}
                isCodexSession={isCodexSession}
                onTickClick={scrollToMessage}
              />
            )}
            <div ref={scrollContainerRef} className="asm-detail-scroll">
              {(isLoadingMessages || isFetchingMessages) &&
              displayedMessages.length === 0 ? (
                <div role="status" className="asm-detail-skeleton">
                  <div className="asm-skeleton-caption">
                    <RefreshCw className="size-3.5 animate-spin" />
                    <span>
                      {t("sessionManager.loadingMessages", {
                        defaultValue: "正在读取会话…",
                      })}
                    </span>
                  </div>
                  <div className="asm-skeleton-turn is-user">
                    <div className="asm-skeleton-bubble w-48" />
                  </div>
                  <div className="asm-skeleton-turn">
                    <div className="asm-skeleton-line w-3/4" />
                    <div className="asm-skeleton-line w-full" />
                    <div className="asm-skeleton-line w-5/6" />
                  </div>
                  <div className="asm-skeleton-turn is-user">
                    <div className="asm-skeleton-bubble w-64" />
                  </div>
                  <div className="asm-skeleton-turn">
                    <div className="asm-skeleton-line w-2/3" />
                    <div className="asm-skeleton-line w-11/12" />
                  </div>
                </div>
              ) : messagesError ? (
                <div role="alert" className="asm-error-panel m-6">
                  <p>
                    {t("sessionManager.messagesLoadFailed", {
                      defaultValue: "会话读取失败，不能据此判断会话为空。",
                    })}
                  </p>
                  <p className="mt-2 break-all">
                    {extractErrorMessage(messagesError)}
                  </p>
                  <Button
                    variant="outline"
                    size="sm"
                    className="mt-3"
                    onClick={onRefetchMessages}
                  >
                    {t("sessionManager.retryLoadMessages", {
                      defaultValue: "重试读取",
                    })}
                  </Button>
                </div>
              ) : displayedMessages.length === 0 ? (
                <div className="flex flex-col items-center justify-center py-12 text-center">
                  <MessageSquare className="size-8 text-muted-foreground/50 mb-2" />
                  <p className="text-sm text-muted-foreground">
                    {t("sessionManager.emptySession")}
                  </p>
                </div>
              ) : (
                <div className="asm-messages">
                  {isFetchingMessages && (
                    <div
                      role="status"
                      className="flex items-center gap-2 py-2 text-xs text-muted-foreground"
                    >
                      <RefreshCw className="size-3.5 animate-spin" />
                      {t("sessionManager.loadingMoreMessages", {
                        defaultValue: "正在继续读取会话…",
                      })}
                    </div>
                  )}
                  <div
                    style={{
                      height: virtualizedContentHeight,
                      position: "relative",
                    }}
                  >
                    {virtualizer.getVirtualItems().map((virtualRow) => (
                      <div
                        key={`${selectedKey}:${virtualRow.key}`}
                        data-index={virtualRow.index}
                        ref={virtualizer.measureElement}
                        style={{
                          position: "absolute",
                          top: 0,
                          left: 0,
                          width: "100%",
                          transform: `translateY(${virtualRow.start}px)`,
                        }}
                      >
                        <SessionMessageItem
                          message={displayedMessages[virtualRow.index]}
                          messageRevision={messageRevision}
                          isCodexSession={isCodexSession}
                          highlightTerms={detailSearchTerms}
                          onCopy={onCopyMessage}
                          onCopyCode={onCopyCode}
                          onLoadFullContent={onLoadFullContent}
                          isActive={activeMessageIndex === virtualRow.index}
                        />
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        </>
      )}
    </div>
  );
}
