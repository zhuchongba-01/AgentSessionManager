import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
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
  Sparkles,
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
import { useMoneyFormat } from "@/hooks/useMoneyFormat";
import { sessionsApi } from "@/lib/api/sessions";
import { isMac } from "@/lib/platform";
import { clearSessionImageCache } from "@/lib/query/sessions";
import { cn } from "@/lib/utils";
import type { SessionMessage, SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import { AgentIcon } from "./AgentIcon";
import { getAgentReaderStyle } from "./reader/agentStyles";
import { ReaderContext, type ReaderContextValue } from "./reader/context";
import {
  downloadMarkdownFile,
  transcriptToMarkdown,
  turnToMarkdown,
} from "./reader/exportMarkdown";
import { useReaderT } from "./reader/i18n";
import {
  agentTurnTs,
  SessionAgentAvatar,
  SessionAgentHeader,
} from "./reader/SessionAgentAvatar";
import { SessionChapterRow } from "./reader/SessionChapterRow";
import { SessionEventRow, SessionInjectedRow } from "./reader/SessionEventRow";
import { SessionFinalReply } from "./reader/SessionFinalReply";
import { SessionOutline } from "./reader/SessionOutline";
import { SessionQuestion } from "./reader/SessionQuestion";
import { SessionStep } from "./reader/SessionStep";
import { SessionTimeline } from "./reader/SessionTimeline";
import { formatTokens, isFailureStep } from "./reader/toolSummary";
import {
  buildTurns,
  chapterRowKey,
  estimateRowHeight,
  findMatchPositionForMessage,
  findMatchRowIndex,
  findSearchHits,
  findTurnRowIndex,
  flattenRows,
  isChangeStep,
  type ReaderFilter,
  type ReaderRow,
  type SearchMatch,
  type SessionTurn,
  type TurnChapter,
} from "./reader/turns";
import { SessionMinimap } from "./SessionMinimap";
import { SessionStatusBadges } from "./SessionStatusBadges";
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
const EXPAND_ALL_STORAGE_KEY = "cc-switch.sessionReader.expandTimelines";

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

function readInitialExpandAll(): boolean {
  try {
    return window.localStorage.getItem(EXPAND_ALL_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

const sanitizeMarkdownFilename = (title: string, fallbackId: string) => {
  const cleaned = title
    .replace(/[\\/:*?"<>|\u0000-\u001f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 60);
  return `${cleaned || fallbackId}.md`;
};

const AGENT_ROW_KINDS: ReadonlySet<ReaderRow["kind"]> = new Set([
  "timeline",
  "step",
  "final",
  "injected",
]);

const hasTimelineRow = (turn: SessionTurn) =>
  turn.steps.length > 1 ||
  (turn.steps.length === 1 && isFailureStep(turn.steps[0]));

const matchAt = (matches: SearchMatch[], position: number) => {
  let remaining = position;
  for (const match of matches) {
    if (remaining <= match.count) return match;
    remaining -= match.count;
  }
  return undefined;
};

const mapViewModeToFilter = (mode: SessionReaderViewMode): ReaderFilter => {
  if (mode === "dialogue") return "conversation";
  if (mode === "changes") return "changes";
  return "all";
};

const rowPrimaryMessageIndex = (
  row: ReaderRow,
  turns: SessionTurn[],
): number | null => {
  const turn = turns[row.turn];
  switch (row.kind) {
    case "question":
      return row.messageIndex;
    case "final":
      return row.messageIndex;
    case "event":
      return row.messageIndex;
    case "injected":
      return row.messageIndex;
    case "chapter":
      return row.chapter.messageIndex;
    case "step":
      return row.step.kind === "merged"
        ? (row.step.children[0]?.messageIndex ?? null)
        : row.step.messageIndex;
    case "timeline":
      return turn?.steps[0]
        ? turn.steps[0].kind === "merged"
          ? (turn.steps[0].children[0]?.messageIndex ?? null)
          : turn.steps[0].messageIndex
        : (turn?.firstMessageIndex ?? null);
    default:
      return null;
  }
};

export interface PendingSnippetJump {
  sessionKey: string;
  transcriptIndex: number;
  query: string;
  nonce: number;
}

export interface SessionDetailPaneProps {
  selectedSession: SessionMeta | null;
  selectedKey: string | null;
  isCodexSession: boolean;
  selectedSessionChildCount: number;
  sessionTitles: Map<string, string>;
  rawMessages?: SessionMessage[];
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
  rawMessages,
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
  onOpenUrl: _onOpenUrl,
  onRevealPath,
  onNotifyExport,
  pendingJumpTarget,
  onConsumeJumpTarget,
}: SessionDetailPaneProps) {
  const { t } = useTranslation();
  const rt = useReaderT();
  const { moneyCompact } = useMoneyFormat();
  const scrollContainerRef = useRef<HTMLDivElement | null>(null);
  const highlightTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const detailSearchInputRef = useRef<HTMLInputElement | null>(null);
  const navigatedCursorRef = useRef<number | null>(null);

  const [flashRowKey, setFlashRowKey] = useState<string | null>(null);
  const [hideToolMessages, setHideToolMessages] = useState(false);
  const [showInjected, setShowInjected] = useState(false);
  const [readerViewMode, setReaderViewMode] =
    useState<SessionReaderViewMode>("all");
  const [expandAllTimelines, setExpandAllTimelines] =
    useState(readInitialExpandAll);
  const [turnOverrides, setTurnOverrides] = useState<Map<string, boolean>>(
    () => new Map(),
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

  useEffect(() => () => clearSessionImageCache(), []);

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

  const handleToggleTurn = useCallback((turnKey: string, expanded: boolean) => {
    setTurnOverrides((prev) => new Map(prev).set(turnKey, !expanded));
  }, []);

  const handleToggleStep = useCallback((stepId: string, expanded: boolean) => {
    setStepOverrides((prev) => new Map(prev).set(stepId, !expanded));
  }, []);

  const handleToggleExpandAllTimelines = useCallback(() => {
    setTurnOverrides(new Map());
    setStepOverrides(new Map());
    setExpandAllTimelines((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(EXPAND_ALL_STORAGE_KEY, next ? "1" : "0");
      } catch {
        // ignore
      }
      return next;
    });
  }, []);

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

  // 当开启「显示注入」时使用包含 injected 标记的原始消息数组，否则使用常规可见消息
  const baseMessages = useMemo(() => {
    const src = showInjected && rawMessages ? rawMessages : visibleMessages;
    if (expandedContents.size === 0) return src;
    return src.map((msg) => {
      const tIdx = msg.transcriptIndex ?? msg.index;
      const full = tIdx !== undefined ? expandedContents.get(tIdx) : undefined;
      if (full === undefined) return msg;
      return {
        ...msg,
        content: full,
        truncated: false,
      };
    });
  }, [expandedContents, rawMessages, showInjected, visibleMessages]);

  const hasInjectedMessages = useMemo(
    () =>
      (rawMessages ?? visibleMessages).some((message) =>
        Boolean(message.injected),
      ),
    [rawMessages, visibleMessages],
  );

  const toolMessageCount = useMemo(() => {
    let count = 0;
    for (const message of baseMessages) {
      if (message.blocks && message.blocks.length > 0) {
        count += message.blocks.filter((b) => b.type === "tool_call").length;
      } else {
        const role = message.role.toLowerCase();
        if (role !== "user" && role !== "assistant" && role !== "thinking") {
          count += 1;
        }
      }
    }
    return count;
  }, [baseMessages]);

  const displayedMessages = useMemo(() => {
    if (!hideToolMessages) return baseMessages;
    const filtered: SessionMessage[] = [];
    for (const message of baseMessages) {
      const role = message.role.toLowerCase();
      if (message.blocks && message.blocks.length > 0) {
        const nonToolBlocks = message.blocks.filter(
          (b) => b.type !== "tool_call" && b.type !== "tool_result",
        );
        if (nonToolBlocks.length === 0) continue;
        if (nonToolBlocks.length !== message.blocks.length) {
          filtered.push({
            ...message,
            blocks: nonToolBlocks,
          });
          continue;
        }
        filtered.push(message);
        continue;
      }
      if (role !== "user" && role !== "assistant" && role !== "thinking") {
        continue;
      }
      filtered.push(message);
    }
    return filtered;
  }, [baseMessages, hideToolMessages]);

  const style = useMemo(
    () => getAgentReaderStyle(selectedSession?.providerId ?? ""),
    [selectedSession?.providerId],
  );

  const selectedProviderLabel = useMemo(
    () =>
      selectedSession ? getProviderLabel(selectedSession.providerId, t) : "",
    [selectedSession, t],
  );

  const fullTurns = useMemo(
    () => buildTurns(baseMessages, { style }),
    [baseMessages, style],
  );

  const displayedTurns = useMemo(
    () => buildTurns(displayedMessages, { style }),
    [displayedMessages, style],
  );

  const hasAnyThinking = useMemo(
    () =>
      fullTurns.some((turn) =>
        turn.steps.some((step) => step.kind === "thinking"),
      ),
    [fullTurns],
  );

  const hasAnyChanges = useMemo(
    () =>
      fullTurns.some((turn) =>
        turn.steps.some(
          (step) =>
            isChangeStep(step) ||
            isFailureStep(step) ||
            (step.kind === "merged" &&
              step.children.some((c) => isChangeStep(c) || isFailureStep(c))),
        ),
      ),
    [fullTurns],
  );

  const collapsibleTurnCount = useMemo(
    () => displayedTurns.filter(hasTimelineRow).length,
    [displayedTurns],
  );

  // 统计模型、总 Token 与费用芯片
  const usageSummary = useMemo(() => {
    let inputTokens = 0;
    let outputTokens = 0;
    let cacheReadTokens = 0;
    let cacheWriteTokens = 0;
    let reasoningTokens = 0;
    let metaCostUsd = 0;
    let hasMetaCost = false;
    let latestModel: string | undefined;

    for (const msg of baseMessages) {
      const meta = msg.meta;
      if (!meta) continue;
      if (meta.model) latestModel = meta.model;
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
    for (const turn of fullTurns) {
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
    const costUsd = hasMetaCost
      ? metaCostUsd
      : hasStepCost
        ? stepCostUsd
        : null;

    return { latestModel, totalTokens, costUsd };
  }, [baseMessages, fullTurns]);

  const activeQuery = detailSearchOpen ? detailSearchQuery.trim() : "";
  const hasTruncatedLegacyMessages = useMemo(
    () =>
      displayedMessages.some(
        (msg) =>
          (!msg.blocks || msg.blocks.length === 0) && Boolean(msg.truncated),
      ),
    [displayedMessages],
  );

  const searchRequestScopeKey = `${sessionScopeKey}\u0000${activeQuery.toLocaleLowerCase()}`;
  const [serverSearchState, setServerSearchState] = useState<{
    key: string;
    indexes: Set<number>;
  } | null>(null);

  const providerId = selectedSession?.providerId;
  const sourcePath = selectedSession?.sourcePath;

  useEffect(() => {
    if (
      !activeQuery ||
      !hasTruncatedLegacyMessages ||
      !providerId ||
      !sourcePath
    ) {
      return;
    }
    let cancelled = false;
    void sessionsApi
      .searchMessages(providerId, sourcePath, activeQuery)
      .then((indices) => {
        if (cancelled || !indices) return;
        setServerSearchState({
          key: searchRequestScopeKey,
          indexes: new Set(indices),
        });
        // 自动预拉取后端命中的被截断旧消息全文，使搜索计数与高亮精确
        for (const msg of displayedMessages) {
          const tIdx = msg.transcriptIndex ?? msg.index;
          if (
            tIdx !== undefined &&
            indices.includes(tIdx) &&
            msg.truncated &&
            !expandedContents.has(tIdx)
          ) {
            void handleLoadMessageFullContent(msg).catch(() => undefined);
          }
        }
      });
    return () => {
      cancelled = true;
    };
  }, [
    activeQuery,
    displayedMessages,
    expandedContents,
    handleLoadMessageFullContent,
    hasTruncatedLegacyMessages,
    providerId,
    searchRequestScopeKey,
    sourcePath,
  ]);

  const search = useMemo(
    () => findSearchHits(displayedTurns, activeQuery),
    [activeQuery, displayedTurns],
  );

  // 若后端已确认某条未展开的截断消息命中但当前前端尚未拉完其全文，补入占位匹配以保证计数立即响应
  const serverExtraMatches = useMemo(() => {
    if (
      !activeQuery ||
      !serverSearchState ||
      serverSearchState.key !== searchRequestScopeKey
    ) {
      return [] as number[];
    }
    const extra: number[] = [];
    displayedMessages.forEach((msg, displayedIdx) => {
      const tIdx = msg.transcriptIndex ?? msg.index;
      if (
        tIdx !== undefined &&
        msg.truncated &&
        serverSearchState.indexes.has(tIdx) &&
        !msg.content
          .toLocaleLowerCase()
          .includes(activeQuery.toLocaleLowerCase())
      ) {
        extra.push(displayedIdx);
      }
    });
    return extra;
  }, [
    activeQuery,
    displayedMessages,
    searchRequestScopeKey,
    serverSearchState,
  ]);

  const filter = mapViewModeToFilter(readerViewMode);

  const rows = useMemo(
    () =>
      flattenRows(displayedTurns, {
        filter,
        expandAll: expandAllTimelines,
        turnOverrides,
        stepOverrides,
        search,
        showInjected,
      }),
    [
      displayedTurns,
      expandAllTimelines,
      filter,
      search,
      showInjected,
      stepOverrides,
      turnOverrides,
    ],
  );

  const agentStartKeys = useMemo(() => {
    const keys = new Set<string>();
    const seen = new Set<number>();
    for (const row of rows) {
      if (seen.has(row.turn) || !AGENT_ROW_KINDS.has(row.kind)) continue;
      seen.add(row.turn);
      keys.add(row.key);
    }
    return keys;
  }, [rows]);

  const totalMatchCount = (search?.total ?? 0) + serverExtraMatches.length;
  const safeMatchCursor =
    totalMatchCount > 0 ? Math.min(activeMatchCursor, totalMatchCount - 1) : 0;

  const activeMatch = useMemo(() => {
    if (!search || totalMatchCount === 0) return undefined;
    if (safeMatchCursor < search.total) {
      return matchAt(search.matches, safeMatchCursor + 1);
    }
    return undefined;
  }, [safeMatchCursor, search, totalMatchCount]);

  const activeMatchRowIndex = useMemo(() => {
    if (activeMatch) {
      return findMatchRowIndex(rows, activeMatch);
    }
    if (
      totalMatchCount > 0 &&
      safeMatchCursor >= (search?.total ?? 0) &&
      serverExtraMatches.length > 0
    ) {
      const targetMsgIdx =
        serverExtraMatches[safeMatchCursor - (search?.total ?? 0)];
      if (targetMsgIdx !== undefined) {
        return rows.findIndex(
          (row) => rowPrimaryMessageIndex(row, displayedTurns) === targetMsgIdx,
        );
      }
    }
    return -1;
  }, [
    activeMatch,
    displayedTurns,
    rows,
    safeMatchCursor,
    search?.total,
    serverExtraMatches,
    totalMatchCount,
  ]);

  const activeSearchKey =
    activeMatchRowIndex >= 0 ? (rows[activeMatchRowIndex]?.key ?? null) : null;

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: (index) =>
      rows[index] ? estimateRowHeight(rows[index]) : 48,
    initialRect: { width: 800, height: 600 },
    getItemKey: (index) =>
      `${selectedKey}:${messageRevision}:${rows[index]?.key ?? index}`,
    overscan: 8,
    paddingStart: 8,
    paddingEnd: 40,
  });

  const virtualItems = virtualizer.getVirtualItems();
  const viewportStart = virtualizer.scrollOffset ?? 0;
  const viewportEnd =
    viewportStart + Math.max(0, virtualizer.scrollRect?.height ?? 600);

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
      const row = rows[item.index];
      if (!row) continue;
      const msgIdx = rowPrimaryMessageIndex(row, displayedTurns);
      if (msgIdx !== null) visible.push(msgIdx);
    }
    return visible;
  }, [displayedTurns, rows, viewportEnd, viewportStart, virtualItems]);

  const activeTurn = useMemo(() => {
    if (rows.length === 0) return 0;
    const firstVisible = virtualItems.find(
      (item) => item.end > viewportStart + 8,
    );
    return firstVisible ? (rows[firstVisible.index]?.turn ?? 0) : 0;
  }, [rows, viewportStart, virtualItems]);

  const clearHighlightTimer = useCallback(() => {
    if (highlightTimerRef.current !== null) {
      clearTimeout(highlightTimerRef.current);
      highlightTimerRef.current = null;
    }
  }, []);

  const flashRow = useCallback(
    (key: string) => {
      clearHighlightTimer();
      setFlashRowKey(key);
      highlightTimerRef.current = setTimeout(() => {
        setFlashRowKey((cur) => (cur === key ? null : cur));
        highlightTimerRef.current = null;
      }, 2000);
    },
    [clearHighlightTimer],
  );

  useEffect(() => {
    clearHighlightTimer();
    navigatedCursorRef.current = null;
    setFlashRowKey(null);
    setTurnOverrides(new Map());
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
        !event.shiftKey &&
        !event.altKey &&
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

  const userMessagesToc = useMemo(
    () => buildUserMessageToc(displayedMessages, isCodexSession),
    [displayedMessages, isCodexSession],
  );
  const minimapMessageIndexes = useMemo(
    () => userMessagesToc.map((item) => item.index),
    [userMessagesToc],
  );
  const showSessionMinimap = shouldShowSessionMinimap(
    minimapMessageIndexes.length,
  );

  const scrollToRowIndex = useCallback(
    (rowIndex: number) => {
      if (rowIndex < 0 || !rows[rowIndex]) return;
      virtualizer.scrollToIndex(rowIndex, {
        align: "center",
        behavior: "auto",
      });
      flashRow(rows[rowIndex].key);
    },
    [flashRow, rows, virtualizer],
  );

  const scrollToDisplayedMessage = useCallback(
    (displayedIndex: number) => {
      let rowIndex = rows.findIndex(
        (row) => rowPrimaryMessageIndex(row, displayedTurns) === displayedIndex,
      );
      if (rowIndex < 0) {
        const turn = displayedTurns.find(
          (item) =>
            displayedIndex >= item.firstMessageIndex &&
            displayedIndex <= item.lastMessageIndex,
        );
        if (turn) {
          rowIndex = findTurnRowIndex(rows, displayedTurns, turn.key);
        }
      }
      if (rowIndex >= 0) {
        scrollToRowIndex(rowIndex);
      }
    },
    [displayedTurns, rows, scrollToRowIndex],
  );

  const jumpToQuestion = useCallback(
    (turn: SessionTurn) => {
      const idx = rows.findIndex(
        (row) => row.turn === turn.index && row.kind === "question",
      );
      scrollToRowIndex(
        idx >= 0 ? idx : findTurnRowIndex(rows, displayedTurns, turn.key),
      );
    },
    [displayedTurns, rows, scrollToRowIndex],
  );

  const jumpToReply = useCallback(
    (turn: SessionTurn) => {
      let idx = rows.findIndex(
        (row) => row.turn === turn.index && row.kind === "final",
      );
      if (idx < 0) {
        idx = rows.findIndex(
          (row) => row.turn === turn.index && AGENT_ROW_KINDS.has(row.kind),
        );
      }
      scrollToRowIndex(
        idx >= 0 ? idx : findTurnRowIndex(rows, displayedTurns, turn.key),
      );
    },
    [displayedTurns, rows, scrollToRowIndex],
  );

  const jumpToChapter = useCallback(
    (turn: SessionTurn, chapter: TurnChapter) => {
      const key = chapterRowKey(turn, chapter);
      const idx = rows.findIndex((row) => row.key === key);
      scrollToRowIndex(
        idx >= 0 ? idx : findTurnRowIndex(rows, displayedTurns, turn.key),
      );
    },
    [displayedTurns, rows, scrollToRowIndex],
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
      if (search && pendingJumpTarget.query) {
        const pos = findMatchPositionForMessage(
          displayedTurns,
          search,
          targetDisplayedIdx,
        );
        setActiveMatchCursor(Math.max(0, pos - 1));
      }
      scrollToDisplayedMessage(targetDisplayedIdx);
      onConsumeJumpTarget?.();
    } else if (!isLoadingMessages && !isFetchingMessages) {
      onConsumeJumpTarget?.();
    }
  }, [
    displayedMessages,
    displayedTurns,
    hideToolMessages,
    isFetchingMessages,
    isLoadingMessages,
    onConsumeJumpTarget,
    pendingJumpTarget,
    readerViewMode,
    scrollToDisplayedMessage,
    search,
    selectedKey,
  ]);

  useEffect(() => {
    if (activeMatchRowIndex >= 0 && activeQuery) {
      virtualizer.scrollToIndex(activeMatchRowIndex, { align: "center" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeMatchRowIndex, activeQuery, safeMatchCursor]);

  const navigateMatch = useCallback(
    (delta: number) => {
      if (totalMatchCount === 0) return;
      const nextCursor =
        (safeMatchCursor + delta + totalMatchCount) % totalMatchCount;
      navigatedCursorRef.current = nextCursor;
      setActiveMatchCursor(nextCursor);
    },
    [safeMatchCursor, totalMatchCount],
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

  const exportOptions = useMemo(
    () => ({
      style,
      appName: selectedProviderLabel,
      youLabel: t("sessionManager.you", { defaultValue: "你" }),
      projectDir: selectedSession?.projectDir ?? undefined,
      t: rt,
      formatCost: moneyCompact,
      includeThinking,
    }),
    [
      includeThinking,
      moneyCompact,
      rt,
      selectedProviderLabel,
      selectedSession?.projectDir,
      style,
      t,
    ],
  );

  const handleCopyTurn = useCallback(
    (turnKey: string) => {
      const turn = displayedTurns.find((item) => item.key === turnKey);
      if (!turn) return;
      onCopy(
        turnToMarkdown(turn, exportOptions),
        t("sessionManager.turnCopied", { defaultValue: "已复制本轮对话" }),
      );
    },
    [displayedTurns, exportOptions, onCopy, t],
  );

  const handleCopyMarkdown = useCallback(() => {
    if (!selectedSession || fullTurns.length === 0) return;
    const title = formatSessionTitle(selectedSession);
    onCopy(
      transcriptToMarkdown(title, fullTurns, exportOptions),
      t("sessionManager.markdownCopied", {
        defaultValue: "已复制会话 Markdown",
      }),
    );
  }, [exportOptions, fullTurns, onCopy, selectedSession, t]);

  const handleExportMarkdown = useCallback(() => {
    if (!selectedSession || fullTurns.length === 0) return;
    const title = formatSessionTitle(selectedSession);
    const filename = sanitizeMarkdownFilename(title, selectedSession.sessionId);
    downloadMarkdownFile(
      filename,
      transcriptToMarkdown(title, fullTurns, {
        ...exportOptions,
        includeThinking: true,
      }),
    );
    onNotifyExport?.(filename);
  }, [exportOptions, fullTurns, onNotifyExport, selectedSession]);

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
    if (rows.length === 0) return;
    virtualizer.scrollToIndex(rows.length - 1, {
      align: "end",
      behavior: "auto",
    });
    const el = scrollContainerRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
    setAtBottom(true);
  }, [rows.length, virtualizer]);

  const readerContext = useMemo<ReaderContextValue>(
    () => ({
      providerId: selectedSession?.providerId ?? "",
      sourcePath: selectedSession?.sourcePath ?? undefined,
      projectDir: selectedSession?.projectDir ?? undefined,
      style,
      appName: selectedProviderLabel,
      searchQuery: activeQuery || undefined,
      onCopy,
    }),
    [
      activeQuery,
      onCopy,
      selectedProviderLabel,
      selectedSession?.projectDir,
      selectedSession?.providerId,
      selectedSession?.sourcePath,
      style,
    ],
  );

  const renderLegacyTruncatedFooter = (msgIndex: number | null): ReactNode => {
    if (msgIndex === null) return null;
    const msg = displayedMessages[msgIndex];
    if (!msg || (msg.blocks && msg.blocks.length > 0) || !msg.truncated) {
      return null;
    }
    const tIdx = msg.transcriptIndex ?? msg.index;
    if (tIdx !== undefined && expandedContents.has(tIdx)) return null;
    return (
      <div className="mt-1.5">
        <button
          type="button"
          onClick={() => {
            void handleLoadMessageFullContent(msg).catch(() => undefined);
          }}
          className="rounded-[2px] text-caption text-fg-2 underline decoration-border-strong underline-offset-[3px] transition-colors hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {t("sessionManager.expandContent", { defaultValue: "展开完整内容" })}
        </button>
      </div>
    );
  };

  const renderAgentRow = (row: ReaderRow, content: ReactNode): ReactNode => {
    const start = agentStartKeys.has(row.key);
    const turn = displayedTurns[row.turn];
    return (
      <div className="relative pe-8 ps-10">
        {start && turn && (
          <>
            <span className="absolute start-0 top-0">
              <SessionAgentAvatar />
            </span>
            <SessionAgentHeader
              ts={agentTurnTs(turn, displayedMessages)}
              model={turn.final?.model}
            />
          </>
        )}
        {content}
      </div>
    );
  };

  const renderRowContent = (row: ReaderRow): ReactNode => {
    const turn = displayedTurns[row.turn];
    if (!turn) return null;
    switch (row.kind) {
      case "question":
        return turn.question ? (
          <div className="flex justify-end pb-3 ps-14">
            <div className="min-w-0 max-w-[76%]">
              <SessionQuestion
                question={turn.question}
                forceExpanded={row.forceExpanded}
              />
              {renderLegacyTruncatedFooter(row.messageIndex)}
            </div>
          </div>
        ) : null;
      case "timeline":
        return (
          <div className="py-0.5">
            <SessionTimeline
              turnKey={turn.key}
              summary={row.summary}
              stepTotal={turn.steps.length}
              expanded={row.expanded}
              hiddenFailures={row.hiddenFailures}
              noFinal={!turn.final && !turn.aborted}
              onToggle={handleToggleTurn}
              onCopyTurn={handleCopyTurn}
            />
          </div>
        );
      case "step": {
        const inTimeline = filter === "all" && hasTimelineRow(turn);
        const stepMsgIdx =
          row.step.kind === "merged" ? null : row.step.messageIndex;
        const legacyMsg =
          stepMsgIdx !== null ? displayedMessages[stepMsgIdx] : undefined;
        const legacyTIdx = legacyMsg?.transcriptIndex ?? legacyMsg?.index;
        const isLegacyExpanded =
          Boolean(
            legacyMsg &&
              (!legacyMsg.blocks || legacyMsg.blocks.length === 0) &&
              legacyTIdx !== undefined &&
              expandedContents.has(legacyTIdx),
          ) && !stepOverrides.has(row.step.id);
        return (
          <div
            className={cn(
              inTimeline && "ms-[13px] border-s border-border ps-2",
              row.depth === 1 && "ps-7",
            )}
          >
            <SessionStep
              step={row.step}
              expanded={row.expanded || isLegacyExpanded}
              previewLines={
                isLegacyExpanded ? Number.MAX_SAFE_INTEGER : row.previewLines
              }
              onToggle={handleToggleStep}
            />
            {renderLegacyTruncatedFooter(stepMsgIdx)}
          </div>
        );
      }
      case "final":
        return turn.final ? (
          <div className="pb-1 pt-2">
            <SessionFinalReply
              final={turn.final}
              forceExpanded={row.forceExpanded}
              hasTimeline={hasTimelineRow(turn)}
              turnKey={turn.key}
              onCopyTurn={handleCopyTurn}
            />
            {renderLegacyTruncatedFooter(row.messageIndex)}
          </div>
        ) : null;
      case "chapter":
        return <SessionChapterRow chapter={row.chapter} />;
      case "event":
        return <SessionEventRow block={row.block} body={row.body} />;
      case "injected":
        return displayedMessages[row.messageIndex] ? (
          <SessionInjectedRow message={displayedMessages[row.messageIndex]} />
        ) : null;
      case "turn_divider":
        return (
          <div aria-hidden className="py-4">
            <div className="h-px bg-border" />
          </div>
        );
    }
  };

  const renderRow = (row: ReaderRow): ReactNode => {
    const content = renderRowContent(row);
    return AGENT_ROW_KINDS.has(row.kind) && content
      ? renderAgentRow(row, content)
      : content;
  };

  return (
    <ReaderContext.Provider value={readerContext}>
      <div
        className="asm-detail"
        data-reader-style={style.id}
        style={
          {
            "--reader-accent": `var(${style.accentVar})`,
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
          {selectedSession && baseMessages.length > 0 && (
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
                {t("sessionManager.readerViewDialogue", {
                  defaultValue: "对话",
                })}
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
            collapsibleTurnCount > 0 && (
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
                setFlashRowKey(null);
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
          {selectedSession && hasInjectedMessages && (
            <button
              type="button"
              className="asm-detail-tool"
              aria-pressed={showInjected}
              onClick={() => setShowInjected((prev) => !prev)}
            >
              <span>{rt("injectedToggle")}</span>
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
          {selectedSession && baseMessages.length > 0 && (
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
                <kbd className="asm-search-kbd">
                  {isMac() ? "⌘K" : "Ctrl K"}
                </kbd>{" "}
                搜索列表
              </span>
              <span>
                <kbd className="asm-search-kbd">
                  {isMac() ? "⌘F" : "Ctrl F"}
                </kbd>{" "}
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
                  parentLabel={resolveParentLabel(
                    selectedSession,
                    sessionTitles,
                  )}
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

              {/* 时间、ID、目录、文件与 Token 用量 */}
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
                            aria-label="在文件管理器中定位项目目录"
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
                            aria-label="在文件管理器中定位源存储文件"
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
                {(usageSummary.latestModel ||
                  usageSummary.totalTokens > 0 ||
                  usageSummary.costUsd !== null) && (
                  <span className="asm-chip asm-chip-mono">
                    <Sparkles className="size-3 shrink-0" />
                    <span>
                      {[
                        usageSummary.latestModel,
                        usageSummary.totalTokens > 0
                          ? `${formatTokens(usageSummary.totalTokens)} Tokens`
                          : null,
                        usageSummary.costUsd !== null
                          ? moneyCompact(usageSummary.costUsd)
                          : null,
                      ]
                        .filter(Boolean)
                        .join(" · ")}
                    </span>
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
                    if (event.key === "Enter" && totalMatchCount > 0) {
                      event.preventDefault();
                      if (event.shiftKey) {
                        navigateMatch(-1);
                      } else if (
                        navigatedCursorRef.current === safeMatchCursor
                      ) {
                        navigateMatch(1);
                      } else {
                        navigatedCursorRef.current = safeMatchCursor;
                        if (activeMatchRowIndex >= 0) {
                          scrollToRowIndex(activeMatchRowIndex);
                        }
                      }
                    }
                  }}
                />
                {activeQuery.length > 0 && (
                  <span className="asm-detail-search-count">
                    {totalMatchCount > 0
                      ? `${safeMatchCursor + 1} / ${totalMatchCount}`
                      : "0 / 0"}
                  </span>
                )}
                <button
                  type="button"
                  aria-label={t("sessionManager.prevMatch", {
                    defaultValue: "上一处匹配",
                  })}
                  disabled={totalMatchCount === 0}
                  onClick={() => navigateMatch(-1)}
                >
                  <ChevronUp className="size-3.5" aria-hidden="true" />
                </button>
                <button
                  type="button"
                  aria-label={t("sessionManager.nextMatch", {
                    defaultValue: "下一处匹配",
                  })}
                  disabled={totalMatchCount === 0}
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

            {/* 消息流（cc-switch 结构化阅读引擎）+ 缩略导航 / 对话目录 */}
            <div className="asm-detail-content">
              {showSessionMinimap && !outlineOpen && (
                <SessionMinimap
                  messages={displayedMessages}
                  messageIndexes={minimapMessageIndexes}
                  visibleMessageIndexes={viewportMessageIndexes}
                  isCodexSession={isCodexSession}
                  onTickClick={scrollToDisplayedMessage}
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
                  ) : rows.length === 0 ? (
                    <div className="flex flex-col items-center justify-center py-12 text-center">
                      <MessageSquare className="mb-2 size-8 text-muted-foreground/50" />
                      <p className="text-sm text-muted-foreground">
                        {readerViewMode === "changes" && baseMessages.length > 0
                          ? rt("noChanges")
                          : t("sessionManager.emptySession")}
                      </p>
                    </div>
                  ) : (
                    <div className="asm-messages px-6 py-4">
                      {isFetchingMessages && (
                        <div
                          role="status"
                          className="mx-auto flex w-full max-w-[820px] items-center gap-2 py-2 text-xs text-muted-foreground"
                        >
                          <RefreshCw className="size-3.5 animate-spin" />
                          {t("sessionManager.loadingMoreMessages", {
                            defaultValue: "正在继续读取会话…",
                          })}
                        </div>
                      )}
                      <div
                        className="relative w-full"
                        style={{ height: virtualizer.getTotalSize() }}
                      >
                        {virtualItems.map((virtualRow) => {
                          const row = rows[virtualRow.index];
                          if (!row) return null;
                          const isHighlighted =
                            row.key === activeSearchKey ||
                            row.key === flashRowKey;
                          return (
                            <div
                              key={virtualRow.key}
                              data-index={virtualRow.index}
                              data-row-kind={row.kind}
                              ref={virtualizer.measureElement}
                              className="absolute left-0 top-0 w-full"
                              style={{
                                transform: `translateY(${virtualRow.start}px)`,
                              }}
                            >
                              <div
                                className={cn(
                                  "mx-auto w-full max-w-[820px] rounded-panel transition-shadow motion-reduce:transition-none",
                                  isHighlighted &&
                                    "ring-2 ring-ring ring-offset-2 ring-offset-background",
                                )}
                              >
                                {renderRow(row)}
                              </div>
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  )}
                </div>
                {!atBottom && rows.length > 0 && (
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
                      {t("sessionManager.latestLabel", {
                        defaultValue: "最新",
                      })}
                    </span>
                  </button>
                )}
              </div>
              {showSessionMinimap && outlineOpen && (
                <SessionOutline
                  turns={displayedTurns}
                  activeTurn={activeTurn}
                  onJumpQuestion={jumpToQuestion}
                  onJumpReply={jumpToReply}
                  onJumpChapter={jumpToChapter}
                />
              )}
            </div>
          </>
        )}
      </div>
    </ReaderContext.Provider>
  );
}
