import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type UIEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  ArrowDown,
  ChevronDown,
  ChevronUp,
  ChevronsDownUp,
  ChevronsUpDown,
  Clock,
  Copy,
  Download,
  ExternalLink,
  FileText,
  FolderOpen,
  Hash,
  MessageSquare,
  PanelRight,
  RefreshCw,
  Search,
  Terminal,
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
import { isMac } from "@/lib/platform";
import { sessionsApi } from "@/lib/api/sessions";
import { cn } from "@/lib/utils";
import type { SessionMessage, SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import { AgentIcon } from "./AgentIcon";
import { getAgentReaderStyle } from "./agentReaderStyles";
import {
  downloadMarkdownFile,
  sanitizeMarkdownFilename,
  sessionToMarkdown,
  turnToMarkdown,
} from "./exportMarkdown";
import { EMPTY_HIGHLIGHT_TERMS } from "./highlightText";
import {
  ReaderAgentLane,
  ReaderChapterRow,
  ReaderEventRow,
  ReaderFinalReplyRow,
  ReaderInjectedRow,
  ReaderQuestionRow,
  ReaderStepRow,
  ReaderTimelineRow,
} from "./ReaderTurnRows";
import { SessionMinimap } from "./SessionMinimap";
import { SessionOutline } from "./SessionOutline";
import { SessionStatusBadges } from "./SessionStatusBadges";
import {
  buildReaderLayout,
  groupMessagesIntoTurns,
  type SessionTurnGroup,
} from "./turnGrouping";
import type { SessionReaderViewMode } from "./types";
import {
  buildResumeShellCommand,
  buildUserMessageToc,
  extractRevealableLocalPath,
  formatMessageTimestamp,
  formatSessionTitle,
  getBaseName,
  getProviderIconName,
  getProviderLabel,
  resolveParentLabel,
  shouldShowSessionMinimap,
} from "./utils";

const OUTLINE_OPEN_STORAGE_KEY = "agent-session-manager.outlineOpen";
const INCLUDE_THINKING_STORAGE_KEY = "agent-session-manager.includeThinking";

function readInitialOutlineOpen(): boolean {
  try {
    return window.localStorage.getItem(OUTLINE_OPEN_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

function readInitialIncludeThinking(): boolean {
  try {
    return window.localStorage.getItem(INCLUDE_THINKING_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

export interface PendingSnippetJump {
  sessionKey: string;
  transcriptIndex: number;
  query: string;
  nonce: number;
}

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
  onCopy: (text: string, successMessage: string) => void;
  onCopyMessage: (content: string) => void;
  onCopyCode: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onRevealPath?: (path: string) => void;
  onNotifyExport?: (filename: string) => void;
  pendingJumpTarget?: PendingSnippetJump | null;
  onConsumeJumpTarget?: () => void;
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
  onCopy,
  onCopyMessage,
  onCopyCode,
  onOpenUrl,
  onRevealPath,
  onNotifyExport,
  pendingJumpTarget,
  onConsumeJumpTarget,
}: SessionDetailPaneProps) {
  const { t } = useTranslation();
  const scrollContainerRef = useRef<HTMLDivElement | null>(null);
  const highlightTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const detailSearchInputRef = useRef<HTMLInputElement | null>(null);
  const navigatedCursorRef = useRef<number | null>(null);
  const [activeMessageIndex, setActiveMessageIndex] = useState<number | null>(
    null,
  );
  const [hideToolMessages, setHideToolMessages] = useState(false);
  const [readerViewMode, setReaderViewMode] =
    useState<SessionReaderViewMode>("all");
  const [expandAllTimelines, setExpandAllTimelines] = useState(false);
  const [expandedTurnKeys, setExpandedTurnKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [collapsedTurnKeys, setCollapsedTurnKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [stepOverrides, setStepOverrides] = useState<Map<string, boolean>>(
    () => new Map(),
  );
  const [includeThinking, setIncludeThinking] = useState(
    readInitialIncludeThinking,
  );
  const [detailSearchOpen, setDetailSearchOpen] = useState(false);
  const [detailSearchQuery, setDetailSearchQuery] = useState("");
  const [activeMatchCursor, setActiveMatchCursor] = useState(0);
  const [outlineOpen, setOutlineOpen] = useState(readInitialOutlineOpen);
  const [atBottom, setAtBottom] = useState(true);

  const handleToggleOutline = useCallback(() => {
    setOutlineOpen((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(OUTLINE_OPEN_STORAGE_KEY, String(next));
      } catch {
        // ignore storage quota/privacy mode
      }
      return next;
    });
  }, []);

  const handleToggleIncludeThinking = useCallback(() => {
    setIncludeThinking((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(INCLUDE_THINKING_STORAGE_KEY, String(next));
      } catch {
        // ignore
      }
      return next;
    });
  }, []);

  const handleToggleStep = useCallback(
    (stepId: string, currentlyExpanded: boolean) => {
      setStepOverrides((prev) => {
        const next = new Map(prev);
        next.set(stepId, !currentlyExpanded);
        return next;
      });
    },
    [],
  );

  const sessionScopeKey = `${selectedKey ?? ""}\u0000${messageRevision}`;
  const currentSessionScopeRef = useRef(sessionScopeKey);
  currentSessionScopeRef.current = sessionScopeKey;

  const [expandedContentState, setExpandedContentState] = useState<{
    scope: string;
    contents: Map<number, string>;
  }>(() => ({ scope: sessionScopeKey, contents: new Map() }));

  const expandedContents = useMemo(
    () =>
      expandedContentState.scope === sessionScopeKey
        ? expandedContentState.contents
        : new Map<number, string>(),
    [expandedContentState, sessionScopeKey],
  );

  const handleLoadMessageFullContent = useCallback(
    async (message: SessionMessage): Promise<string> => {
      const scopeAtRequest = sessionScopeKey;
      const fullText = await onLoadFullContent(message);
      const tIdx = message.transcriptIndex ?? message.index;
      if (
        tIdx !== undefined &&
        currentSessionScopeRef.current === scopeAtRequest
      ) {
        setExpandedContentState((prev) => {
          const base =
            prev.scope === scopeAtRequest
              ? prev.contents
              : new Map<number, string>();
          if (base.get(tIdx) === fullText) {
            return prev.scope === scopeAtRequest
              ? prev
              : { scope: scopeAtRequest, contents: base };
          }
          const next = new Map(base);
          next.set(tIdx, fullText);
          return { scope: scopeAtRequest, contents: next };
        });
      }
      return fullText;
    },
    [onLoadFullContent, sessionScopeKey],
  );

  const toolMessageCount = useMemo(
    () =>
      visibleMessages.filter((message) => {
        const role = message.role.toLowerCase();
        return role !== "user" && role !== "assistant" && role !== "thinking";
      }).length,
    [visibleMessages],
  );

  const displayedMessages = useMemo(() => {
    if (!hideToolMessages) return visibleMessages;
    const filtered: SessionMessage[] = [];
    for (const message of visibleMessages) {
      const role = message.role.toLowerCase();
      if (role !== "user" && role !== "assistant" && role !== "thinking") {
        continue;
      }
      if (role === "assistant" && message.meta?.blocks?.length) {
        const nonToolBlocks = message.meta.blocks.filter(
          (b) => b.type !== "tool_call" && b.type !== "tool_result",
        );
        if (nonToolBlocks.length === 0) continue;
        if (nonToolBlocks.length !== message.meta.blocks.length) {
          filtered.push({
            ...message,
            meta: { ...message.meta, blocks: nonToolBlocks },
          });
          continue;
        }
      }
      filtered.push(message);
    }
    return filtered;
  }, [hideToolMessages, visibleMessages]);

  const fullTurns = useMemo(
    () => groupMessagesIntoTurns(visibleMessages, selectedSession?.providerId),
    [selectedSession?.providerId, visibleMessages],
  );

  const displayedTurns = useMemo(
    () =>
      groupMessagesIntoTurns(displayedMessages, selectedSession?.providerId),
    [displayedMessages, selectedSession?.providerId],
  );

  const hasAnyThinking = useMemo(
    () => fullTurns.some((turn) => turn.thinkingCount > 0),
    [fullTurns],
  );

  const hasAnyChanges = useMemo(
    () => fullTurns.some((turn) => turn.hasChanges || turn.errorCount > 0),
    [fullTurns],
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

  const hasTruncatedMessages = useMemo(
    () => displayedMessages.some((message) => Boolean(message.truncated)),
    [displayedMessages],
  );

  const searchRequestScopeKey = `${sessionScopeKey}\u0000${detailSearchTerms.join(" ")}`;
  const [serverSearchState, setServerSearchState] = useState<{
    key: string;
    indexes: Set<number>;
  } | null>(null);

  const providerId = selectedSession?.providerId;
  const sourcePath = selectedSession?.sourcePath;

  useEffect(() => {
    if (
      detailSearchTerms.length === 0 ||
      !hasTruncatedMessages ||
      !providerId ||
      !sourcePath
    ) {
      return;
    }
    let cancelled = false;
    void sessionsApi
      .searchMessages(providerId, sourcePath, detailSearchQuery)
      .then((indices) => {
        if (cancelled || !indices) return;
        setServerSearchState({
          key: searchRequestScopeKey,
          indexes: new Set(indices),
        });
      });
    return () => {
      cancelled = true;
    };
  }, [
    detailSearchQuery,
    detailSearchTerms.length,
    hasTruncatedMessages,
    providerId,
    searchRequestScopeKey,
    sourcePath,
  ]);

  const activeServerMatches =
    serverSearchState?.key === searchRequestScopeKey
      ? serverSearchState.indexes
      : null;

  const matchedMessageIndexes = useMemo(() => {
    if (detailSearchTerms.length === 0) return [];
    const matches: number[] = [];
    displayedMessages.forEach((message, index) => {
      const tIdx = message.transcriptIndex ?? message.index;
      const candidateText =
        (tIdx !== undefined ? expandedContents.get(tIdx) : undefined) ??
        message.content;
      const content = candidateText.toLocaleLowerCase();
      const localMatch = detailSearchTerms.every((term) =>
        content.includes(term),
      );
      const serverMatch =
        tIdx !== undefined && Boolean(activeServerMatches?.has(tIdx));
      if (localMatch || serverMatch) {
        matches.push(index);
      }
    });
    return matches;
  }, [
    activeServerMatches,
    detailSearchTerms,
    displayedMessages,
    expandedContents,
  ]);

  const matchedSet = useMemo(
    () => new Set(matchedMessageIndexes),
    [matchedMessageIndexes],
  );

  const safeMatchCursor =
    matchedMessageIndexes.length > 0
      ? Math.min(activeMatchCursor, matchedMessageIndexes.length - 1)
      : 0;

  const readerLayout = useMemo(
    () =>
      buildReaderLayout({
        messages: displayedMessages,
        turns: displayedTurns,
        viewMode: readerViewMode,
        expandAllTimelines,
        expandedTurnKeys,
        collapsedTurnKeys,
        stepOverrides,
        matchedMessageSet: matchedSet,
      }),
    [
      collapsedTurnKeys,
      displayedMessages,
      displayedTurns,
      expandAllTimelines,
      expandedTurnKeys,
      matchedSet,
      readerViewMode,
      stepOverrides,
    ],
  );

  const virtualizer = useVirtualizer({
    count: readerLayout.items.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: (index) => {
      const kind = readerLayout.items[index]?.kind;
      if (kind === "divider") return 29;
      if (kind === "event" || kind === "injected") return 30;
      if (kind === "chapter") return 38;
      if (kind === "timeline" || kind === "step") return 34;
      if (kind === "question") return 76;
      return 140;
    },
    initialRect: { width: 800, height: 600 },
    getItemKey: (index) =>
      `${selectedKey}:${messageRevision}:${readerLayout.items[index]?.key ?? index}`,
    overscan: 6,
    gap: 4,
  });

  const virtualItems = virtualizer.getVirtualItems();
  const viewportStart = virtualizer.scrollOffset ?? 0;
  const viewportEnd =
    viewportStart + Math.max(0, virtualizer.scrollRect?.height ?? 0);
  const viewportMessageIndexes = useMemo(() => {
    const visible: number[] = [];
    for (const item of virtualItems) {
      if (
        !(
          viewportEnd > viewportStart &&
          item.end > viewportStart &&
          item.start < viewportEnd
        )
      ) {
        continue;
      }
      const row = readerLayout.items[item.index];
      if (!row) continue;
      if (
        row.kind === "question" ||
        row.kind === "step" ||
        row.kind === "final" ||
        row.kind === "event" ||
        row.kind === "injected"
      ) {
        visible.push(row.messageIndex);
      } else if (row.kind === "chapter") {
        visible.push(row.chapter.messageIndex);
      } else if (row.kind === "timeline") {
        visible.push(...row.turn.stepIndexes);
      }
    }
    return visible;
  }, [readerLayout.items, viewportEnd, viewportStart, virtualItems]);

  const activeTurnIndex = useMemo(() => {
    if (displayedTurns.length === 0) return 0;
    if (activeMessageIndex !== null) {
      const hitTurn = displayedTurns.find((turn) =>
        turn.messageIndexes.includes(activeMessageIndex),
      );
      if (hitTurn) return hitTurn.turnIndex;
    }
    for (const item of virtualItems) {
      if (
        viewportEnd > viewportStart &&
        item.end > viewportStart &&
        item.start < viewportEnd
      ) {
        const row = readerLayout.items[item.index];
        if (row) return row.turnIndex;
      }
    }
    return displayedTurns[0]?.turnIndex ?? 0;
  }, [
    activeMessageIndex,
    displayedTurns,
    readerLayout.items,
    viewportEnd,
    viewportStart,
    virtualItems,
  ]);
  const virtualizedContentHeight = virtualizer.getTotalSize();

  const clearHighlightTimer = useCallback(() => {
    if (highlightTimerRef.current !== null) {
      clearTimeout(highlightTimerRef.current);
      highlightTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    clearHighlightTimer();
    navigatedCursorRef.current = null;
    setActiveMessageIndex(null);
    setExpandedTurnKeys(new Set());
    setCollapsedTurnKeys(new Set());
    setStepOverrides(new Map());
    setReaderViewMode("all");
    setDetailSearchOpen(false);
    setDetailSearchQuery("");
    setActiveMatchCursor(0);
    setAtBottom(true);
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

  // 提取用户消息用于目录。下标空间是「可见消息」，与缩略导航一致。
  const userMessagesToc = useMemo(
    () => buildUserMessageToc(displayedMessages, isCodexSession),
    [displayedMessages, isCodexSession],
  );
  const minimapMessageIndexes = useMemo(
    () => userMessagesToc.map((item) => item.index),
    [userMessagesToc],
  );

  const scrollToMessage = useCallback(
    (displayedIndex: number) => {
      const targetRow =
        readerLayout.messageIndexToRow.get(displayedIndex) ?? displayedIndex;
      const rowItem = readerLayout.items[targetRow];
      if (rowItem?.kind === "timeline" && !rowItem.expanded) {
        const keyToOpen = rowItem.turnKey;
        setCollapsedTurnKeys((prev) => {
          if (!prev.has(keyToOpen)) return prev;
          const next = new Set(prev);
          next.delete(keyToOpen);
          return next;
        });
        setExpandedTurnKeys((prev) => {
          const next = new Set(prev);
          next.add(keyToOpen);
          return next;
        });
      }
      virtualizer.scrollToIndex(targetRow, {
        align: "center",
        behavior: "auto",
      });
      clearHighlightTimer();
      setActiveMessageIndex(displayedIndex);
      highlightTimerRef.current = setTimeout(() => {
        setActiveMessageIndex(null);
        highlightTimerRef.current = null;
      }, 2000);
    },
    [
      clearHighlightTimer,
      readerLayout.items,
      readerLayout.messageIndexToRow,
      virtualizer,
    ],
  );

  // 跨会话正文搜索点击摘录后跳转到对应消息
  useEffect(() => {
    if (!pendingJumpTarget || pendingJumpTarget.sessionKey !== selectedKey) {
      return;
    }
    if (pendingJumpTarget.query) {
      setDetailSearchOpen(true);
      setDetailSearchQuery(pendingJumpTarget.query);
    }
    if (hideToolMessages) {
      setHideToolMessages(false);
      return;
    }
    if (readerViewMode !== "all") {
      setReaderViewMode("all");
      return;
    }
    const targetDisplayedIdx = displayedMessages.findIndex(
      (msg, idx) =>
        (msg.transcriptIndex ?? msg.index ?? idx) ===
        pendingJumpTarget.transcriptIndex,
    );
    if (targetDisplayedIdx >= 0) {
      scrollToMessage(targetDisplayedIdx);
      onConsumeJumpTarget?.();
    } else if (!isLoadingMessages && !isFetchingMessages) {
      onConsumeJumpTarget?.();
    }
  }, [
    displayedMessages,
    hideToolMessages,
    isFetchingMessages,
    isLoadingMessages,
    onConsumeJumpTarget,
    pendingJumpTarget,
    readerViewMode,
    scrollToMessage,
    selectedKey,
  ]);

  const navigateMatch = useCallback(
    (delta: number) => {
      if (matchedMessageIndexes.length === 0) return;
      const count = matchedMessageIndexes.length;
      const nextCursor = (safeMatchCursor + delta + count) % count;
      navigatedCursorRef.current = nextCursor;
      setActiveMatchCursor(nextCursor);
      scrollToMessage(matchedMessageIndexes[nextCursor]);
    },
    [matchedMessageIndexes, safeMatchCursor, scrollToMessage],
  );

  const closeDetailSearch = useCallback(() => {
    navigatedCursorRef.current = null;
    setDetailSearchOpen(false);
    setDetailSearchQuery("");
    setActiveMatchCursor(0);
  }, []);

  useEffect(() => {
    if (!detailSearchOpen) return;
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (document.querySelector('[role="dialog"], [role="listbox"]')) {
        return;
      }
      const target = event.target as HTMLElement | null;
      if (
        target &&
        target !== detailSearchInputRef.current &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT" ||
          target.isContentEditable)
      ) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      closeDetailSearch();
    };
    window.addEventListener("keydown", handleEscape, true);
    return () => window.removeEventListener("keydown", handleEscape, true);
  }, [closeDetailSearch, detailSearchOpen]);

  const toggleTurnTimeline = useCallback(
    (turn: SessionTurnGroup, currentlyExpanded: boolean) => {
      if (currentlyExpanded) {
        setExpandedTurnKeys((prev) => {
          if (!prev.has(turn.turnKey)) return prev;
          const next = new Set(prev);
          next.delete(turn.turnKey);
          return next;
        });
        setCollapsedTurnKeys((prev) => {
          const next = new Set(prev);
          next.add(turn.turnKey);
          return next;
        });
      } else {
        setCollapsedTurnKeys((prev) => {
          if (!prev.has(turn.turnKey)) return prev;
          const next = new Set(prev);
          next.delete(turn.turnKey);
          return next;
        });
        setExpandedTurnKeys((prev) => {
          const next = new Set(prev);
          next.add(turn.turnKey);
          return next;
        });
      }
    },
    [],
  );

  const handleToggleExpandAllTimelines = useCallback(() => {
    setExpandedTurnKeys(new Set());
    setCollapsedTurnKeys(new Set());
    setStepOverrides(new Map());
    setExpandAllTimelines((prev) => !prev);
  }, []);

  const handleCopyMarkdown = useCallback(() => {
    if (!selectedSession || visibleMessages.length === 0) return;
    const providerLabel = getProviderLabel(selectedSession.providerId, t);
    const md = sessionToMarkdown(fullTurns, visibleMessages, {
      session: selectedSession,
      providerLabel,
      isCodexSession,
      includeThinking,
      expandedContents,
    });
    onCopy(
      md,
      t("sessionManager.markdownCopied", {
        defaultValue: "已复制会话 Markdown",
      }),
    );
  }, [
    expandedContents,
    fullTurns,
    includeThinking,
    isCodexSession,
    onCopy,
    selectedSession,
    t,
    visibleMessages,
  ]);

  const handleExportMarkdown = useCallback(() => {
    if (!selectedSession || visibleMessages.length === 0) return;
    const providerLabel = getProviderLabel(selectedSession.providerId, t);
    const md = sessionToMarkdown(fullTurns, visibleMessages, {
      session: selectedSession,
      providerLabel,
      isCodexSession,
      includeThinking: true,
      expandedContents,
    });
    const filename = sanitizeMarkdownFilename(
      formatSessionTitle(selectedSession),
      selectedSession.sessionId,
    );
    downloadMarkdownFile(filename, md);
    onNotifyExport?.(filename);
  }, [
    expandedContents,
    fullTurns,
    isCodexSession,
    onNotifyExport,
    selectedSession,
    t,
    visibleMessages,
  ]);

  const canResume = Boolean(
    selectedSession?.resumeCommand &&
      !selectedSession.residual &&
      !selectedSession.cleanupPending,
  );
  const fullResumeCommand = useMemo(
    () =>
      selectedSession?.resumeCommand
        ? buildResumeShellCommand(
            selectedSession.resumeCommand,
            selectedSession.projectDir,
          )
        : "",
    [selectedSession?.projectDir, selectedSession?.resumeCommand],
  );

  const revealableProjectDir = useMemo(
    () => extractRevealableLocalPath(selectedSession?.projectDir),
    [selectedSession?.projectDir],
  );
  const revealableSourcePath = useMemo(
    () => extractRevealableLocalPath(selectedSession?.sourcePath),
    [selectedSession?.sourcePath],
  );

  const showSessionMinimap = shouldShowSessionMinimap(
    minimapMessageIndexes.length,
  );

  const readerStyle = useMemo(
    () => getAgentReaderStyle(selectedSession?.providerId),
    [selectedSession?.providerId],
  );

  const selectedProviderLabel = useMemo(
    () =>
      selectedSession ? getProviderLabel(selectedSession.providerId, t) : "",
    [selectedSession, t],
  );

  const turnsByIndex = useMemo(() => {
    const map = new Map<number, SessionTurnGroup>();
    for (const turn of displayedTurns) {
      map.set(turn.turnIndex, turn);
    }
    return map;
  }, [displayedTurns]);

  const scrollToTurnReply = useCallback(
    (turnIndex: number) => {
      const targetRow =
        readerLayout.turnIndexToReplyRow.get(turnIndex) ??
        readerLayout.turnIndexToQuestionRow.get(turnIndex);
      if (targetRow === undefined) return;
      virtualizer.scrollToIndex(targetRow, {
        align: "start",
        behavior: "auto",
      });
      const row = readerLayout.items[targetRow];
      const msgIdx =
        row &&
        (row.kind === "question" ||
          row.kind === "step" ||
          row.kind === "final" ||
          row.kind === "event" ||
          row.kind === "injected")
          ? row.messageIndex
          : row?.kind === "chapter"
            ? row.chapter.messageIndex
            : row?.kind === "timeline"
              ? (row.turn.finalAssistantIndex ??
                row.turn.stepIndexes[0] ??
                null)
              : null;
      if (msgIdx !== null) {
        clearHighlightTimer();
        setActiveMessageIndex(msgIdx);
        highlightTimerRef.current = setTimeout(() => {
          setActiveMessageIndex(null);
          highlightTimerRef.current = null;
        }, 2000);
      }
    },
    [
      clearHighlightTimer,
      readerLayout.items,
      readerLayout.turnIndexToQuestionRow,
      readerLayout.turnIndexToReplyRow,
      virtualizer,
    ],
  );

  const handleCopySingleTurn = useCallback(
    (turn: SessionTurnGroup) => {
      if (!selectedSession) return;
      const md = turnToMarkdown(turn, displayedMessages, {
        session: selectedSession,
        providerLabel: selectedProviderLabel,
        isCodexSession,
        includeThinking,
        expandedContents,
      });
      if (!md) return;
      onCopy(
        md,
        t("sessionManager.turnCopied", {
          defaultValue: "已复制本轮对话",
        }),
      );
    },
    [
      displayedMessages,
      expandedContents,
      includeThinking,
      isCodexSession,
      onCopy,
      selectedProviderLabel,
      selectedSession,
      t,
    ],
  );

  const handleScrollContainerScroll = useCallback(
    (event: UIEvent<HTMLDivElement>) => {
      const el = event.currentTarget;
      if (el.scrollHeight <= el.clientHeight + 32) {
        setAtBottom(true);
        return;
      }
      const nextAtBottom =
        el.scrollTop + el.clientHeight >= el.scrollHeight - 32;
      setAtBottom((prev) => (prev === nextAtBottom ? prev : nextAtBottom));
    },
    [],
  );

  const handleJumpToLatest = useCallback(() => {
    if (readerLayout.items.length === 0) return;
    virtualizer.scrollToIndex(readerLayout.items.length - 1, {
      align: "end",
      behavior: "auto",
    });
    const el = scrollContainerRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
    setAtBottom(true);
  }, [readerLayout.items.length, virtualizer]);

  return (
    <div
      className="asm-detail"
      style={
        {
          "--reader-accent": readerStyle.accentColor,
        } as CSSProperties
      }
    >
      <div className="asm-detail-toolbar">
        {selectedSession ? (
          <div className="asm-detail-breadcrumb">
            <AgentIcon
              icon={getProviderIconName(selectedSession.providerId)}
              name={getProviderLabel(selectedSession.providerId, t)}
              size={16}
            />
            <span className="asm-detail-provider">
              {getProviderLabel(selectedSession.providerId, t)}
            </span>
            {(selectedSession.projectName || selectedSession.projectDir) && (
              <>
                <span className="asm-detail-crumb-sep" aria-hidden="true">
                  /
                </span>
                <span className="asm-detail-crumb-dir">
                  {selectedSession.projectName ||
                    getBaseName(selectedSession.projectDir)}
                </span>
              </>
            )}
          </div>
        ) : (
          <span className="asm-detail-provider">
            {t("sessionManager.sessionDetail", {
              defaultValue: "会话详情",
            })}
          </span>
        )}
        <span className="asm-toolbar-spacer" />
        {selectedSession && visibleMessages.length > 0 && (
          <div
            className="asm-reader-view-switch"
            role="group"
            aria-label="阅读视图筛选"
          >
            <button
              type="button"
              className="asm-list-view-option"
              aria-pressed={readerViewMode === "all"}
              onClick={() => setReaderViewMode("all")}
            >
              {t("sessionManager.readerViewAll", { defaultValue: "全部" })}
            </button>
            <button
              type="button"
              className="asm-list-view-option"
              aria-pressed={readerViewMode === "dialogue"}
              onClick={() => setReaderViewMode("dialogue")}
            >
              {t("sessionManager.readerViewDialogue", { defaultValue: "对话" })}
            </button>
            {hasAnyChanges && (
              <button
                type="button"
                className="asm-list-view-option"
                aria-pressed={readerViewMode === "changes"}
                onClick={() => setReaderViewMode("changes")}
              >
                {t("sessionManager.readerViewChanges", {
                  defaultValue: "改动",
                })}
              </button>
            )}
          </div>
        )}
        {selectedSession &&
          readerViewMode === "all" &&
          readerLayout.collapsibleTurnCount > 0 && (
            <button
              type="button"
              className="asm-detail-tool"
              aria-pressed={expandAllTimelines}
              onClick={handleToggleExpandAllTimelines}
            >
              {expandAllTimelines ? (
                <ChevronsDownUp className="size-3.5" aria-hidden="true" />
              ) : (
                <ChevronsUpDown className="size-3.5" aria-hidden="true" />
              )}
              <span>
                {expandAllTimelines
                  ? t("sessionManager.collapseAllSteps", {
                      defaultValue: "收起过程",
                    })
                  : t("sessionManager.expandAllSteps", {
                      defaultValue: "展开过程",
                    })}
              </span>
            </button>
          )}
        {selectedSession && toolMessageCount > 0 && (
          <button
            type="button"
            className="asm-detail-tool"
            aria-pressed={hideToolMessages}
            onClick={() => {
              clearHighlightTimer();
              navigatedCursorRef.current = null;
              setActiveMessageIndex(null);
              setActiveMatchCursor(0);
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
        {selectedSession && hasAnyThinking && (
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className="asm-detail-tool"
                aria-pressed={includeThinking}
                aria-label={t("sessionManager.copyIncludeThinking", {
                  defaultValue: "复制时包含思考",
                })}
                onClick={handleToggleIncludeThinking}
              >
                <span aria-hidden="true" className="text-xs leading-none">
                  ✻
                </span>
                <span>
                  {t("sessionManager.includeThinkingShort", {
                    defaultValue: "含思考",
                  })}
                </span>
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.copyIncludeThinking", {
                defaultValue: "复制时包含思考过程",
              })}
            </TooltipContent>
          </Tooltip>
        )}
        {selectedSession && visibleMessages.length > 0 && (
          <>
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  className="asm-detail-icon-tool"
                  aria-label={t("sessionManager.copyMarkdown", {
                    defaultValue: "复制 Markdown",
                  })}
                  onClick={handleCopyMarkdown}
                >
                  <Copy className="size-3.5" />
                </button>
              </TooltipTrigger>
              <TooltipContent>
                {t("sessionManager.copyMarkdown", {
                  defaultValue: "复制 Markdown",
                })}
              </TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  className="asm-detail-icon-tool"
                  aria-label={t("sessionManager.exportMarkdown", {
                    defaultValue: "导出 .md",
                  })}
                  onClick={handleExportMarkdown}
                >
                  <Download className="size-3.5" />
                </button>
              </TooltipTrigger>
              <TooltipContent>
                {t("sessionManager.exportMarkdown", {
                  defaultValue: "导出 .md",
                })}
              </TooltipContent>
            </Tooltip>
          </>
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
                <Search className="size-3.5" />
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.searchInSession", {
                defaultValue: "在当前会话中搜索",
              })}{" "}
              ({isMac() ? "⌘F" : "Ctrl F"})
            </TooltipContent>
          </Tooltip>
        )}
        {selectedSession && showSessionMinimap && (
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className={cn(
                  "asm-detail-icon-tool",
                  outlineOpen && "is-active",
                )}
                aria-label={t("sessionManager.toggleOutline", {
                  defaultValue: "切换对话目录",
                })}
                aria-pressed={outlineOpen}
                onClick={handleToggleOutline}
              >
                <PanelRight className="size-3.5" />
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.tocTitle", { defaultValue: "对话目录" })}
            </TooltipContent>
          </Tooltip>
        )}
      </div>
      {!selectedSession ? (
        <div className="asm-empty-studio">
          <div className="asm-empty-orb" aria-hidden="true">
            <MessageSquare className="size-6" />
          </div>
          <p className="text-sm font-medium text-foreground/85">
            {t("sessionManager.selectSession")}
          </p>
          <div className="asm-empty-shortcuts" aria-hidden="true">
            <span>
              <kbd className="asm-search-kbd">↑ ↓</kbd> 切换会话
            </span>
            <span>
              <kbd className="asm-search-kbd">{isMac() ? "⌘K" : "Ctrl K"}</kbd>{" "}
              搜索列表
            </span>
            <span>
              <kbd className="asm-search-kbd">{isMac() ? "⌘F" : "Ctrl F"}</kbd>{" "}
              全文检索
            </span>
          </div>
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
                {canResume && !hasBatchSelection && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="asm-resume-btn h-8 gap-1.5 px-2.5 text-xs"
                        onClick={() =>
                          onCopy(
                            fullResumeCommand,
                            t("sessionManager.resumeCommandCopied", {
                              defaultValue: "已复制恢复命令",
                            }),
                          )
                        }
                      >
                        <Terminal className="size-3.5" />
                        <span>
                          {t("sessionManager.copyResumeCommand", {
                            defaultValue: "复制恢复命令",
                          })}
                        </span>
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent side="bottom" className="max-w-sm">
                      <p className="font-mono text-xs break-all">
                        {fullResumeCommand}
                      </p>
                      <p className="text-muted-foreground mt-1">
                        点击复制命令（已包含工作目录切换）
                      </p>
                    </TooltipContent>
                  </Tooltip>
                )}
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="asm-delete-btn h-8 gap-1.5 px-3 text-xs"
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
                <span className="asm-chip-group">
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
                        className={cn(
                          "asm-chip asm-chip-mono",
                          revealableProjectDir &&
                            onRevealPath &&
                            "has-reveal-sibling",
                        )}
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
                  {revealableProjectDir && onRevealPath && (
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <button
                          type="button"
                          className="asm-chip-reveal"
                          aria-label={`在文件管理器中定位项目目录`}
                          onClick={() => onRevealPath(revealableProjectDir)}
                        >
                          <ExternalLink className="size-3" />
                        </button>
                      </TooltipTrigger>
                      <TooltipContent side="bottom">
                        {t("sessionManager.revealInFileManager", {
                          defaultValue: "在文件管理器中定位",
                        })}
                      </TooltipContent>
                    </Tooltip>
                  )}
                </span>
              )}
              {selectedSession.sourcePath && (
                <span className="asm-chip-group">
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
                        className={cn(
                          "asm-chip asm-chip-mono asm-chip-source",
                          revealableSourcePath &&
                            onRevealPath &&
                            "has-reveal-sibling",
                        )}
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
                  {revealableSourcePath && onRevealPath && (
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <button
                          type="button"
                          className="asm-chip-reveal"
                          aria-label={`在文件管理器中定位源存储文件`}
                          onClick={() => onRevealPath(revealableSourcePath)}
                        >
                          <ExternalLink className="size-3" />
                        </button>
                      </TooltipTrigger>
                      <TooltipContent side="bottom">
                        {t("sessionManager.revealInFileManager", {
                          defaultValue: "在文件管理器中定位",
                        })}
                      </TooltipContent>
                    </Tooltip>
                  )}
                </span>
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
                  navigatedCursorRef.current = null;
                  setDetailSearchQuery(event.target.value);
                  setActiveMatchCursor(0);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    event.preventDefault();
                    event.stopPropagation();
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
                    } else if (navigatedCursorRef.current === safeMatchCursor) {
                      navigateMatch(1);
                    } else {
                      navigatedCursorRef.current = safeMatchCursor;
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

          {/* 消息流（cc-switch 对话式阅读器）+ 导航栏/对话目录 */}
          <div className="asm-detail-content">
            {showSessionMinimap && !outlineOpen && (
              <SessionMinimap
                messages={displayedMessages}
                messageIndexes={minimapMessageIndexes}
                visibleMessageIndexes={viewportMessageIndexes}
                isCodexSession={isCodexSession}
                onTickClick={scrollToMessage}
              />
            )}
            <div className="asm-detail-scroll-stage">
              <div
                ref={scrollContainerRef}
                onScroll={handleScrollContainerScroll}
                className="asm-detail-scroll"
              >
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
                ) : readerLayout.items.length === 0 ? (
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
                        className="asm-ccs-stream-inner flex items-center gap-2 py-2 text-xs text-muted-foreground"
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
                      {virtualItems.map((virtualRow) => {
                        const item = readerLayout.items[virtualRow.index];
                        if (!item) return null;

                        return (
                          <div
                            key={virtualRow.key}
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
                            <div className="asm-ccs-stream-inner">
                              {item.kind === "divider" ? (
                                <div
                                  aria-hidden="true"
                                  className="asm-ccs-turn-divider"
                                />
                              ) : item.kind === "event" ? (
                                <ReaderEventRow
                                  event={item.event}
                                  highlightTerms={detailSearchTerms}
                                  onOpenUrl={onOpenUrl}
                                />
                              ) : item.kind === "injected" ? (
                                <ReaderInjectedRow
                                  message={item.message}
                                  highlightTerms={detailSearchTerms}
                                />
                              ) : item.kind === "chapter" ? (
                                <ReaderAgentLane
                                  providerId={selectedSession.providerId}
                                  providerLabel={selectedProviderLabel}
                                  isAgentStart={item.isAgentStart}
                                  agentStartTs={
                                    turnsByIndex.get(item.turnIndex)
                                      ?.agentStartTs
                                  }
                                  model={
                                    turnsByIndex.get(item.turnIndex)?.model
                                  }
                                  isActive={
                                    activeMessageIndex ===
                                    item.chapter.messageIndex
                                  }
                                >
                                  <ReaderChapterRow
                                    chapter={item.chapter}
                                    highlightTerms={detailSearchTerms}
                                  />
                                </ReaderAgentLane>
                              ) : item.kind === "question" ? (
                                <ReaderQuestionRow
                                  message={item.message}
                                  messageRevision={messageRevision}
                                  isCodexSession={isCodexSession}
                                  isActive={
                                    activeMessageIndex === item.messageIndex
                                  }
                                  isActiveSearchMatch={
                                    detailSearchOpen &&
                                    matchedMessageIndexes.length > 0 &&
                                    matchedMessageIndexes[safeMatchCursor] ===
                                      item.messageIndex
                                  }
                                  highlightTerms={detailSearchTerms}
                                  onCopy={onCopyMessage}
                                  onCopyCode={onCopyCode}
                                  onOpenUrl={onOpenUrl}
                                  onLoadFullContent={
                                    handleLoadMessageFullContent
                                  }
                                />
                              ) : item.kind === "timeline" ? (
                                <ReaderAgentLane
                                  providerId={selectedSession.providerId}
                                  providerLabel={selectedProviderLabel}
                                  isAgentStart={item.isAgentStart}
                                  agentStartTs={item.turn.agentStartTs}
                                  model={item.turn.model}
                                  isActive={
                                    activeMessageIndex !== null &&
                                    item.turn.stepIndexes.includes(
                                      activeMessageIndex,
                                    )
                                  }
                                >
                                  <ReaderTimelineRow
                                    turn={item.turn}
                                    expanded={item.expanded}
                                    hiddenFailures={item.hiddenFailures}
                                    onToggle={() =>
                                      toggleTurnTimeline(
                                        item.turn,
                                        item.expanded,
                                      )
                                    }
                                    onCopyTurn={() =>
                                      handleCopySingleTurn(item.turn)
                                    }
                                  />
                                </ReaderAgentLane>
                              ) : item.kind === "step" ? (
                                <ReaderAgentLane
                                  providerId={selectedSession.providerId}
                                  providerLabel={selectedProviderLabel}
                                  isAgentStart={item.isAgentStart}
                                  agentStartTs={
                                    turnsByIndex.get(item.turnIndex)
                                      ?.agentStartTs
                                  }
                                  model={
                                    turnsByIndex.get(item.turnIndex)?.model
                                  }
                                  isActive={
                                    activeMessageIndex === item.messageIndex
                                  }
                                >
                                  <ReaderStepRow
                                    step={item.step}
                                    messageRevision={messageRevision}
                                    style={readerStyle}
                                    projectDir={selectedSession.projectDir}
                                    depth={item.depth}
                                    inTimeline={item.inTimeline}
                                    expanded={item.expanded}
                                    previewLines={item.previewLines}
                                    onToggleStep={handleToggleStep}
                                    isActiveSearchMatch={
                                      detailSearchOpen &&
                                      matchedMessageIndexes.length > 0 &&
                                      matchedMessageIndexes[safeMatchCursor] ===
                                        item.messageIndex
                                    }
                                    highlightTerms={detailSearchTerms}
                                    onCopy={onCopyMessage}
                                    onCopyCode={onCopyCode}
                                    onOpenUrl={onOpenUrl}
                                    onLoadFullContent={
                                      handleLoadMessageFullContent
                                    }
                                  />
                                </ReaderAgentLane>
                              ) : (
                                <ReaderAgentLane
                                  providerId={selectedSession.providerId}
                                  providerLabel={selectedProviderLabel}
                                  isAgentStart={item.isAgentStart}
                                  agentStartTs={
                                    turnsByIndex.get(item.turnIndex)
                                      ?.agentStartTs
                                  }
                                  model={
                                    turnsByIndex.get(item.turnIndex)?.model
                                  }
                                  isActive={
                                    activeMessageIndex === item.messageIndex
                                  }
                                >
                                  <ReaderFinalReplyRow
                                    message={item.message}
                                    overrideText={item.overrideText}
                                    messageRevision={messageRevision}
                                    isActiveSearchMatch={
                                      detailSearchOpen &&
                                      matchedMessageIndexes.length > 0 &&
                                      matchedMessageIndexes[safeMatchCursor] ===
                                        item.messageIndex
                                    }
                                    highlightTerms={detailSearchTerms}
                                    onCopy={onCopyMessage}
                                    onCopyCode={onCopyCode}
                                    onOpenUrl={onOpenUrl}
                                    onLoadFullContent={
                                      handleLoadMessageFullContent
                                    }
                                  />
                                </ReaderAgentLane>
                              )}
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  </div>
                )}
              </div>
              {!atBottom && readerLayout.items.length > 0 && (
                <button
                  type="button"
                  aria-label={t("sessionManager.jumpToLatest", {
                    defaultValue: "跳到最新消息",
                  })}
                  onClick={handleJumpToLatest}
                  className="asm-ccs-jump-latest"
                >
                  <ArrowDown className="size-3.5" aria-hidden="true" />
                  <span>
                    {t("sessionManager.latestLabel", { defaultValue: "最新" })}
                  </span>
                </button>
              )}
            </div>
            {showSessionMinimap && outlineOpen && (
              <SessionOutline
                turns={displayedTurns}
                messages={displayedMessages}
                providerId={selectedSession.providerId}
                providerLabel={selectedProviderLabel}
                isCodexSession={isCodexSession}
                activeTurnIndex={activeTurnIndex}
                onJumpToMessage={scrollToMessage}
                onJumpToTurnReply={scrollToTurnReply}
              />
            )}
          </div>
        </>
      )}
    </div>
  );
}
