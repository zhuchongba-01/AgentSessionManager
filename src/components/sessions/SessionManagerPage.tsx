import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useVirtualizer } from "@tanstack/react-virtual";
import { toast } from "sonner";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  Search,
  Copy,
  AlertTriangle,
  RefreshCw,
  Trash2,
  MessageSquare,
  Clock,
  FolderOpen,
  FileText,
  CheckSquare,
  Boxes,
  ChevronDown,
  ChevronRight,
  Moon,
  Sun,
  Minus,
  Square,
  X,
  Github,
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
import type { SessionMessage, SessionMeta } from "@/types";
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
import type { ConfirmDialogCheckbox } from "@/components/ConfirmDialog";
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
  buildSessionTitleIndex,
  buildUserMessageToc,
  collectSessionDescendants,
  formatMessageTimestamp,
  formatSessionTitle,
  getBaseName,
  getVisibleSessionMessages,
  getProviderIconName,
  getProviderLabel,
  getSessionDirectoryGroupKey,
  getSessionKey,
  groupSessionsByProviderAndDirectory,
  resolveParentLabel,
  shouldShowSessionMinimap,
  type SessionDirectoryGroup,
  type SessionProviderGroup,
} from "./utils";

const SESSION_LIST_VIEW_MODE_STORAGE_KEY = "agent-session-manager.listViewMode";
const SESSION_GROUP_EXPANSION_STORAGE_KEY =
  "agent-session-manager.groupExpansionState";

/**
 * 确认框里逐条列出的目标数上限。
 *
 * 「全选筛选结果（含其他分页）」的选择范围可以远大于屏幕上渲染的行数（列表每页 100
 * 条），而删除不可恢复。列出前 N 条用于核对；超出的部分不假装已核对，而是要求用户在
 * 对话框里明确确认「已核对」才能继续——是知情确认，不是禁止。
 */
