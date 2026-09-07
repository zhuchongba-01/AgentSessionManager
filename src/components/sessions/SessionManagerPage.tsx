import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import { toast } from "sonner";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  AlertTriangle,
  RefreshCw,
  Trash2,
  MessageSquare,
  Clock,
  FolderOpen,
  FileText,
  CheckSquare,
  Boxes,
  ListTree,
  List,
  ChevronDown,
  ChevronRight,
  Moon,
  Sun,
  Minus,
  Square,
  X,
  Download,
  Github,
  Terminal,
} from "lucide-react";
import appIcon from "@/icons/app-icon.png";
import {
  useDeleteSessionMutation,
  useSessionMessagesQuery,
  useSessionsQuery,
} from "@/lib/query/sessions";
import { piKeys } from "@/lib/query/pi";
import { piApi } from "@/lib/api/pi";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMeta } from "@/types";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { extractErrorMessage } from "@/utils/errorUtils";
import { isMac } from "@/lib/platform";
import {
  APP_VERSION,
  checkForAvailableUpdate,
  GITHUB_LATEST_RELEASE_URL,
  GITHUB_REPOSITORY_URL,
  type AvailableUpdate,
} from "@/lib/githubRelease";
import { useTheme } from "@/components/theme-provider";
import { AgentIcon } from "./AgentIcon";
import { SessionItem } from "./SessionItem";
import { SessionRows } from "./SessionRows";
import { SessionStatusBadges } from "./SessionStatusBadges";
import { SessionMessageItem } from "./SessionMessageItem";
import { SessionMinimap } from "./SessionMinimap";
import {
  extractCodexPromptPreview,
  formatMessageTimestamp,
  formatSessionMessagePreview,
  formatSessionTitle,
  getBaseName,
  getVisibleSessionMessages,
  getProviderIconName,
  getProviderLabel,
  getSessionDirectoryGroupKey,
  getSessionKey,
  groupSessionsByProviderAndDirectory,
  type SessionDirectoryGroup,
  type SessionProviderGroup,
} from "./utils";

const SESSION_LIST_VIEW_MODE_STORAGE_KEY = "agent-session-manager.listViewMode";
const SESSION_GROUP_EXPANSION_STORAGE_KEY =
  "agent-session-manager.groupExpansionState";

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

type ProviderFilter =
  | "all"
  | "codex"
  | "grokbuild"
  | "claude"
  | "opencode"
  | "pi"
  | "zcode";

type SessionListViewMode = "flat" | "grouped";

type GroupSelectionState = {
  checked: boolean | "indeterminate";
  isSelected: boolean;
  selectedCount: number;
  selectableCount: number;
};

type SessionGroupExpansionState = {
  expandedProviderIds: Set<string>;
  expandedDirectoryKeys: Set<string>;
  restoredFromStorage: boolean;
};

