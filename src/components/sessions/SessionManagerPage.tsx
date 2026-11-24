import {
  useCallback,
  useDeferredValue,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { openUrl } from "@tauri-apps/plugin-opener";
import { AlertTriangle } from "lucide-react";
import {
  useSessionMessagesQuery,
  useSessionsQuery,
} from "@/lib/query/sessions";
import { piKeys } from "@/lib/query/pi";
import { piApi } from "@/lib/api/pi";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMessage, SessionMeta } from "@/types";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { TooltipProvider } from "@/components/ui/tooltip";
import { cn, safeLocalStorageGet, safeLocalStorageSet } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";
import {
  checkForAvailableUpdate,
  type AvailableUpdate,
} from "@/lib/githubRelease";
import { useTheme } from "@/components/theme-provider";
import { AppTitleBar } from "./AppTitleBar";
import { SessionListSidebar } from "./SessionListSidebar";
import {
  SessionDetailPane,
  type PendingSnippetJump,
} from "./SessionDetailPane";
import {
  getContentHitKey,
  useSessionContentSearch,
} from "./useSessionContentSearch";
import { useSessionDeletion } from "./useSessionDeletion";
import type {
  ProviderFilter,
  SessionGroupExpansionState,
  SessionListViewMode,
  SessionStatusFilter,
} from "./types";
import {
  buildSessionTitleIndex,
  collectSessionDescendants,
  getVisibleSessionMessages,
  getProviderLabel,
  getSessionDirectoryGroupKey,
  getSessionKey,
  groupSessionsByProviderAndDirectory,
  groupSessionsByTime,
} from "./utils";

const SESSION_LIST_VIEW_MODE_STORAGE_KEY = "agent-session-manager.listViewMode";
const SESSION_GROUP_EXPANSION_STORAGE_KEY =
  "agent-session-manager.groupExpansionState";
const SIDEBAR_WIDTH_STORAGE_KEY = "agent-session-manager.sidebarWidth";
const DEFAULT_SIDEBAR_WIDTH = 320;
const MIN_SIDEBAR_WIDTH = 240;
const MAX_SIDEBAR_WIDTH = 520;

const getEffectiveMaxSidebarWidth = () => {
  if (
    typeof window !== "undefined" &&
    Number.isFinite(window.innerWidth) &&
    window.innerWidth > 0 &&
    window.innerWidth <= 1050
  ) {
    return Math.max(
      MIN_SIDEBAR_WIDTH,
      Math.min(MAX_SIDEBAR_WIDTH, Math.floor(window.innerWidth * 0.42)),
    );
  }
  return MAX_SIDEBAR_WIDTH;
};

const clampSidebarWidth = (width: number) =>
  Math.min(
    getEffectiveMaxSidebarWidth(),
    Math.max(MIN_SIDEBAR_WIDTH, Math.round(width)),
  );

const readInitialSidebarWidth = (): number => {
  const raw = safeLocalStorageGet(SIDEBAR_WIDTH_STORAGE_KEY);
  const parsed = raw ? Number(raw) : Number.NaN;
  return Number.isFinite(parsed)
    ? clampSidebarWidth(parsed)
    : DEFAULT_SIDEBAR_WIDTH;
};

const readInitialSessionListViewMode = (): SessionListViewMode => {
  const stored = safeLocalStorageGet(SESSION_LIST_VIEW_MODE_STORAGE_KEY);
  return stored === "grouped" || stored === "flat" || stored === "time"
    ? stored
    : "grouped";
};

const readInitialSessionGroupExpansionState =
  (): SessionGroupExpansionState => {
    if (typeof window === "undefined") {
      return {
        expandedProviderIds: new Set(),
        expandedDirectoryKeys: new Set(),
        restoredFromStorage: false,
      };
    }

    try {
      const stored = safeLocalStorageGet(SESSION_GROUP_EXPANSION_STORAGE_KEY);
      const parsed = stored ? JSON.parse(stored) : null;

      if (!parsed || typeof parsed !== "object") {
        return {
          expandedProviderIds: new Set(),
          expandedDirectoryKeys: new Set(),
          restoredFromStorage: false,
        };
      }

      const expandedProviderIds = Array.isArray(parsed.expandedProviderIds)
        ? parsed.expandedProviderIds.filter(
            (providerId: unknown): providerId is string =>
              typeof providerId === "string",
          )
        : [];
      const expandedDirectoryKeys = Array.isArray(parsed.expandedDirectoryKeys)
        ? parsed.expandedDirectoryKeys.filter(
            (directoryKey: unknown): directoryKey is string =>
              typeof directoryKey === "string",
          )
        : [];

      return {
        expandedProviderIds: new Set(expandedProviderIds),
        expandedDirectoryKeys: new Set(expandedDirectoryKeys),
        restoredFromStorage: true,
      };
    } catch {
      return {
        expandedProviderIds: new Set(),
        expandedDirectoryKeys: new Set(),
        restoredFromStorage: false,
      };
    }
  };

const serializeSessionGroupExpansionState = (
  expandedProviderGroups: Set<string>,
  expandedDirectoryGroups: Set<string>,
) =>
  JSON.stringify({
    expandedProviderIds: Array.from(expandedProviderGroups).sort(),
    expandedDirectoryKeys: Array.from(expandedDirectoryGroups).sort(),
  });