const CONFIRM_DETAIL_LIMIT = 50;

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
  | "deepseek"
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
  const [deleteTargets, setDeleteTargets] = useState<SessionMeta[] | null>(
    null,
  );
  // 删除确认框里的两个勾选项状态。勾选状态由页面持有（ConfirmDialog 受控），
  // 这样每次打开删除框时都能明确地重置，而不是靠副作用猜测。
  const [deleteWithChildren, setDeleteWithChildren] = useState(true);
  const [deleteAcknowledged, setDeleteAcknowledged] = useState(false);
  const [selectedSessionKeys, setSelectedSessionKeys] = useState<Set<string>>(
    () => new Set(),
  );
  const [isBatchDeleting, setIsBatchDeleting] = useState(false);
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

  const [searchQuery, setSearchQuery] = useState("");
  const searchTerms = searchQuery
    .trim()
    .toLocaleLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  const isSearching = searchTerms.length > 0;
  const filteredSessions = useMemo(() => {
    const terms = searchQuery
      .trim()
      .toLocaleLowerCase()
      .split(/\s+/)
      .filter(Boolean);
    const list = sessions.filter((session) => {
      if (providerFilter !== "all" && session.providerId !== providerFilter)
        return false;
      const text = [
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
        .toLocaleLowerCase();
      return terms.every((term) => text.includes(term));
    });
    return [...list].sort(
      (a, b) =>
        (b.lastActiveAt ?? b.createdAt ?? 0) -
        (a.lastActiveAt ?? a.createdAt ?? 0),
    );
  }, [sessions, providerFilter, searchQuery]);

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

  /**
   * 当前打开会话的子会话数量。
   *
   * 详情面板的删除按钮据此把它的实际范围说清楚：有子会话时它不再是「仅删除此会话」，
   * 而是会连带清理整条子会话链（范围仍可在确认框里改动）。
   */
  const selectedSessionChildCount = useMemo(
    () =>
      selectedSession
        ? collectSessionDescendants(sessions, [selectedSession]).length
        : 0,
    [selectedSession, sessions],
  );

  /** 父会话显示名：详情面板的归属徽标与删除按钮共用同一张表。 */
  const sessionTitles = useMemo(
    () => buildSessionTitleIndex(sessions),
    [sessions],
  );

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
  const deleteSessionMutation = useDeleteSessionMutation();
  const isDeleting = deleteSessionMutation.isPending || isBatchDeleting;

  const virtualizer = useVirtualizer({
    count: visibleMessages.length,
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

  // 提取用户消息用于目录。下标空间是「可见消息」，与虚拟列表、缩略导航一致。
  const userMessagesToc = useMemo(
    () => buildUserMessageToc(visibleMessages, isCodexSession),
    [isCodexSession, visibleMessages],
  );
  const minimapMessageIndexes = useMemo(
    () => userMessagesToc.map((item) => item.index),
    [userMessagesToc],
  );

  const scrollToMessage = (index: number) => {
    virtualizer.scrollToIndex(index, { align: "center", behavior: "smooth" });
    setActiveMessageIndex(index);
    setTimeout(() => setActiveMessageIndex(null), 2000);
  };

  // 缩略导航的显示条件只看「有没有用户提问」：此前用过的像素高度、正文字数两个
  // 判据都让用户看到「有时候有、有时候没有」——判据越多，越容易出现该出现却没出现。
  const showSessionMinimap = shouldShowSessionMinimap(
    minimapMessageIndexes.length,
  );

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

  // 仅保留当前会话、当前读取批次的全文；进行中的请求也共用。
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
      // 摘要也要进缓存键：会话改写后同一下标、同一长度会换出另一条消息，
      // 只按 (index, chars) 复用会把上一条的全文交给这一条。
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
          // 失败不缓存；已切走的会话不再弹出迟到错误。
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

  /**
   * 待删会话的全部子会话（子 Agent 会话及其后继）。
   *
   * 从**完整扫描结果**里算，不用过滤后的列表：搜索/筛选把某条子会话藏起来不代表它
   * 不存在，漏掉它只会让父会话的预检失败（后端会拒绝，不会真的留下孤儿）。
   */
  const deleteDescendants = useMemo(
    () => collectSessionDescendants(sessions, deleteTargets ?? []),
    [sessions, deleteTargets],
  );

  /** 本次确认框实际会删除的范围：勾选「同时删除子会话」时含全部后代。 */
  const plannedDeleteTargets = useMemo(
    () =>
      deleteWithChildren
        ? [...(deleteTargets ?? []), ...deleteDescendants]
        : (deleteTargets ?? []),
    [deleteTargets, deleteDescendants, deleteWithChildren],
  );

  const handleDeleteConfirm = async () => {
    if (!deleteTargets || deleteTargets.length === 0 || isDeleting) {
      return;
    }

    const targets = plannedDeleteTargets.filter(
      (session) => session.sourcePath,
    );
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
        });
        setDeleteTargets(null);
        setSelectedSessionKeys((current) => {
          const next = new Set(current);
          next.delete(getSessionKey(target));
          return next;
        });
      } catch (error) {
        // mutation 的 onError 已提示原因；但对话框不能就此消失——否则用户
        // 既看不到失败的是哪一条，也没有重试入口。保留对话框与选择状态。
        toast.error(
          t("sessionManager.deleteFailedRetry", {
            defaultValue:
              "删除失败，会话仍在列表中。请先退出对应 Agent 后重试。",
            error: extractErrorMessage(error),
          }),
        );
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
            `${formatSessionTitle(result)}（${getProviderLabel(result.providerId, t)}）`,
            result.error || t("common.unknown"),
            ...(result.warnings ?? []),
          ].join("："),
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
        // 逐条列出失败对象，而不是只报数量 + 第一条：混合结果下用户必须能
        // 看出哪几条没删掉、为什么。
        toast.error(
          t("sessionManager.batchDeleteFailed", {
            defaultValue: "{{failed}} 个会话删除失败",
            failed: failedErrors.length,
          }),
          {
            description: failedErrors.slice(0, 5).join("\n"),
            duration: 12_000,
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
        parentLabel={resolveParentLabel(session, sessionTitles)}
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
    openDeleteDialog(selectedDeletableSessions);
  };

  /**
   * 打开删除确认框。
   *
   * 默认勾选「同时删除子会话」：子会话在列表里往往只显示为「残留」，用户既认不出
   * 归属、也没法先逐个删干净，逐条手删正是要修掉的麻烦。范围照旧在确认框里逐条列出，
   * 用户随时可以取消勾选只删父会话。
   */
  const openDeleteDialog = (targets: SessionMeta[]) => {
    if (targets.length === 0) return;
    setDeleteWithChildren(true);
    setDeleteAcknowledged(false);
    setDeleteTargets(targets);
  };

  /**
   * 当前是否处于「已勾选了要批量删除的会话」状态。
   *
   * 详情面板里的删除按钮据此切换语义：有勾选项时它就是批量删除，没有时才是
   * 「只删右侧这一条」。两处删除入口共用同一个判断，避免用户在批量管理里多选
   * 之后点到详情按钮、结果只删掉一条。
   */
  const hasBatchSelection =
    selectionMode && selectedDeletableSessions.length > 0;

  /**
   * 确认框里逐条列出待删会话。数量超过上限时前半段逐条列出、其余只报数量并
   * 明确标注「未列出」，避免用一份不完整的清单冒充已核对。
   *
   * 清单列的是**实际会删除的范围**（含勾选后一并删除的子会话），每条子会话都标出
   * 它属于哪条父会话——这正是列表里「残留」标签看不出、用户最需要核对的信息。
   */
  const deleteConfirmDetails = useMemo(() => {
    if (plannedDeleteTargets.length <= 1) return null;
    const unlisted = Math.max(
      0,
      plannedDeleteTargets.length - CONFIRM_DETAIL_LIMIT,
    );
    return {
      items: plannedDeleteTargets
        .slice(0, CONFIRM_DETAIL_LIMIT)
        .map((session) => {
          const parent = resolveParentLabel(session, sessionTitles);
          return {
            title: formatSessionTitle(session),
            meta: [
              getProviderLabel(session.providerId, t),
              parent
                ? t("sessionManager.batchDeleteChildMeta", {
                    defaultValue: "子会话 · 属于「{{parent}}」",
                    parent,
                  })
                : session.sessionId,
            ].join(" · "),
          };
        }),
      unlisted,
      overflowLabel:
        unlisted > 0
          ? t("sessionManager.batchDeleteDetailsOverflow", {
              defaultValue:
                "上面 {{listed}} 条已逐条列出；另有 {{count}} 条未列出，但同样在本次删除范围内。",
              listed: CONFIRM_DETAIL_LIMIT,
              count: unlisted,
            })
          : undefined,
    };
  }, [plannedDeleteTargets, sessionTitles, t]);

  /** 勾选后一并删除的子会话里，有源路径可直接删除的数量。 */
  const deletableDescendantCount = useMemo(
    () => deleteDescendants.filter((session) => session.sourcePath).length,
    [deleteDescendants],
  );

  /**
   * 删除确认框里的勾选项。
   *
   * - 「同时删除子会话」：本次要修的核心。默认勾选，取消勾选时后端预检会拒绝删除
   *   仍被依赖的父会话，不会留下孤儿。
   * - 「已核对未列出的条目」：只有当清单列不全时才出现，且必须勾选才能确认。
   */
  const deleteDialogCheckboxes = useMemo<ConfirmDialogCheckbox[]>(() => {
    const items: ConfirmDialogCheckbox[] = [];
    if (deleteDescendants.length > 0) {
      const blocked = deleteDescendants.length - deletableDescendantCount;
      items.push({
        id: "delete-with-children",
        checked: deleteWithChildren,
        onCheckedChange: setDeleteWithChildren,
        label:
          deleteTargets && deleteTargets.length > 1
            ? t("sessionManager.cascadeDeleteLabelMulti", {
                defaultValue:
                  "同时删除这 {{roots}} 条会话下的 {{count}} 个子会话",
                roots: deleteTargets.length,
                count: deleteDescendants.length,
              })
            : t("sessionManager.cascadeDeleteLabel", {
                defaultValue: "同时删除「{{title}}」的 {{count}} 个子会话",
                title: deleteTargets?.[0]
                  ? formatSessionTitle(deleteTargets[0])
                  : "",
                count: deleteDescendants.length,
              }),
        warning: [
          t("sessionManager.cascadeDeleteWarning", {
            defaultValue:
              "子会话不与父会话一起删除就没法单独清理：它们不会进入回收站，文件与索引记录都会被永久删除。",
          }),
          blocked > 0
            ? t("sessionManager.cascadeDeleteBlocked", {
                defaultValue:
                  "另有 {{count}} 个子会话没有可用的源路径，管理器无法删除；父会话的删除会因此被拒绝，请重新扫描后再试。",
                count: blocked,
              })
            : "",
        ]
          .filter(Boolean)
          .join("\n"),
      });
    }
    if (deleteConfirmDetails && deleteConfirmDetails.unlisted > 0) {
      items.push({
        id: "acknowledge-unlisted",
        checked: deleteAcknowledged,
        onCheckedChange: setDeleteAcknowledged,
        required: true,
        label: t("sessionManager.batchDeleteAcknowledge", {
          defaultValue:
            "我已核对将删除的 {{count}} 条会话（含未列出的 {{unlisted}} 条）",
          count: plannedDeleteTargets.length,
          unlisted: deleteConfirmDetails.unlisted,
        }),
        warning: t("sessionManager.batchDeleteAcknowledgeWarning", {
          defaultValue:
            "这些会话不会进入回收站：文件与数据库记录都会被永久删除，且无法撤销。",
        }),
      });
    }
    return items;
  }, [
    deleteAcknowledged,
    deleteConfirmDetails,
    deleteDescendants,
    deleteTargets,
    deleteWithChildren,
    deletableDescendantCount,
    plannedDeleteTargets.length,
    t,
  ]);

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
                      onClick={() =>
                        void handleOpenUrl(GITHUB_LATEST_RELEASE_URL)
                      }
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
                      <SelectItem value="all" className="asm-agent-filter-item">
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
                          "deepseek",
                          "grokbuild",
                          "claude",
                          "opencode",
                          "zcode",
                          "pi",
                        ] as const
                      ).map((providerId) => (
                        <SelectItem
                          key={providerId}
                          value={providerId}
                          className="asm-agent-filter-item"
                        >
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
                  <div
                    className="asm-list-view-switch"
                    role="group"
                    aria-label={t("sessionManager.viewModeTooltip", {
                      defaultValue: "查看方式",
                    })}
                  >
                    <button
                      type="button"
                      className="asm-list-view-option"
                      aria-pressed={listViewMode === "flat"}
                      aria-label={t("sessionManager.viewModeFlat", {
                        defaultValue: "列表",
                      })}
                      onClick={() => setListViewMode("flat")}
                    >
                      {t("sessionManager.viewModeFlat", {
                        defaultValue: "列表",
                      })}
                    </button>
                    <button
                      type="button"
                      className="asm-list-view-option"
                      aria-pressed={listViewMode === "grouped"}
                      aria-label={t("sessionManager.viewModeGrouped", {
                        defaultValue: "分类",
                      })}
                      onClick={() => setListViewMode("grouped")}
                    >
                      {t("sessionManager.viewModeGrouped", {
                        defaultValue: "分类",
                      })}
                    </button>
                  </div>
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
                    {/* 此处不放删除按钮：删除入口统一在右侧详情面板
                        （批量模式下它显示「删除已勾选的 N 条」）。 */}
                  </div>
                )}
                <div className="asm-search" role="search">
                  <Search className="size-4 shrink-0" aria-hidden="true" />
                  <input
                    type="search"
                    aria-label="搜索会话"
                    placeholder="搜索标题、项目、摘要或 ID"
                    value={searchQuery}
                    onChange={(event) => setSearchQuery(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Escape") setSearchQuery("");
                    }}
                  />
                  {searchQuery && (
                    <button
                      type="button"
                      aria-label="清空搜索"
                      onClick={() => setSearchQuery("")}
                    >
                      <X className="size-3.5" aria-hidden="true" />
                    </button>
                  )}
                </div>
                {isSearching && (
                  <div className="asm-search-summary" role="status">
                    找到 {filteredSessions.length} 条会话
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
                        {isSearching
                          ? "没有匹配的会话"
                          : t("sessionManager.noSessions")}
                      </p>
                      {isSearching && (
                        <div className="asm-empty-search">
                          <p>试试更短的关键词，或调整 Agent 筛选。</p>
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => setSearchQuery("")}
                          >
                            清空搜索条件
                          </Button>
                        </div>
                      )}
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
                        const providerOpen =
                          isSearching ||
                          expandedProviderGroups.has(providerGroup.providerId);
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
                                    isSearching ||
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
                        <SessionStatusBadges
                          session={selectedSession}
                          parentLabel={resolveParentLabel(
                            selectedSession,
                            sessionTitles,
                          )}
                        />

                        {/* 操作按钮组 */}
                        <div className="asm-detail-actions">
                          <div className="asm-detail-quick-actions">
                            <button
                              type="button"
                              onClick={() =>
                                void handleCopy(
                                  selectedSession.sessionId,
                                  "已复制会话 ID",
                                )
                              }
                            >
                              <Copy className="size-3.5" aria-hidden="true" />
                              复制 ID
                            </button>
                            {selectedSession.sourcePath && (
                              <button
                                type="button"
                                onClick={() =>
                                  void handleCopy(
                                    selectedSession.sourcePath!,
                                    t("sessionManager.sourcePathCopied"),
                                  )
                                }
                              >
                                <FileText
                                  className="size-3.5"
                                  aria-hidden="true"
                                />
                                复制源路径
                              </button>
                            )}
                          </div>
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
                                    ? openBatchDeleteDialog()
                                    : openDeleteDialog([selectedSession])
                                }
                                disabled={
                                  hasBatchSelection
                                    ? isDeleting ||
                                      selectedDeletableSessions.length === 0
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
                                      ? t(
                                          "sessionManager.deleteSelectedCount",
                                          {
                                            defaultValue:
                                              "删除已勾选的 {{count}} 条",
                                            count:
                                              selectedDeletableSessions.length,
                                          },
                                        )
                                      : selectedSession.cleanupPending
                                        ? "继续清理"
                                        : selectedSessionChildCount > 0
                                          ? t(
                                              "sessionManager.deleteThisWithChildren",
                                              {
                                                defaultValue:
                                                  "删除此会话及其 {{count}} 个子会话",
                                                count:
                                                  selectedSessionChildCount,
                                              },
                                            )
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
                                    count: selectedDeletableSessions.length,
                                  })
                                : selectedSessionChildCount > 0
                                  ? t(
                                      "sessionManager.deleteWithChildrenTooltip",
                                      {
                                        defaultValue:
                                          "永久删除右侧这一条会话及其 {{count}} 个子会话；范围可以在确认框里改",
                                        count: selectedSessionChildCount,
                                      },
                                    )
                                  : t("sessionManager.deleteTooltip", {
                                      defaultValue:
                                        "仅永久删除右侧这一条会话记录",
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
                      {showSessionMinimap && (
                        <SessionMinimap
                          messages={visibleMessages}
                          messageIndexes={minimapMessageIndexes}
                          visibleMessageIndexes={viewportMessageIndexes}
                          onTickClick={scrollToMessage}
                        />
                      )}
                      <div
                        ref={scrollContainerRef}
                        className="asm-detail-scroll"
                      >
                        {(isLoadingMessages || isFetchingMessages) &&
                        visibleMessages.length === 0 ? (
                          <div
                            role="status"
                            className="flex items-center justify-center gap-2 py-12 text-sm text-muted-foreground"
                          >
                            <RefreshCw className="size-5 animate-spin" />
                            {t("sessionManager.loadingMessages", {
                              defaultValue: "正在读取会话…",
                            })}
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
                                      messageRevision={messageRevision}
                                      onCopy={handleCopyMessage}
                                      onLoadFullContent={handleLoadFullContent}
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
              ? deleteWithChildren && deleteDescendants.length > 0
                ? t("sessionManager.batchDeleteWithChildrenMessage", {
                    defaultValue:
                      "将永久删除 {{count}} 个本地会话记录，其中 {{children}} 条是随父会话一并删除的子会话。\n\n此操作不可恢复。",
                    count: plannedDeleteTargets.length,
                    children: deleteDescendants.length,
                  })
                : t("sessionManager.batchDeleteConfirmMessage", {
                    defaultValue:
                      "将永久删除已选中的 {{count}} 个本地会话记录。\n\n此操作不可恢复。",
                    count: plannedDeleteTargets.length,
                  })
              : deleteTargets?.[0]
                ? plannedDeleteTargets.length > 1
                  ? t("sessionManager.deleteConfirmWithChildrenMessage", {
                      defaultValue:
                        "将永久删除本地会话“{{title}}”及其 {{count}} 个子会话。\n\n此操作不可恢复。",
                      title: formatSessionTitle(deleteTargets[0]),
                      count: plannedDeleteTargets.length - 1,
                    })
                  : t("sessionManager.deleteConfirmMessage", {
                      defaultValue:
                        "将永久删除本地会话“{{title}}”\nSession ID: {{sessionId}}\n\n此操作不可恢复。",
                      title: formatSessionTitle(deleteTargets[0]),
                      sessionId: deleteTargets[0].sessionId,
                    })
                : ""
          }
          details={deleteConfirmDetails?.items}
          detailsOverflowLabel={deleteConfirmDetails?.overflowLabel}
          // 勾选项由页面持有状态：范围（是否连带子会话）和「已核对未列出的条目」都在
          // 这里说清楚，勾选与否则回传给页面，不靠组件内部副作用同步。
          checkboxes={deleteDialogCheckboxes}
          confirmText={
            deleteTargets && deleteTargets.length > 1
              ? t("sessionManager.batchDeleteConfirmAction", {
                  defaultValue: "删除所选会话",
                })
              : plannedDeleteTargets.length > 1
                ? t("sessionManager.deleteWithChildrenAction", {
                    defaultValue: "删除会话及其子会话",
                  })
                : t("sessionManager.deleteConfirmAction", {
                    defaultValue: "删除会话",
                  })
          }
          cancelText={t("common.cancel", { defaultValue: "取消" })}
          variant="destructive"
          onConfirm={() => void handleDeleteConfirm()}
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