const readInitialSessionListViewMode = (): SessionListViewMode => {
  if (typeof window === "undefined") return "grouped";
  const stored = window.localStorage.getItem(
    SESSION_LIST_VIEW_MODE_STORAGE_KEY,
  );
  return stored === "grouped" || stored === "flat" ? stored : "grouped";
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
      const stored = window.localStorage.getItem(
        SESSION_GROUP_EXPANSION_STORAGE_KEY,
      );
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
  const { resolvedTheme, setTheme } = useTheme();
  const queryClient = useQueryClient();
  const {
    data,
    isLoading,
    isFetching,
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
  const detailRef = useRef<HTMLDivElement | null>(null);
  const scrollContainerRef = useRef<HTMLDivElement | null>(null);
  const rescanInFlightRef = useRef<Promise<void> | null>(null);
  const [activeMessageIndex, setActiveMessageIndex] = useState<number | null>(
    null,
  );
  // 迷你导航条：当前阅读位置对应的刻度
  // 迷你导航条显示阈值：内容达到两页高才出现
  const [minimapPages, setMinimapPages] = useState(0);
  const tickRafRef = useRef<number | null>(null);
  const [deleteTargets, setDeleteTargets] = useState<SessionMeta[] | null>(
    null,
  );
  const [selectedSessionKeys, setSelectedSessionKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [isBatchDeleting, setIsBatchDeleting] = useState(false);
  const [isResuming, setIsResuming] = useState(false);
  const [selectionMode, setSelectionMode] = useState(false);
  const [availableUpdate, setAvailableUpdate] =
    useState<AvailableUpdate | null>(null);

  const [providerFilter, setProviderFilter] = useState<ProviderFilter>(
    appId as ProviderFilter,
  );
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
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

  const filteredSessions = useMemo(() => {
    const list =
      providerFilter === "all"
        ? sessions
        : sessions.filter((session) => session.providerId === providerFilter);
    return [...list].sort(
      (a, b) =>
        (b.lastActiveAt ?? b.createdAt ?? 0) -
        (a.lastActiveAt ?? a.createdAt ?? 0),
    );
  }, [sessions, providerFilter]);

  const groupedSessions = useMemo(
    () =>
      groupSessionsByProviderAndDirectory(
        filteredSessions,
        t("sessionManager.unknownDirectory", {
          defaultValue: "未知目录",
        }),
      ),
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
          ),
        ),
      ),
    }),
    [sessions],
  );

  useEffect(() => {
    window.localStorage.setItem(
      SESSION_LIST_VIEW_MODE_STORAGE_KEY,
      listViewMode,
    );
  }, [listViewMode]);

  useEffect(() => {
    window.localStorage.setItem(
      SESSION_GROUP_EXPANSION_STORAGE_KEY,
      serializeSessionGroupExpansionState(
        expandedProviderGroups,
        expandedDirectoryGroups,
      ),
    );
  }, [expandedDirectoryGroups, expandedProviderGroups]);

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
  }, [isLoading, validGroupExpansionKeys]);

  useEffect(() => {
    if (filteredSessions.length === 0) {
      setSelectedKey(null);
      return;
    }
    const exists = selectedKey
      ? filteredSessions.some(
          (session) => getSessionKey(session) === selectedKey,
        )
      : false;
    if (!exists) {
      setSelectedKey(getSessionKey(filteredSessions[0]));
    }
  }, [filteredSessions, selectedKey]);

  const selectedSession = useMemo(() => {
    if (!selectedKey) return null;
    return (
      filteredSessions.find(
        (session) => getSessionKey(session) === selectedKey,
      ) || null
    );
  }, [filteredSessions, selectedKey]);
  const isCodexSession = selectedSession?.providerId === "codex";

  const listViewModeLabel =
    listViewMode === "grouped"
      ? t("sessionManager.viewModeGrouped", {
          defaultValue: "分类",
        })
      : t("sessionManager.viewModeFlat", {
          defaultValue: "列表",
        });
  const providerFilterLabel =
    providerFilter === "all"
      ? t("sessionManager.providerFilterAll", { defaultValue: "全部 Agent" })
      : getProviderLabel(providerFilter, t);

  const {
    data: messages = [],
    isLoading: isLoadingMessages,
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
  const deleteSessionMutation = useDeleteSessionMutation();
  const isDeleting = deleteSessionMutation.isPending || isBatchDeleting;

  const virtualizer = useVirtualizer({
    count: visibleMessages.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: () => 120,
    overscan: 5,
    gap: 12,
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

  useEffect(() => {
    if (scrollContainerRef.current) {
      scrollContainerRef.current.scrollTop = 0;
    }
  }, [selectedKey]);

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

  // 提取用户消息用于目录
  const userMessagesToc = useMemo(() => {
    return visibleMessages
      .map((msg, index) => ({ msg, index }))
      .filter(({ msg }) => msg.role.toLowerCase() === "user")
      .map(({ msg, index }) => {
        const previewContent = isCodexSession
          ? extractCodexPromptPreview(msg.content)
          : msg.content;

        return {
          index,
          preview: formatSessionMessagePreview(previewContent),
          ts: msg.ts,
        };
      });
  }, [isCodexSession, visibleMessages]);
  const minimapMessageIndexes = useMemo(
    () => userMessagesToc.map((item) => item.index),
    [userMessagesToc],
  );

  const scrollToMessage = (index: number) => {
    virtualizer.scrollToIndex(index, { align: "center", behavior: "smooth" });
    setActiveMessageIndex(index);
    setTimeout(() => setActiveMessageIndex(null), 2000);
  };

  // 滚动或内容变化时计算页数（迷你导航条的显示阈值：内容满两页才出现）
  const syncScrollMetrics = useCallback(() => {
    const el = scrollContainerRef.current;
    if (!el || visibleMessages.length === 0) {
      setMinimapPages(0);
      return;
    }
    setMinimapPages(el.scrollHeight / Math.max(1, el.clientHeight));
  }, [visibleMessages.length]);

  const handleScrollMetrics = useCallback(() => {
    if (tickRafRef.current !== null) return;
    tickRafRef.current = requestAnimationFrame(() => {
      tickRafRef.current = null;
      syncScrollMetrics();
    });
  }, [syncScrollMetrics]);

  useEffect(
    () => () => {
      if (tickRafRef.current !== null) cancelAnimationFrame(tickRafRef.current);
    },
    [],
  );

  useEffect(() => {
    syncScrollMetrics();
  }, [syncScrollMetrics, visibleMessages]);

  const handleCopy = useCallback(
    async (text: string, successMessage: string) => {
      try {
        await navigator.clipboard.writeText(text);
        toast.success(successMessage);
      } catch (error) {
        toast.error(
          extractErrorMessage(error) ||
            t("common.error", { defaultValue: "Copy failed" }),
        );
      }
    },
    [t],
  );

  const handleResume = async () => {
    if (!selectedSession?.sourcePath || isResuming || isDeleting) return;
    setIsResuming(true);
    try {
      const command = await sessionsApi.resume(
        selectedSession.providerId,
        selectedSession.sourcePath,
        isMac(),
      );
      if (isMac()) toast.success("已打开恢复终端");
      else
        await handleCopy(
          command,
          "恢复命令已复制，请粘贴到 PowerShell 或对应终端执行",
        );
    } catch (error) {
      toast.error("无法恢复会话", { description: extractErrorMessage(error) });
    } finally {
      setIsResuming(false);
    }
  };

  const handleOpenUrl = useCallback(
    async (url: string) => {
      try {
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

  const handleDeleteConfirm = async (_includeProject = false) => {
    if (!deleteTargets || deleteTargets.length === 0 || isDeleting) {
      return;
    }

    const targets = deleteTargets.filter((session) => session.sourcePath);
    if (targets.length === 0) {
      setDeleteTargets(null);
      return;
    }

    if (targets.length === 1) {
      const [target] = targets;
      try {
        await deleteSessionMutation.mutateAsync({
          providerId: target.providerId,
          sessionId: target.sessionId,
          sourcePath: target.sourcePath!,
          includeProject: false,
          sharedConfirmed: false,
        });
        setDeleteTargets(null);
        setSelectedSessionKeys((current) => {
          const next = new Set(current);
          next.delete(getSessionKey(target));
          return next;
        });
      } catch {
        // 错误已由 mutation 的 onError 提示；关闭对话框避免停留在删除中状态
        setDeleteTargets(null);
      }
      return;
    }

    setDeleteTargets(null);
    setIsBatchDeleting(true);
    try {
      const results = await sessionsApi.deleteMany(
        targets.map((session) => ({
          providerId: session.providerId,
          sessionId: session.sessionId,
          sourcePath: session.sourcePath!,
        })),
      );

      const deletedKeys = results
        .filter((result) => result.success)
        .map(
          (result) =>
            `${result.providerId}:${result.sessionId}:${result.sourcePath ?? ""}`,
        );

      const failedErrors = results
        .filter((result) => !result.success)
        .map((result) =>
          [
            result.error || t("common.unknown"),
            ...(result.warnings ?? []),
          ].join("；"),
        );

      const deleteWarnings = results
        .filter((result) => result.success)
        .flatMap((result) => result.warnings ?? []);

      if (deletedKeys.length > 0) {
        const deletedKeySet = new Set(deletedKeys);
        queryClient.setQueryData<SessionMeta[]>(["sessions"], (current) =>
          (current ?? []).filter(
            (session) => !deletedKeySet.has(getSessionKey(session)),
          ),
        );
      }

      results
        .filter((result) => result.success)
        .forEach((result) => {
          queryClient.removeQueries({
            queryKey: ["sessionMessages", result.providerId, result.sourcePath],
          });
        });

      setSelectedSessionKeys((current) => {
        const next = new Set(current);
        deletedKeys.forEach((key) => next.delete(key));
        return next;
      });

      if (deletedKeys.length > 0) {
        toast.success(
          t("sessionManager.batchDeleteSuccess", {
            defaultValue: "已删除 {{count}} 个会话",
            count: deletedKeys.length,
          }),
        );
      }

      if (deleteWarnings.length > 0) {
        toast.warning(deleteWarnings[0]);
      }

      if (failedErrors.length > 0) {
        toast.error(
          t("sessionManager.batchDeleteFailed", {
            defaultValue: "{{failed}} 个会话删除失败",
            failed: failedErrors.length,
          }),
          {
            description: failedErrors[0],
          },
        );
      }
    } catch (error) {
      toast.error(
        extractErrorMessage(error) ||
          t("sessionManager.batchDeleteRequestFailed", {
            defaultValue: "批量删除失败，请稍后重试",
          }),
      );
    } finally {
      await queryClient.invalidateQueries({ queryKey: ["sessions"] });
      await queryClient.invalidateQueries({ queryKey: ["sessionMessages"] });
      setIsBatchDeleting(false);
    }
  };

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

  const getGroupSelectionState = (
    groupSessions: SessionMeta[],
  ): GroupSelectionState => {
    const selectableSessions = groupSessions.filter((session) =>
      Boolean(session.sourcePath),
    );
    const selectedCount = selectableSessions.filter((session) =>
      selectedSessionKeys.has(getSessionKey(session)),
    ).length;
    const isSelected =
      selectableSessions.length > 0 &&
      selectedCount === selectableSessions.length;

    return {
      checked:
        selectedCount === 0 ? false : isSelected ? true : "indeterminate",
      isSelected,
      selectedCount,
      selectableCount: selectableSessions.length,
    };
  };

  const toggleSessionChecked = (session: SessionMeta, checked: boolean) => {
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
  };

  const toggleSessionGroupChecked = (
    groupSessions: SessionMeta[],
    checked: boolean,
  ) => {
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
  };

  const toggleProviderGroup = (providerId: string) => {
    setExpandedProviderGroups((current) => {
      const next = new Set(current);
      if (next.has(providerId)) {
        next.delete(providerId);
      } else {
        next.add(providerId);
      }
      return next;
    });
  };

  const toggleDirectoryGroup = (directoryKey: string) => {
    setExpandedDirectoryGroups((current) => {
      const next = new Set(current);
      if (next.has(directoryKey)) {
        next.delete(directoryKey);
      } else {
        next.add(directoryKey);
      }
      return next;
    });
  };

  const renderSessionItem = (session: SessionMeta) => {
    const sessionKey = getSessionKey(session);
    const isSelected = selectedKey !== null && sessionKey === selectedKey;

    return (
      <SessionItem
        key={sessionKey}
        session={session}
        isSelected={isSelected}
        selectionMode={selectionMode}
        isChecked={selectedSessionKeys.has(sessionKey)}
        isCheckDisabled={!session.sourcePath}
        onSelect={setSelectedKey}
        onToggleChecked={(checked) => toggleSessionChecked(session, checked)}
      />
    );
  };

  const renderGroupSelectionBadge = (
    selectionState: GroupSelectionState,
    totalCount: number,
  ) => (
    <span className="asm-count">
      {selectionMode
        ? `${selectionState.selectedCount}/${selectionState.selectableCount}`
        : totalCount}
    </span>
  );

  const renderProviderGroupCheckbox = (
    providerGroup: SessionProviderGroup,
    providerLabel: string,
    selectionState: GroupSelectionState,
  ) => {
    if (!selectionMode) return null;

    return (
      <Checkbox
        checked={selectionState.checked}
        disabled={selectionState.selectableCount === 0}
        aria-label={t("sessionManager.selectProviderGroupForBatch", {
          defaultValue: "选择 {{provider}} 供应商分组内会话",
          provider: providerLabel,
        })}
        onClick={(event) => event.stopPropagation()}
        onCheckedChange={() =>
          toggleSessionGroupChecked(
            providerGroup.sessions,
            !selectionState.isSelected,
          )
        }
      />
    );
  };

  const renderDirectoryGroupCheckbox = (
    directoryGroup: SessionDirectoryGroup,
    selectionState: GroupSelectionState,
  ) => {
    if (!selectionMode) return null;

    return (
      <Checkbox
        checked={selectionState.checked}
        disabled={selectionState.selectableCount === 0}
        aria-label={t("sessionManager.selectDirectoryGroupForBatch", {
          defaultValue: "选择 {{directory}} 目录分组内会话",
          directory: directoryGroup.label,
        })}
        onClick={(event) => event.stopPropagation()}
        onCheckedChange={() =>
          toggleSessionGroupChecked(
            directoryGroup.sessions,
            !selectionState.isSelected,
          )
        }
      />
    );
  };

  const handleToggleSelectAll = () => {
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
  };

  const openBatchDeleteDialog = () => {
    if (selectedDeletableSessions.length === 0) return;
    setDeleteTargets(selectedDeletableSessions);
  };

  const exitSelectionMode = () => {
    setSelectionMode(false);
    setSelectedSessionKeys(new Set());
  };

  const handleRescan = async () => {
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
  };

  return (
    <TooltipProvider>
      <div className="asm-shell">
        <section className="asm-workspace" onWheel={(e) => e.stopPropagation()}>
          <header
            className="asm-command-bar"
            data-tauri-drag-region
            style={isMac() ? { paddingLeft: 80 } : undefined}
          >
            {/* 品牌区：即标题栏（自绘），空余区域可拖拽窗口 */}
            <div className="asm-topbar-brand" data-tauri-drag-region>
              <img src={appIcon} alt="" className="asm-app-icon" />
              <span className="asm-app-name" data-tauri-drag-region>
                Agent会话管理器
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
                      onClick={() =>
                        void handleOpenUrl(GITHUB_LATEST_RELEASE_URL)
                      }
                    >
                      <Download className="size-4" />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent>
                    {t("sessionManager.updateTooltip", {
                      defaultValue: "发现新版本 v{{version}}，前往下载",
                      version: availableUpdate.version,
                    })}
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
            <div className="asm-topbar-actions">
              {!isMac() && (
                <div className="asm-window-controls">
                  <button
                    type="button"
                    aria-label="最小化"
                    onClick={() => windowAction("minimize")}
                  >
                    <Minus className="size-3.5" />
                  </button>
                  <button
                    type="button"
                    aria-label="最大化"
                    onClick={() => windowAction("toggleMaximize")}
                  >
                    <Square className="size-3" />
                  </button>
                  <button
                    type="button"
                    aria-label="关闭"
                    className="close"
                    onClick={() => windowAction("close")}
                  >
                    <X className="size-4" />
                  </button>
                </div>
              )}
            </div>
          </header>

          <div className="asm-dashboard">
            {includesPiSessions &&
              piSessionDiscovery.data?.status ===
                "requires_project_context" && (
                <div
                  role="status"
                  className="flex shrink-0 items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-800 dark:text-amber-200"
                >
                  <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
                  <span>
                    {t("sessionManager.piRelativeSessionDir")}{" "}
                    <code>{piSessionDiscovery.data.configuredPath}</code>
                  </span>
                </div>
              )}
            {includesPiSessions &&
              (piSessionDiscovery.data?.status === "unavailable" ||
                piSessionDiscovery.isError) && (
                <div
                  role="alert"
                  className="flex shrink-0 items-start gap-2 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-800 dark:text-red-200"
                >
                  <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
                  <span>
                    {t("sessionManager.piDiscoveryUnavailable", {
                      error:
                        piSessionDiscovery.data?.status === "unavailable"
                          ? piSessionDiscovery.data.reason
                          : extractErrorMessage(piSessionDiscovery.error),
                    })}
                  </span>
                </div>
              )}
            <div className="asm-session-grid">
              {/* 左侧会话列表 */}
              {/* 左侧菜单区域禁用浏览器右键菜单（含图标与所有行） */}
              <div
                className="asm-list"
                onContextMenu={(event) => event.preventDefault()}
              >
                <div className="asm-list-toolbar">
                  <span className="asm-list-title">
                    {t("sessionManager.sessions", { defaultValue: "会话" })}
                  </span>
                  <Select
                    value={providerFilter}
                    onValueChange={(value) =>
                      setProviderFilter(value as ProviderFilter)
                    }
                  >
                    <SelectTrigger
                      className="asm-agent-filter-trigger"
                      aria-label={t("sessionManager.providerFilterTooltip", {
                        defaultValue: "Agent 筛选",
                      })}
                    >
                      {providerFilter === "all" ? (
                        <Boxes className="size-3.5 shrink-0" />
                      ) : (
                        <AgentIcon
                          icon={getProviderIconName(providerFilter)}
                          name={providerFilterLabel}
                          size={16}
                        />
                      )}
                      <span className="truncate">
                        {providerFilter === "all"
                          ? t("sessionManager.providerFilterAll", {
                              defaultValue: "全部 Agent",
                            })
                          : providerFilterLabel}
                      </span>
                    </SelectTrigger>
                    <SelectContent className="asm-select-content asm-agent-filter-content">
                      <SelectItem value="all">
                        <div className="asm-agent-option">
                          <Boxes className="size-4" />
                          <span>
                            {t("sessionManager.providerFilterAll", {
                              defaultValue: "全部 Agent",
                            })}
                          </span>
                          <span className="asm-agent-option-count">
                            {sessions.length}
                          </span>
                        </div>
                      </SelectItem>
                      {(
                        [
                          "codex",
                          "grokbuild",
                          "claude",
                          "opencode",
                          "zcode",
                          "pi",
                        ] as const
                      ).map((providerId) => (
                        <SelectItem key={providerId} value={providerId}>
                          <div className="asm-agent-option">
                            <AgentIcon
                              icon={getProviderIconName(providerId)}
                              name={getProviderLabel(providerId, t)}
                              size={16}
                            />
                            <span>{getProviderLabel(providerId, t)}</span>
                            <span className="asm-agent-option-count">
                              {providerSessionCounts[providerId] ?? 0}
                            </span>
                          </div>
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <span className="asm-toolbar-spacer" />
                  {(selectionMode || deletableFilteredSessions.length > 0) && (
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          variant={selectionMode ? "secondary" : "ghost"}
                          size="icon"
                          className={
                            selectionMode
                              ? "size-7 bg-accent text-accent-foreground hover:bg-accent/80"
                              : "size-7 text-muted-foreground"
                          }
                          aria-label={
                            selectionMode
                              ? t("sessionManager.exitBatchModeTooltip", {
                                  defaultValue: "退出批量管理",
                                })
                              : t("sessionManager.manageBatchTooltip", {
                                  defaultValue: "批量管理",
                                })
                          }
                          onClick={() => {
                            if (selectionMode) {
                              exitSelectionMode();
                            } else {
                              setSelectionMode(true);
                            }
                          }}
                        >
                          <CheckSquare className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>
                        {selectionMode
                          ? t("sessionManager.exitBatchModeTooltip", {
                              defaultValue: "退出批量管理",
                            })
                          : t("sessionManager.manageBatchTooltip", {
                              defaultValue: "批量管理",
                            })}
                      </TooltipContent>
                    </Tooltip>
                  )}
                  <Select
                    value={listViewMode}
                    onValueChange={(value) =>
                      setListViewMode(value as SessionListViewMode)
                    }
                  >
                    <SelectTrigger
                      className="asm-list-view-trigger"
                      aria-label={t("sessionManager.viewModeTooltip", {
                        defaultValue: "查看方式",
                      })}
                      title={listViewModeLabel}
                    >
                      {listViewMode === "grouped" ? (
                        <ListTree className="size-3.5 shrink-0" />
                      ) : (
                        <List className="size-3.5 shrink-0" />
                      )}
                    </SelectTrigger>
                    <SelectContent className="asm-select-content w-40">
                      <SelectItem value="flat">
                        <div className="flex items-center gap-2">
                          <List className="size-3.5" />
                          <span>
                            {t("sessionManager.viewModeFlat", {
                              defaultValue: "列表",
                            })}
                          </span>
                        </div>
                      </SelectItem>
                      <SelectItem value="grouped">
                        <div className="flex items-center gap-2">
                          <ListTree className="size-3.5" />
                          <span>
                            {t("sessionManager.viewModeGrouped", {
                              defaultValue: "分类",
                            })}
                          </span>
                        </div>
                      </SelectItem>
                    </SelectContent>
                  </Select>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        className="asm-list-tool-button"
                        aria-label={
                          isFetching
                            ? t("sessionManager.rescanning", {
                                defaultValue: "扫描中",
                              })
                            : t("sessionManager.rescanButton", {
                                defaultValue: "重新扫描",
                              })
                        }
                        onClick={() => void handleRescan()}
                        disabled={isFetching}
                      >
                        <RefreshCw
                          className={`size-3.5 ${isFetching ? "animate-spin" : ""}`}
                        />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>
                      {isFetching
                        ? t("sessionManager.rescanning", {
                            defaultValue: "扫描中",
                          })
                        : t("sessionManager.rescanButton", {
                            defaultValue: "重新扫描",
                          })}
                    </TooltipContent>
                  </Tooltip>
                </div>
                {selectionMode && (
                  <div className="asm-batch-bar">
                    <span>
                      {t("sessionManager.selectedCount", {
                        defaultValue: "已选 {{count}} 项",
                        count: selectedDeletableSessions.length,
                      })}
                    </span>
                    {deletableFilteredSessions.length > 0 && (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-2 text-xs whitespace-nowrap"
                        onClick={handleToggleSelectAll}
                      >
                        {allFilteredSelected
                          ? t("sessionManager.clearFilteredSelection", {
                              defaultValue: "取消全选",
                            })
                          : t("sessionManager.selectAllFiltered", {
                              defaultValue: "全选筛选结果（含其他分页）",
                            })}
                      </Button>
                    )}
                    {selectedDeletableSessions.length > 0 && (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-2 text-xs whitespace-nowrap"
                        onClick={() => setSelectedSessionKeys(new Set())}
                      >
                        {t("sessionManager.clearSelection", {
                          defaultValue: "清空",
                        })}
                      </Button>
                    )}
                    <Button
                      variant="destructive"
                      size="sm"
                      className="ml-auto h-6 gap-1 px-2 whitespace-nowrap"
                      onClick={openBatchDeleteDialog}
                      disabled={
                        isDeleting || selectedDeletableSessions.length === 0
                      }
                    >
                      <Trash2 className="size-3" />
                      <span className="text-xs">
                        {isBatchDeleting
                          ? t("sessionManager.batchDeleting", {
                              defaultValue: "删除中...",
                            })
                          : t("sessionManager.deleteSelected", {
                              defaultValue: "批量删除",
                            })}
                      </span>
                    </Button>
                  </div>
                )}
                <div className="asm-list-scroll">
                  {isLoading ? (
                    <div className="flex items-center justify-center py-12">
                      <RefreshCw className="size-5 animate-spin text-muted-foreground" />
                    </div>
                  ) : sessionsError ? (
                    <div role="alert" className="p-4 text-sm text-destructive">
                      无法读取会话列表：{extractErrorMessage(sessionsError)}
                      <Button
                        variant="outline"
                        onClick={() => void handleRescan()}
                      >
                        重试扫描
                      </Button>
                    </div>
                  ) : scanWarnings.length > 0 &&
                    filteredSessions.length === 0 ? (
                    <div role="alert" className="p-3 text-sm text-amber-700">
                      扫描未完整完成：{scanWarnings.join("；")}
                    </div>
                  ) : filteredSessions.length === 0 ? (
                    <div className="flex flex-col items-center justify-center py-12 text-center">
                      <MessageSquare className="size-8 text-muted-foreground/50 mb-2" />
                      <p className="text-sm text-muted-foreground">
                        {t("sessionManager.noSessions")}
                      </p>
                    </div>
                  ) : listViewMode === "grouped" ? (
                    <div>
                      {scanWarnings.length > 0 && (
                        <div
                          role="alert"
                          className="p-3 text-sm text-amber-700"
                        >
                          扫描未完整完成：{scanWarnings.join("；")}
                        </div>
                      )}
                      {groupedSessions.map((providerGroup) => {
                        const providerOpen = expandedProviderGroups.has(
                          providerGroup.providerId,
                        );
                        const providerLabel = getProviderLabel(
                          providerGroup.providerId,
                          t,
                        );
                        const providerSelectionState = getGroupSelectionState(
                          providerGroup.sessions,
                        );

                        return (
                          <Collapsible
                            key={providerGroup.providerId}
                            open={providerOpen}
                            onOpenChange={() =>
                              toggleProviderGroup(providerGroup.providerId)
                            }
                          >
                            <div className="asm-group-row">
                              {renderProviderGroupCheckbox(
                                providerGroup,
                                providerLabel,
                                providerSelectionState,
                              )}
                              <CollapsibleTrigger asChild>
                                <button
                                  type="button"
                                  className="asm-group-trigger"
                                  aria-expanded={providerOpen}
                                  aria-label={t(
                                    "sessionManager.toggleProviderGroup",
                                    {
                                      defaultValue:
                                        "展开或折叠 {{provider}} 供应商分组",
                                      provider: providerLabel,
                                    },
                                  )}
                                >
                                  {providerOpen ? (
                                    <ChevronDown className="size-3.5 shrink-0 text-muted-foreground/70" />
                                  ) : (
                                    <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/70" />
                                  )}
                                  <AgentIcon
                                    icon={getProviderIconName(
                                      providerGroup.providerId,
                                    )}
                                    name={providerLabel}
                                    size={16}
                                  />
                                  <span className="asm-group-name">
                                    {providerLabel}
                                  </span>
                                  {renderGroupSelectionBadge(
                                    providerSelectionState,
                                    providerGroup.sessions.length,
                                  )}
                                </button>
                              </CollapsibleTrigger>
                            </div>
                            <CollapsibleContent className="asm-group-items">
                              {providerGroup.directories.map(
                                (directoryGroup) => {
                                  const directoryOpen =
                                    expandedDirectoryGroups.has(
                                      directoryGroup.key,
                                    );
                                  const directorySelectionState =
                                    getGroupSelectionState(
                                      directoryGroup.sessions,
                                    );

                                  return (
                                    <Collapsible
                                      key={directoryGroup.key}
                                      open={directoryOpen}
                                      onOpenChange={() =>
                                        toggleDirectoryGroup(directoryGroup.key)
                                      }
                                    >
                                      <div className="asm-dir-row">
                                        {renderDirectoryGroupCheckbox(
                                          directoryGroup,
                                          directorySelectionState,
                                        )}
                                        <CollapsibleTrigger asChild>
                                          <button
                                            type="button"
                                            className="asm-dir-trigger"
                                            aria-expanded={directoryOpen}
                                            aria-label={t(
                                              "sessionManager.toggleDirectoryGroup",
                                              {
                                                defaultValue:
                                                  "展开或折叠 {{directory}} 目录分组",
                                                directory: directoryGroup.label,
                                              },
                                            )}
                                          >
                                            {directoryOpen ? (
                                              <ChevronDown className="size-3.5 shrink-0 text-muted-foreground/60" />
                                            ) : (
                                              <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/60" />
                                            )}
                                            <FolderOpen className="size-4 shrink-0 text-muted-foreground/60" />
                                            <Tooltip>
                                              <TooltipTrigger asChild>
                                                <span className="asm-dir-name">
                                                  {directoryGroup.label}
                                                </span>
                                              </TooltipTrigger>
                                              <TooltipContent
                                                side="bottom"
                                                className="max-w-xs"
                                              >
                                                <p className="font-mono text-xs break-all">
                                                  {directoryGroup.projectDir ??
                                                    t(
                                                      "sessionManager.unknownDirectory",
                                                      {
                                                        defaultValue:
                                                          "未知目录",
                                                      },
                                                    )}
                                                </p>
                                              </TooltipContent>
                                            </Tooltip>
                                            {renderGroupSelectionBadge(
                                              directorySelectionState,
                                              directoryGroup.sessions.length,
                                            )}
                                          </button>
                                        </CollapsibleTrigger>
                                      </div>
                                      <CollapsibleContent className="asm-dir-items">
                                        <SessionRows
                                          sessions={directoryGroup.sessions}
                                          renderItem={renderSessionItem}
                                        />
                                      </CollapsibleContent>
                                    </Collapsible>
                                  );
                                },
                              )}
                            </CollapsibleContent>
                          </Collapsible>
                        );
                      })}
                    </div>
                  ) : (
                    <div>
                      {scanWarnings.length > 0 && (
                        <div
                          role="alert"
                          className="p-3 text-sm text-amber-700"
                        >
                          扫描未完整完成：{scanWarnings.join("；")}
                        </div>
                      )}
                      <SessionRows
                        sessions={filteredSessions}
                        renderItem={renderSessionItem}
                      />
                    </div>
                  )}
                </div>
                <div
                  className="asm-list-status"
                  title={
                    updatedAtLabel
                      ? t("sessionManager.statusbarUpdated", {
                          defaultValue: "更新于 {{time}}",
                          time: updatedAtLabel,
                        })
                      : undefined
                  }
                >
                  <span>
                    {t("sessionManager.statusbarSessions", {
                      defaultValue: "{{count}} 个会话",
                      count: sessions.length,
                    })}
                  </span>
                  <span className="asm-toolbar-spacer" />
                  <span>
                    {t("sessionManager.statusbarAgents", {
                      defaultValue: "{{count}} 个 Agent",
                      count: providerCount,
                    })}
                  </span>
                </div>
              </div>

              {/* 右侧会话详情 */}
              <div className="asm-detail" ref={detailRef}>
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
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <button
                        type="button"
                        className="asm-detail-tool"
                        aria-label={t("sessionManager.openProjectGitHub", {
                          defaultValue: "在 GitHub 查看项目",
                        })}
                        onClick={() =>
                          void handleOpenUrl(GITHUB_REPOSITORY_URL)
                        }
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
                          setTheme(resolvedTheme === "dark" ? "light" : "dark")
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
                    <p className="text-sm">
                      {t("sessionManager.selectSession")}
                    </p>
                  </div>
                ) : (
                  <>
                    {/* 详情头部 */}
                    <div className="asm-detail-head">
                      <div className="asm-detail-titlerow">
                        <h2 className="asm-detail-title">
                          {formatSessionTitle(selectedSession)}
                        </h2>
                        <SessionStatusBadges session={selectedSession} />

                        {/* 操作按钮组 */}
                        <div className="asm-detail-actions">
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={
                              isDeleting ||
                              isResuming ||
                              !selectedSession.resumeCommand ||
                              selectedSession.cleanupPending ||
                              selectedSession.archived ||
                              selectedSession.residual
                            }
                            onClick={() => void handleResume()}
                          >
                            <Terminal className="mr-1 size-3.5" />
                            {isResuming
                              ? "恢复中…"
                              : isMac()
                                ? "恢复会话"
                                : "复制恢复命令"}
                          </Button>
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <Button
                                size="sm"
                                variant="ghost"
                                className="h-8 gap-1.5 rounded-md px-3 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
                                onClick={() =>
                                  setDeleteTargets([selectedSession])
                                }
                                disabled={
                                  !selectedSession.sourcePath || isDeleting
                                }
                              >
                                <Trash2 className="size-3.5" />
                                <span>
                                  {isDeleting
                                    ? t("sessionManager.deleting", {
                                        defaultValue: "删除中...",
                                      })
                                    : selectedSession.cleanupPending
                                      ? "继续清理"
                                      : t("sessionManager.delete", {
                                          defaultValue: "删除会话",
                                        })}
                                </span>
                              </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                              {t("sessionManager.deleteTooltip", {
                                defaultValue: "永久删除此本地会话记录",
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
                            selectedSession.lastActiveAt ??
                              selectedSession.createdAt,
                          )}
                        </span>
                        {selectedSession.projectDir && (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <button
                                type="button"
                                onClick={() =>
                                  void handleCopy(
                                    selectedSession.projectDir!,
                                    t("sessionManager.projectDirCopied"),
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
                                  void handleCopy(
                                    selectedSession.sourcePath!,
                                    t("sessionManager.sourcePathCopied"),
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

                    {/* 消息列表 + 目录 */}
                    <div className="asm-detail-content">
                      {minimapPages >= 2 && (
                        <SessionMinimap
                          messages={visibleMessages}
                          messageIndexes={minimapMessageIndexes}
                          visibleMessageIndexes={viewportMessageIndexes}
                          onTickClick={scrollToMessage}
                        />
                      )}
                      <div
                        ref={scrollContainerRef}
                        onScroll={handleScrollMetrics}
                        className="asm-detail-scroll"
                      >
                        {isLoadingMessages ? (
                          <div className="flex items-center justify-center py-12">
                            <RefreshCw className="size-5 animate-spin text-muted-foreground" />
                          </div>
                        ) : messagesError ? (
                          <div
                            role="alert"
                            className="p-6 text-sm text-destructive"
                          >
                            <p>会话读取失败，不能据此判断会话为空。</p>
                            <p className="mt-2 break-all">
                              {extractErrorMessage(messagesError)}
                            </p>
                            <Button
                              variant="outline"
                              className="mt-3"
                              onClick={() => void refetchMessages()}
                            >
                              重试读取
                            </Button>
                          </div>
                        ) : visibleMessages.length === 0 ? (
                          <div className="flex flex-col items-center justify-center py-12 text-center">
                            <MessageSquare className="size-8 text-muted-foreground/50 mb-2" />
                            <p className="text-sm text-muted-foreground">
                              {t("sessionManager.emptySession")}
                            </p>
                          </div>
                        ) : (
                          <div className="asm-messages">
                            <div
                              style={{
                                height: virtualizer.getTotalSize(),
                                position: "relative",
                              }}
                            >
                              {virtualizer
                                .getVirtualItems()
                                .map((virtualRow) => (
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
                                      message={
                                        visibleMessages[virtualRow.index]
                                      }
                                      isActive={
                                        activeMessageIndex === virtualRow.index
                                      }
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
            </div>
          </div>
        </section>
        <ConfirmDialog
          isOpen={Boolean(deleteTargets)}
          pending={isDeleting}
          title={
            deleteTargets && deleteTargets.length > 1
              ? t("sessionManager.batchDeleteConfirmTitle", {
                  defaultValue: "批量删除会话",
                })
              : t("sessionManager.deleteConfirmTitle", {
                  defaultValue: "删除会话",
                })
          }
          message={
            deleteTargets && deleteTargets.length > 1
              ? t("sessionManager.batchDeleteConfirmMessage", {
                  defaultValue:
                    "将永久删除已选中的 {{count}} 个本地会话记录。\n\n此操作不可恢复。",
                  count: deleteTargets.length,
                })
              : deleteTargets?.[0]
                ? t("sessionManager.deleteConfirmMessage", {
                    defaultValue:
                      "将永久删除本地会话“{{title}}”\nSession ID: {{sessionId}}\n\n此操作不可恢复。",
                    title: formatSessionTitle(deleteTargets[0]),
                    sessionId: deleteTargets[0].sessionId,
                  })
                : ""
          }
          confirmText={
            deleteTargets && deleteTargets.length > 1
              ? t("sessionManager.batchDeleteConfirmAction", {
                  defaultValue: "删除所选会话",
                })
              : t("sessionManager.deleteConfirmAction", {
                  defaultValue: "删除会话",
                })
          }
          cancelText={t("common.cancel", { defaultValue: "取消" })}
          variant="destructive"
          onConfirm={(includeProject) =>
            void handleDeleteConfirm(includeProject)
          }
          onCancel={() => {
            if (!isDeleting) {
              setDeleteTargets(null);
            }
          }}
        />
      </div>
    </TooltipProvider>
  );
}