const filterSetToAllowedValues = (
  current: Set<string>,
  allowedValues: Set<string>,
) => {
  let changed = false;
  const next = new Set<string>();

  current.forEach((value) => {
    if (allowedValues.has(value)) {
      next.add(value);
    } else {
      changed = true;
    }
  });

  return changed ? next : current;
};

export function SessionManagerPage({ appId }: { appId: string }) {
  const { t } = useTranslation();
  const { theme, resolvedTheme, setTheme } = useTheme();
  const queryClient = useQueryClient();
  const {
    data,
    isLoading,
    isFetching,
    isSuccess: sessionsIsSuccess,
    refetch,
    dataUpdatedAt,
    error: sessionsError,
    warnings: scanWarnings,
  } = useSessionsQuery();
  const sessions = data ?? [];
  const includesPiSessions = appId === "all" || appId === "pi";
  const piSessionDiscovery = useQuery({
    queryKey: piKeys.sessionDiscovery,
    queryFn: () => piApi.getSessionDiscovery(),
    enabled: includesPiSessions,
    staleTime: 30 * 1000,
  });
  const rescanInFlightRef = useRef<Promise<void> | null>(null);
  const [selectedSessionKeys, setSelectedSessionKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [selectionMode, setSelectionMode] = useState(false);
  const [availableUpdate, setAvailableUpdate] =
    useState<AvailableUpdate | null>(null);

  const [providerFilter, setProviderFilter] = useState<ProviderFilter>(
    appId as ProviderFilter,
  );
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [pendingJumpTarget, setPendingJumpTarget] =
    useState<PendingSnippetJump | null>(null);
  const [listViewMode, setListViewMode] = useState<SessionListViewMode>(
    readInitialSessionListViewMode,
  );
  const [initialGroupExpansionState] = useState(
    readInitialSessionGroupExpansionState,
  );
  const [expandedProviderGroups, setExpandedProviderGroups] = useState<
    Set<string>
  >(() => initialGroupExpansionState.expandedProviderIds);
  const [expandedDirectoryGroups, setExpandedDirectoryGroups] = useState<
    Set<string>
  >(() => initialGroupExpansionState.expandedDirectoryKeys);
  const hasInitializedGroupExpansion = useRef(
    initialGroupExpansionState.restoredFromStorage,
  );

  useEffect(() => {
    let cancelled = false;
    void checkForAvailableUpdate().then((update) => {
      if (!cancelled) setAvailableUpdate(update);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    setProviderFilter(appId as ProviderFilter);
  }, [appId]);

  const [searchQuery, setSearchQuery] = useState("");
  const deferredSearchQuery = useDeferredValue(searchQuery);
  const [statusFilter, setStatusFilter] = useState<SessionStatusFilter>("all");
  const [sidebarWidth, setSidebarWidth] = useState<number>(
    readInitialSidebarWidth,
  );
  const [isResizing, setIsResizing] = useState(false);
  const sidebarWidthRef = useRef(sidebarWidth);
  sidebarWidthRef.current = sidebarWidth;
  const resizeCleanupRef = useRef<(() => void) | null>(null);

  const contentSearch = useSessionContentSearch({
    query: searchQuery,
    providerFilter,
    listIsSuccess: sessionsIsSuccess,
    listUpdatedAt: dataUpdatedAt,
  });

  useEffect(() => {
    return () => {
      resizeCleanupRef.current?.();
      resizeCleanupRef.current = null;
    };
  }, []);

  const handleResizeStart = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      event.preventDefault();
      resizeCleanupRef.current?.();
      const startX = event.clientX;
      const sidebarElement =
        (event.currentTarget.previousElementSibling as HTMLElement | null) ??
        document.querySelector<HTMLElement>(".asm-list");
      const renderedWidth = sidebarElement?.getBoundingClientRect().width;
      const startWidth =
        renderedWidth && renderedWidth > 0
          ? clampSidebarWidth(renderedWidth)
          : sidebarWidthRef.current;
      sidebarWidthRef.current = startWidth;
      setSidebarWidth(startWidth);
      setIsResizing(true);

      const handlePointerMove = (moveEvent: PointerEvent) => {
        const nextWidth = clampSidebarWidth(
          startWidth + (moveEvent.clientX - startX),
        );
        sidebarWidthRef.current = nextWidth;
        setSidebarWidth(nextWidth);
      };

      const handlePointerUp = () => {
        window.removeEventListener("pointermove", handlePointerMove);
        window.removeEventListener("pointerup", handlePointerUp);
        window.removeEventListener("pointercancel", handlePointerUp);
        resizeCleanupRef.current = null;
        setIsResizing(false);
        safeLocalStorageSet(
          SIDEBAR_WIDTH_STORAGE_KEY,
          String(sidebarWidthRef.current),
        );
      };

      window.addEventListener("pointermove", handlePointerMove);
      window.addEventListener("pointerup", handlePointerUp);
      window.addEventListener("pointercancel", handlePointerUp);
      resizeCleanupRef.current = () => {
        window.removeEventListener("pointermove", handlePointerMove);
        window.removeEventListener("pointerup", handlePointerUp);
        window.removeEventListener("pointercancel", handlePointerUp);
      };
    },
    [],
  );

  const handleResizeReset = useCallback(() => {
    const resetWidth = clampSidebarWidth(DEFAULT_SIDEBAR_WIDTH);
    setSidebarWidth(resetWidth);
    sidebarWidthRef.current = resetWidth;
    safeLocalStorageSet(SIDEBAR_WIDTH_STORAGE_KEY, String(resetWidth));
  }, []);

  const handleResizeKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      const step = event.shiftKey ? 32 : 16;
      const delta = event.key === "ArrowRight" ? step : -step;
      const nextWidth = clampSidebarWidth(sidebarWidthRef.current + delta);
      sidebarWidthRef.current = nextWidth;
      setSidebarWidth(nextWidth);
      safeLocalStorageSet(SIDEBAR_WIDTH_STORAGE_KEY, String(nextWidth));
    },
    [],
  );

  const searchTerms = useMemo(
    () =>
      deferredSearchQuery
        .trim()
        .toLocaleLowerCase()
        .split(/\s+/)
        .filter(Boolean),
    [deferredSearchQuery],
  );
  const isSearching = searchTerms.length > 0;

  const searchIndex = useMemo(
    () =>
      sessions.map((session) => ({
        session,
        haystack: [
          session.title,
          session.summary,
          session.projectDir,
          session.projectName,
          session.sessionId,
          session.sourcePath,
          session.sidebarSection?.name,
        ]
          .filter(Boolean)
          .join("\n")
          .toLocaleLowerCase(),
      })),
    [sessions],
  );

  const providerScopedIndex = useMemo(
    () =>
      searchIndex.filter(
        ({ session }) =>
          providerFilter === "all" || session.providerId === providerFilter,
      ),
    [providerFilter, searchIndex],
  );

  const statusCounts = useMemo(() => {
    let residual = 0;
    let cleanupPending = 0;
    let archived = 0;
    let child = 0;
    for (const { session } of providerScopedIndex) {
      if (session.residual && !session.archived && !session.parentSessionId) {
        residual += 1;
      }
      if (session.cleanupPending) cleanupPending += 1;
      if (session.archived) archived += 1;
      if (session.parentSessionId) child += 1;
    }
    return {
      all: providerScopedIndex.length,
      residual,
      cleanupPending,
      archived,
      child,
    };
  }, [providerScopedIndex]);

  useEffect(() => {
    if (statusFilter !== "all" && statusCounts[statusFilter] === 0) {
      setStatusFilter("all");
    }
  }, [statusCounts, statusFilter]);

  const filteredSessions = useMemo(() => {
    const list = providerScopedIndex
      .filter(({ session, haystack }) => {
        if (statusFilter === "residual") {
          if (!session.residual || session.archived || session.parentSessionId)
            return false;
        } else if (statusFilter === "cleanupPending") {
          if (!session.cleanupPending) return false;
        } else if (statusFilter === "archived") {
          if (!session.archived) return false;
        } else if (statusFilter === "child") {
          if (!session.parentSessionId) return false;
        }
        if (searchTerms.length === 0) return true;
        const metaMatch = searchTerms.every((term) => haystack.includes(term));
        if (metaMatch) return true;
        if (contentSearch.enabled) {
          return contentSearch.hits.has(
            getContentHitKey(session.providerId, session.sourcePath),
          );
        }
        return false;
      })
      .map(({ session }) => session);
    return list.sort(
      (a, b) =>
        (b.lastActiveAt ?? b.createdAt ?? 0) -
        (a.lastActiveAt ?? a.createdAt ?? 0),
    );
  }, [
    contentSearch.enabled,
    contentSearch.hits,
    providerScopedIndex,
    searchTerms,
    statusFilter,
  ]);

  const groupedSessions = useMemo(
    () =>
      groupSessionsByProviderAndDirectory(
        filteredSessions,
        t("sessionManager.unknownDirectory", {
          defaultValue: "未知目录",
        }),
        t("sessionManager.ungroupedDirectory", {
          defaultValue: "未分组",
        }),
      ),
    [filteredSessions, t],
  );

  const timeGroupedSessions = useMemo(
    () =>
      groupSessionsByTime(filteredSessions, {
        today: t("sessionManager.timeToday", { defaultValue: "今天" }),
        yesterday: t("sessionManager.timeYesterday", { defaultValue: "昨天" }),
        thisWeek: t("sessionManager.timeThisWeek", { defaultValue: "本周" }),
        older: t("sessionManager.timeOlder", { defaultValue: "更早" }),
      }),
    [filteredSessions, t],
  );

  const validGroupExpansionKeys = useMemo(
    () => ({
      providerIds: new Set(sessions.map((session) => session.providerId)),
      directoryKeys: new Set(
        sessions.map((session) =>
          getSessionDirectoryGroupKey(
            session.providerId,
            session.projectDir,
            session.projectName,
            session.sidebarSection,
          ),
        ),
      ),
    }),
    [sessions],
  );

  useEffect(() => {
    safeLocalStorageSet(SESSION_LIST_VIEW_MODE_STORAGE_KEY, listViewMode);
  }, [listViewMode]);

  useEffect(() => {
    safeLocalStorageSet(
      SESSION_GROUP_EXPANSION_STORAGE_KEY,
      serializeSessionGroupExpansionState(
        expandedProviderGroups,
        expandedDirectoryGroups,
      ),
    );
  }, [expandedDirectoryGroups, expandedProviderGroups]);

  const wasSearchingRef = useRef(false);
  useEffect(() => {
    if (isSearching && !wasSearchingRef.current && groupedSessions.length > 0) {
      setExpandedProviderGroups((current) => {
        const next = new Set(current);
        groupedSessions.forEach((group) => next.add(group.providerId));
        return next;
      });
      setExpandedDirectoryGroups((current) => {
        const next = new Set(current);
        groupedSessions.forEach((group) =>
          group.directories.forEach((dir) => next.add(dir.key)),
        );
        return next;
      });
    }
    wasSearchingRef.current = isSearching;
  }, [groupedSessions, isSearching]);

  useEffect(() => {
    if (isLoading) return;

    if (!hasInitializedGroupExpansion.current && sessions.length > 0) {
      setExpandedProviderGroups(new Set(validGroupExpansionKeys.providerIds));
      setExpandedDirectoryGroups(
        new Set(validGroupExpansionKeys.directoryKeys),
      );
      hasInitializedGroupExpansion.current = true;
      return;
    }

    setExpandedProviderGroups((current) =>
      filterSetToAllowedValues(current, validGroupExpansionKeys.providerIds),
    );
    setExpandedDirectoryGroups((current) =>
      filterSetToAllowedValues(current, validGroupExpansionKeys.directoryKeys),
    );
  }, [isLoading, sessions.length, validGroupExpansionKeys]);

  const navigableSessions = useMemo(() => {
    if (listViewMode === "time") {
      return timeGroupedSessions.flatMap((group) => group.sessions);
    }
    if (listViewMode !== "grouped") {
      return filteredSessions;
    }
    const visible: SessionMeta[] = [];
    for (const providerGroup of groupedSessions) {
      if (!expandedProviderGroups.has(providerGroup.providerId)) continue;
      for (const directoryGroup of providerGroup.directories) {
        if (!expandedDirectoryGroups.has(directoryGroup.key)) continue;
        visible.push(...directoryGroup.sessions);
      }
    }
    return visible;
  }, [
    expandedDirectoryGroups,
    expandedProviderGroups,
    filteredSessions,
    groupedSessions,
    listViewMode,
    timeGroupedSessions,
  ]);

  const prevOrderedKeysRef = useRef<string[]>([]);
  const keyboardNavigatedKeyRef = useRef<string | null>(null);

  useEffect(() => {
    if (filteredSessions.length === 0) {
      prevOrderedKeysRef.current = [];
      setSelectedKey(null);
      return;
    }

    const visualOrder =
      listViewMode === "grouped"
        ? groupedSessions.flatMap((providerGroup) =>
            providerGroup.directories.flatMap(
              (directoryGroup) => directoryGroup.sessions,
            ),
          )
        : listViewMode === "time"
          ? timeGroupedSessions.flatMap((group) => group.sessions)
          : filteredSessions;
    const currentOrderKeys = visualOrder.map(getSessionKey);
    const currentKeySet = new Set(currentOrderKeys);
    const exists = selectedKey ? currentKeySet.has(selectedKey) : false;

    if (!exists) {
      let nextKey: string | null = null;
      if (selectedKey) {
        const prevKeys = prevOrderedKeysRef.current;
        const oldIndex = prevKeys.indexOf(selectedKey);
        if (oldIndex >= 0) {
          for (let i = oldIndex + 1; i < prevKeys.length; i += 1) {
            if (currentKeySet.has(prevKeys[i])) {
              nextKey = prevKeys[i];
              break;
            }
          }
          if (!nextKey) {
            for (let i = oldIndex - 1; i >= 0; i -= 1) {
              if (currentKeySet.has(prevKeys[i])) {
                nextKey = prevKeys[i];
                break;
              }
            }
          }
        }
      }
      setSelectedKey(
        nextKey ??
          getSessionKey(
            navigableSessions[0] ?? visualOrder[0] ?? filteredSessions[0],
          ),
      );
    }

    prevOrderedKeysRef.current = currentOrderKeys;
  }, [
    filteredSessions,
    groupedSessions,
    listViewMode,
    navigableSessions,
    selectedKey,
    timeGroupedSessions,
  ]);

  useEffect(() => {
    if (!selectedKey || keyboardNavigatedKeyRef.current !== selectedKey) {
      return;
    }
    keyboardNavigatedKeyRef.current = null;
    const selectedEl = document.querySelector<HTMLElement>(
      ".asm-list .asm-session-item.is-selected",
    );
    selectedEl?.scrollIntoView?.({ block: "nearest" });
  }, [selectedKey]);

  const selectedSession = useMemo(() => {
    if (!selectedKey) return null;
    return (
      filteredSessions.find(
        (session) => getSessionKey(session) === selectedKey,
      ) || null
    );
  }, [filteredSessions, selectedKey]);
  const isCodexSession = selectedSession?.providerId === "codex";

  const selectedSessionChildCount = useMemo(
    () =>
      selectedSession
        ? collectSessionDescendants(sessions, [selectedSession]).length
        : 0,
    [selectedSession, sessions],
  );

  const sessionTitles = useMemo(
    () => buildSessionTitleIndex(sessions),
    [sessions],
  );

  const deletion = useSessionDeletion({
    sessions,
    sessionTitles,
    setSelectedSessionKeys,
  });

  const providerFilterLabel =
    providerFilter === "all"
      ? t("sessionManager.providerFilterAll", { defaultValue: "全部 Agent" })
      : getProviderLabel(providerFilter, t);

  const {
    data: messages = [],
    isLoading: isLoadingMessages,
    isFetching: isFetchingMessages,
    messageRevision,
    error: messagesError,
    refetch: refetchMessages,
  } = useSessionMessagesQuery(
    selectedSession?.providerId,
    selectedSession?.sourcePath,
  );
  const visibleMessages = useMemo(
    () => getVisibleSessionMessages(messages, isCodexSession),
    [isCodexSession, messages],
  );

  useEffect(() => {
    const validKeys = new Set(
      sessions.map((session) => getSessionKey(session)),
    );
    setSelectedSessionKeys((current) => {
      let changed = false;
      const next = new Set<string>();
      current.forEach((key) => {
        if (validKeys.has(key)) {
          next.add(key);
        } else {
          changed = true;
        }
      });
      return changed ? next : current;
    });
  }, [sessions]);

  const handleCopy = useCallback(
    async (text: string, successMessage: string) => {
      try {
        await navigator.clipboard.writeText(text);
        toast.success(successMessage);
      } catch (error) {
        toast.error(
          extractErrorMessage(error) ||
            t("sessionManager.copyFailed", { defaultValue: "复制失败" }),
        );
      }
    },
    [t],
  );

  const handleCopyMessage = useCallback(
    (content: string) => {
      void handleCopy(
        content,
        t("sessionManager.messageCopied", {
          defaultValue: "已复制消息内容",
        }),
      );
    },
    [handleCopy, t],
  );

  const handleCopyCode = useCallback(
    (code: string) => {
      return handleCopy(
        code,
        t("sessionManager.codeCopied", {
          defaultValue: "已复制代码片段",
        }),
      );
    },
    [handleCopy, t],
  );

  const handleRevealPath = useCallback(
    async (path: string) => {
      try {
        await sessionsApi.revealPath(path);
      } catch (error) {
        toast.error(
          t("sessionManager.revealFailed", {
            defaultValue: "无法在文件管理器中定位：{{error}}",
            error: extractErrorMessage(error),
          }),
        );
      }
    },
    [t],
  );

  const handleNotifyExport = useCallback(
    (filename: string) => {
      toast.success(
        t("sessionManager.markdownExported", {
          defaultValue: "已导出 {{filename}}",
          filename,
        }),
      );
    },
    [t],
  );

  const handleSelectSnippet = useCallback(
    (sessionKey: string, transcriptIndex: number) => {
      setSelectedKey(sessionKey);
      setPendingJumpTarget({
        sessionKey,
        transcriptIndex,
        query: searchQuery.trim(),
        nonce: Date.now(),
      });
    },
    [searchQuery],
  );

  const fullContentCache = useMemo(
    () => new Map<string, Promise<string>>(),
    [selectedKey, messageRevision],
  );
  const currentFullContentCache = useRef(fullContentCache);
  currentFullContentCache.current = fullContentCache;

  const handleLoadFullContent = useCallback(
    (message: SessionMessage): Promise<string> => {
      const providerId = selectedSession?.providerId;
      const sourcePath = selectedSession?.sourcePath;
      if (!providerId || !sourcePath || message.index === undefined) {
        return Promise.resolve(message.content);
      }

      const contentChars = message.contentChars ?? message.content.length;
      const cacheKey = `${message.index}\u0000${contentChars}\u0000${message.contentIdentity ?? ""}`;
      const cached = fullContentCache.get(cacheKey);
      if (cached !== undefined) return cached;

      const request = sessionsApi
        .getMessageContent(
          providerId,
          sourcePath,
          message.index,
          contentChars,
          message.contentIdentity,
        )
        .catch((error) => {
          fullContentCache.delete(cacheKey);
          if (currentFullContentCache.current === fullContentCache) {
            toast.error(
              extractErrorMessage(error) ||
                t("sessionManager.loadFullContentFailed", {
                  defaultValue: "读取完整内容失败",
                }),
            );
          }
          throw error;
        });
      fullContentCache.set(cacheKey, request);
      return request;
    },
    [
      fullContentCache,
      selectedSession?.providerId,
      selectedSession?.sourcePath,
      t,
    ],
  );

  const handleOpenUrl = useCallback(
    async (url: string) => {
      try {
        const parsed = new URL(url);
        if (parsed.protocol !== "https:") {
          throw new Error(
            t("sessionManager.unsupportedUrlProtocol", {
              defaultValue: "仅允许打开 HTTPS 链接",
            }),
          );
        }
        await openUrl(url);
      } catch (error) {
        toast.error(
          t("sessionManager.openLinkFailed", {
            defaultValue: "无法打开链接：{{error}}",
            error: extractErrorMessage(error),
          }),
        );
      }
    },
    [t],
  );

  const deletableFilteredSessions = useMemo(
    () => filteredSessions.filter((session) => Boolean(session.sourcePath)),
    [filteredSessions],
  );

  const selectedSessions = useMemo(
    () =>
      sessions.filter((session) =>
        selectedSessionKeys.has(getSessionKey(session)),
      ),
    [sessions, selectedSessionKeys],
  );

  const providerCount = useMemo(
    () => new Set(sessions.map((session) => session.providerId)).size,
    [sessions],
  );

  const providerSessionCounts = useMemo(
    () =>
      sessions.reduce<Record<string, number>>((counts, session) => {
        counts[session.providerId] = (counts[session.providerId] ?? 0) + 1;
        return counts;
      }, {}),
    [sessions],
  );

  const updatedAtLabel = useMemo(() => {
    if (!dataUpdatedAt) return "";
    return new Date(dataUpdatedAt).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }, [dataUpdatedAt]);

  const selectedDeletableSessions = useMemo(
    () => selectedSessions.filter((session) => Boolean(session.sourcePath)),
    [selectedSessions],
  );

  useEffect(() => {
    if (!selectionMode) return;

    const visibleKeys = new Set(
      deletableFilteredSessions.map((session) => getSessionKey(session)),
    );

    setSelectedSessionKeys((current) => {
      let changed = false;
      const next = new Set<string>();

      current.forEach((key) => {
        if (visibleKeys.has(key)) {
          next.add(key);
        } else {
          changed = true;
        }
      });

      return changed ? next : current;
    });
  }, [deletableFilteredSessions, selectionMode]);

  const allFilteredSelected =
    deletableFilteredSessions.length > 0 &&
    deletableFilteredSessions.every((session) =>
      selectedSessionKeys.has(getSessionKey(session)),
    );

  const toggleSessionChecked = useCallback(
    (session: SessionMeta, checked: boolean) => {
      if (!session.sourcePath) return;
      const key = getSessionKey(session);
      setSelectedSessionKeys((current) => {
        const next = new Set(current);
        if (checked) {
          next.add(key);
        } else {
          next.delete(key);
        }
        return next;
      });
    },
    [],
  );

  const toggleSessionGroupChecked = useCallback(
    (groupSessions: SessionMeta[], checked: boolean) => {
      const selectableSessions = groupSessions.filter((session) =>
        Boolean(session.sourcePath),
      );
      if (selectableSessions.length === 0) return;

      setSelectedSessionKeys((current) => {
        const next = new Set(current);
        selectableSessions.forEach((session) => {
          const sessionKey = getSessionKey(session);
          if (checked) {
            next.add(sessionKey);
          } else {
            next.delete(sessionKey);
          }
        });
        return next;
      });
    },
    [],
  );

  const toggleProviderGroup = useCallback((providerId: string) => {
    setExpandedProviderGroups((current) => {
      const next = new Set(current);
      if (next.has(providerId)) {
        next.delete(providerId);
      } else {
        next.add(providerId);
      }
      return next;
    });
  }, []);

  const toggleDirectoryGroup = useCallback((directoryKey: string) => {
    setExpandedDirectoryGroups((current) => {
      const next = new Set(current);
      if (next.has(directoryKey)) {
        next.delete(directoryKey);
      } else {
        next.add(directoryKey);
      }
      return next;
    });
  }, []);

  const handleToggleSelectAll = useCallback(() => {
    setSelectedSessionKeys((current) => {
      const next = new Set(current);
      if (allFilteredSelected) {
        deletableFilteredSessions.forEach((session) =>
          next.delete(getSessionKey(session)),
        );
      } else {
        deletableFilteredSessions.forEach((session) =>
          next.add(getSessionKey(session)),
        );
      }
      return next;
    });
  }, [allFilteredSelected, deletableFilteredSessions]);

  const hasBatchSelection =
    selectionMode && selectedDeletableSessions.length > 0;

  const openBatchDeleteDialog = useCallback(() => {
    if (selectedDeletableSessions.length === 0) return;
    deletion.openDeleteDialog(selectedDeletableSessions);
  }, [deletion, selectedDeletableSessions]);

  const handleSingleDelete = useCallback(
    (session: SessionMeta) => {
      deletion.openDeleteDialog([session]);
    },
    [deletion],
  );

  const exitSelectionMode = useCallback(() => {
    setSelectionMode(false);
    setSelectedSessionKeys(new Set());
  }, []);

  const allGroupsExpanded = useMemo(
    () =>
      groupedSessions.length > 0 &&
      groupedSessions.every(
        (providerGroup) =>
          expandedProviderGroups.has(providerGroup.providerId) &&
          providerGroup.directories.every((directoryGroup) =>
            expandedDirectoryGroups.has(directoryGroup.key),
          ),
      ),
    [expandedDirectoryGroups, expandedProviderGroups, groupedSessions],
  );

  const handleToggleAllGroups = useCallback(() => {
    if (allGroupsExpanded) {
      setExpandedProviderGroups(new Set());
      setExpandedDirectoryGroups(new Set());
      return;
    }
    const nextProviders = new Set(
      groupedSessions.map((providerGroup) => providerGroup.providerId),
    );
    const nextDirectories = new Set(
      groupedSessions.flatMap((providerGroup) =>
        providerGroup.directories.map((directoryGroup) => directoryGroup.key),
      ),
    );
    setExpandedProviderGroups(nextProviders);
    setExpandedDirectoryGroups(nextDirectories);
  }, [allGroupsExpanded, groupedSessions]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (deletion.isDialogOpen) return;
      const target = event.target as HTMLElement | null;
      const tagName = target?.tagName;
      if (
        tagName === "INPUT" ||
        tagName === "TEXTAREA" ||
        tagName === "SELECT" ||
        target?.isContentEditable ||
        target?.getAttribute("role") === "combobox" ||
        Boolean(target?.closest('[role="listbox"], [role="dialog"]'))
      ) {
        return;
      }
      if (event.key === "Escape" && selectionMode) {
        exitSelectionMode();
        return;
      }
      if (
        (event.key === "ArrowDown" || event.key === "ArrowUp") &&
        !target?.closest(".asm-detail") &&
        target?.getAttribute("role") !== "separator" &&
        navigableSessions.length > 0
      ) {
        event.preventDefault();
        const currentIndex = selectedKey
          ? navigableSessions.findIndex(
              (session) => getSessionKey(session) === selectedKey,
            )
          : -1;
        const nextIndex =
          event.key === "ArrowDown"
            ? currentIndex < navigableSessions.length - 1
              ? currentIndex + 1
              : 0
            : currentIndex > 0
              ? currentIndex - 1
              : navigableSessions.length - 1;
        const nextKey = getSessionKey(navigableSessions[nextIndex]);
        keyboardNavigatedKeyRef.current = nextKey;
        setSelectedKey(nextKey);
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [
    deletion.isDialogOpen,
    exitSelectionMode,
    navigableSessions,
    selectedKey,
    selectionMode,
  ]);

  const handleRescan = useCallback(async () => {
    if (rescanInFlightRef.current) {
      return rescanInFlightRef.current;
    }

    const task = (async () => {
      const before = new Set(sessions.map(getSessionKey));
      try {
        const result = await refetch();
        if (result.isError) {
          throw result.error;
        }
        const nextSessions = result.data ?? [];
        await queryClient.invalidateQueries({ queryKey: ["sessionMessages"] });
        await queryClient.invalidateQueries({
          queryKey: piKeys.sessionDiscovery,
        });
        const after = new Set(nextSessions.map(getSessionKey));
        const added = [...after].filter((key) => !before.has(key)).length;
        const removed = [...before].filter((key) => !after.has(key)).length;

        if (added === 0 && removed === 0) {
          toast.success(
            t("sessionManager.rescanNoChanges", {
              defaultValue: "扫描完成：没有发现新会话，共 {{count}} 条",
              count: nextSessions.length,
            }),
          );
        } else {
          toast.success(
            t("sessionManager.rescanDelta", {
              defaultValue:
                "扫描完成：新增 {{added}} 条，移除 {{removed}} 条，共 {{count}} 条",
              added,
              removed,
              count: nextSessions.length,
            }),
          );
        }
      } catch (error) {
        toast.error(
          t("sessionManager.rescanFailed", {
            defaultValue: "扫描失败：{{error}}",
            error: extractErrorMessage(error),
          }),
        );
      }
    })();
    rescanInFlightRef.current = task;
    try {
      await task;
    } finally {
      if (rescanInFlightRef.current === task) {
        rescanInFlightRef.current = null;
      }
    }
  }, [queryClient, refetch, sessions, t]);

  return (
    <TooltipProvider>
      <div className="asm-shell">
        <section className="asm-workspace" onWheel={(e) => e.stopPropagation()}>
          <AppTitleBar
            availableUpdate={availableUpdate}
            theme={theme}
            resolvedTheme={resolvedTheme}
            onSetTheme={setTheme}
            onOpenUrl={handleOpenUrl}
          />

          <div className="asm-dashboard">
            {includesPiSessions &&
              piSessionDiscovery.data?.status ===
                "requires_project_context" && (
                <div className="asm-banner-wrap">
                  <div
                    role="status"
                    className="flex shrink-0 items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-800 dark:text-amber-200"
                  >
                    <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                    <span>
                      {t("sessionManager.piRelativeSessionDir")}{" "}
                      <code>{piSessionDiscovery.data.configuredPath}</code>
                    </span>
                  </div>
                </div>
              )}
            {includesPiSessions &&
              (piSessionDiscovery.data?.status === "unavailable" ||
                piSessionDiscovery.isError) && (
                <div className="asm-banner-wrap">
                  <div
                    role="alert"
                    className="flex shrink-0 items-start gap-2 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-xs text-red-800 dark:text-red-200"
                  >
                    <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                    <span>
                      {t("sessionManager.piDiscoveryUnavailable", {
                        error:
                          piSessionDiscovery.data?.status === "unavailable"
                            ? piSessionDiscovery.data.reason
                            : extractErrorMessage(piSessionDiscovery.error),
                      })}
                    </span>
                  </div>
                </div>
              )}
            <div
              className={cn("asm-session-grid", isResizing && "is-resizing")}
              style={
                {
                  "--asm-sidebar-width": `${sidebarWidth}px`,
                } as CSSProperties
              }
            >
              <SessionListSidebar
                sessions={sessions}
                filteredSessions={filteredSessions}
                groupedSessions={groupedSessions}
                timeGroupedSessions={timeGroupedSessions}
                providerFilter={providerFilter}
                onProviderFilterChange={setProviderFilter}
                providerFilterLabel={providerFilterLabel}
                providerSessionCounts={providerSessionCounts}
                selectionMode={selectionMode}
                onEnterSelectionMode={() => setSelectionMode(true)}
                onExitSelectionMode={exitSelectionMode}
                listViewMode={listViewMode}
                onListViewModeChange={setListViewMode}
                isLoading={isLoading}
                isFetching={isFetching}
                sessionsError={sessionsError}
                scanWarnings={scanWarnings}
                onRescan={() => void handleRescan()}
                deletableFilteredSessions={deletableFilteredSessions}
                selectedDeletableSessions={selectedDeletableSessions}
                allFilteredSelected={allFilteredSelected}
                onToggleSelectAll={handleToggleSelectAll}
                onClearSelection={() => setSelectedSessionKeys(new Set())}
                searchQuery={searchQuery}
                onSearchQueryChange={setSearchQuery}
                searchTerms={searchTerms}
                isSearching={isSearching}
                contentSearchEnabled={contentSearch.enabled}
                onContentSearchEnabledChange={contentSearch.setEnabled}
                contentHits={contentSearch.hits}
                contentIndexStatus={contentSearch.status}
                isContentSearching={contentSearch.isSearching}
                contentSearchError={contentSearch.error}
                onRetryContentSearch={contentSearch.retry}
                statusFilter={statusFilter}
                onStatusFilterChange={setStatusFilter}
                statusCounts={statusCounts}
                allGroupsExpanded={allGroupsExpanded}
                onToggleAllGroups={handleToggleAllGroups}
                expandedProviderGroups={expandedProviderGroups}
                expandedDirectoryGroups={expandedDirectoryGroups}
                onToggleProviderGroup={toggleProviderGroup}
                onToggleDirectoryGroup={toggleDirectoryGroup}
                selectedKey={selectedKey}
                selectedSessionKeys={selectedSessionKeys}
                sessionTitles={sessionTitles}
                onSelectSession={setSelectedKey}
                onSelectSnippet={handleSelectSnippet}
                onToggleSessionChecked={toggleSessionChecked}
                onToggleSessionGroupChecked={toggleSessionGroupChecked}
                updatedAtLabel={updatedAtLabel}
                providerCount={providerCount}
              />

              <div
                role="separator"
                tabIndex={0}
                aria-orientation="vertical"
                aria-label={t("sessionManager.resizeSidebar", {
                  defaultValue: "调整侧边栏宽度",
                })}
                aria-valuenow={sidebarWidth}
                aria-valuemin={MIN_SIDEBAR_WIDTH}
                aria-valuemax={getEffectiveMaxSidebarWidth()}
                className="asm-resizer"
                onPointerDown={handleResizeStart}
                onDoubleClick={handleResizeReset}
                onKeyDown={handleResizeKeyDown}
              />

              <SessionDetailPane
                selectedSession={selectedSession}
                selectedKey={selectedKey}
                isCodexSession={isCodexSession}
                selectedSessionChildCount={selectedSessionChildCount}
                sessionTitles={sessionTitles}
                visibleMessages={visibleMessages}
                isLoadingMessages={isLoadingMessages}
                isFetchingMessages={isFetchingMessages}
                messageRevision={messageRevision}
                messagesError={messagesError}
                onRefetchMessages={() => void refetchMessages()}
                onLoadFullContent={handleLoadFullContent}
                hasBatchSelection={hasBatchSelection}
                selectedDeletableCount={selectedDeletableSessions.length}
                isDeleting={deletion.isDeleting}
                onBatchDelete={openBatchDeleteDialog}
                onSingleDelete={handleSingleDelete}
                onCopy={(text, message) => void handleCopy(text, message)}
                onCopyMessage={handleCopyMessage}
                onCopyCode={handleCopyCode}
                onOpenUrl={handleOpenUrl}
                onRevealPath={(path) => void handleRevealPath(path)}
                onNotifyExport={handleNotifyExport}
                pendingJumpTarget={pendingJumpTarget}
                onConsumeJumpTarget={() => setPendingJumpTarget(null)}
              />
            </div>
          </div>
        </section>

        <ConfirmDialog
          isOpen={deletion.isDialogOpen}
          pending={deletion.isDeleting}
          title={deletion.dialogTitle}
          message={deletion.dialogMessage}
          details={deletion.deleteConfirmDetails?.items}
          detailsOverflowLabel={deletion.deleteConfirmDetails?.overflowLabel}
          checkboxes={deletion.deleteDialogCheckboxes}
          confirmText={deletion.dialogConfirmText}
          cancelText={t("common.cancel", { defaultValue: "取消" })}
          variant="destructive"
          onConfirm={() => void deletion.handleDeleteConfirm()}
          onCancel={deletion.closeDeleteDialog}
        />
      </div>
    </TooltipProvider>
  );
}
