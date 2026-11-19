import { useCallback, useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import {
  Boxes,
  CheckSquare,
  ChevronDown,
  ChevronRight,
  ChevronsDownUp,
  ChevronsUpDown,
  FolderOpen,
  MessageSquare,
  RefreshCw,
  Search,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { isMac } from "@/lib/platform";
import { cn } from "@/lib/utils";
import type { SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import { AgentIcon } from "./AgentIcon";
import { EMPTY_HIGHLIGHT_TERMS } from "./highlightText";
import { SessionItem } from "./SessionItem";
import { SessionRows } from "./SessionRows";
import {
  PROVIDER_FILTER_ORDER,
  type GroupSelectionState,
  type ProviderFilter,
  type SessionListViewMode,
  type SessionStatusFilter,
} from "./types";
import {
  getProviderIconName,
  getProviderLabel,
  getSessionKey,
  resolveParentLabel,
  type SessionDirectoryGroup,
  type SessionProviderGroup,
} from "./utils";

interface SessionListSidebarProps {
  sessions: SessionMeta[];
  filteredSessions: SessionMeta[];
  groupedSessions: SessionProviderGroup[];
  providerFilter: ProviderFilter;
  onProviderFilterChange: (value: ProviderFilter) => void;
  providerFilterLabel: string;
  providerSessionCounts: Record<string, number>;
  selectionMode: boolean;
  onEnterSelectionMode: () => void;
  onExitSelectionMode: () => void;
  listViewMode: SessionListViewMode;
  onListViewModeChange: (mode: SessionListViewMode) => void;
  isLoading: boolean;
  isFetching: boolean;
  sessionsError: unknown;
  scanWarnings: string[];
  onRescan: () => void;
  deletableFilteredSessions: SessionMeta[];
  selectedDeletableSessions: SessionMeta[];
  allFilteredSelected: boolean;
  onToggleSelectAll: () => void;
  onClearSelection: () => void;
  searchQuery: string;
  onSearchQueryChange: (value: string) => void;
  searchTerms: string[];
  isSearching: boolean;
  statusFilter: SessionStatusFilter;
  onStatusFilterChange: (value: SessionStatusFilter) => void;
  statusCounts: {
    all: number;
    residual: number;
    cleanupPending: number;
    archived: number;
    child: number;
  };
  allGroupsExpanded: boolean;
  onToggleAllGroups: () => void;
  expandedProviderGroups: Set<string>;
  expandedDirectoryGroups: Set<string>;
  onToggleProviderGroup: (providerId: string) => void;
  onToggleDirectoryGroup: (directoryKey: string) => void;
  selectedKey: string | null;
  selectedSessionKeys: Set<string>;
  sessionTitles: Map<string, string>;
  onSelectSession: (key: string) => void;
  onToggleSessionChecked: (session: SessionMeta, checked: boolean) => void;
  onToggleSessionGroupChecked: (
    groupSessions: SessionMeta[],
    checked: boolean,
  ) => void;
  updatedAtLabel: string;
  providerCount: number;
}

export function SessionListSidebar({
  sessions,
  filteredSessions,
  groupedSessions,
  providerFilter,
  onProviderFilterChange,
  providerFilterLabel,
  providerSessionCounts,
  selectionMode,
  onEnterSelectionMode,
  onExitSelectionMode,
  listViewMode,
  onListViewModeChange,
  isLoading,
  isFetching,
  sessionsError,
  scanWarnings,
  onRescan,
  deletableFilteredSessions,
  selectedDeletableSessions,
  allFilteredSelected,
  onToggleSelectAll,
  onClearSelection,
  searchQuery,
  onSearchQueryChange,
  searchTerms,
  isSearching,
  statusFilter,
  onStatusFilterChange,
  statusCounts,
  allGroupsExpanded,
  onToggleAllGroups,
  expandedProviderGroups,
  expandedDirectoryGroups,
  onToggleProviderGroup,
  onToggleDirectoryGroup,
  selectedKey,
  selectedSessionKeys,
  sessionTitles,
  onSelectSession,
  onToggleSessionChecked,
  onToggleSessionGroupChecked,
  updatedAtLabel,
  providerCount,
}: SessionListSidebarProps) {
  const { t } = useTranslation();
  const searchInputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() === "k" && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        searchInputRef.current?.focus();
        searchInputRef.current?.select();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

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

  const activeHighlightTerms =
    searchTerms.length > 0 ? searchTerms : EMPTY_HIGHLIGHT_TERMS;

  const renderSessionItem = useCallback(
    (session: SessionMeta) => {
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
          highlightTerms={activeHighlightTerms}
          onSelect={onSelectSession}
          onToggleChecked={onToggleSessionChecked}
        />
      );
    },
    [
      activeHighlightTerms,
      onSelectSession,
      onToggleSessionChecked,
      selectedKey,
      selectedSessionKeys,
      selectionMode,
      sessionTitles,
    ],
  );

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
          onToggleSessionGroupChecked(
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
          onToggleSessionGroupChecked(
            directoryGroup.sessions,
            !selectionState.isSelected,
          )
        }
      />
    );
  };

  const scanWarningText = t("sessionManager.scanIncomplete", {
    defaultValue: "扫描未完整完成：{{warnings}}",
    warnings: scanWarnings.join("；"),
  });

  const hasStatusOptions =
    statusCounts.residual +
      statusCounts.cleanupPending +
      statusCounts.archived +
      statusCounts.child >
      0 || statusFilter !== "all";
  const isFiltering = isSearching || statusFilter !== "all";

  return (
    <div className="asm-list" onContextMenu={(event) => event.preventDefault()}>
      <div className="asm-list-toolbar">
        <Select
          value={providerFilter}
          onValueChange={(value) =>
            onProviderFilterChange(value as ProviderFilter)
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
            {PROVIDER_FILTER_ORDER.map((providerId) => (
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
                    onExitSelectionMode();
                  } else {
                    onEnterSelectionMode();
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
            onClick={() => onListViewModeChange("flat")}
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
            onClick={() => onListViewModeChange("grouped")}
          >
            {t("sessionManager.viewModeGrouped", {
              defaultValue: "分类",
            })}
          </button>
        </div>
        {listViewMode === "grouped" && groupedSessions.length > 0 && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="asm-list-tool-button"
                aria-label={
                  allGroupsExpanded
                    ? t("sessionManager.collapseAllGroups", {
                        defaultValue: "折叠全部分组",
                      })
                    : t("sessionManager.expandAllGroups", {
                        defaultValue: "展开全部分组",
                      })
                }
                onClick={onToggleAllGroups}
              >
                {allGroupsExpanded ? (
                  <ChevronsDownUp className="size-3.5" />
                ) : (
                  <ChevronsUpDown className="size-3.5" />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>
              {allGroupsExpanded
                ? t("sessionManager.collapseAllGroups", {
                    defaultValue: "折叠全部分组",
                  })
                : t("sessionManager.expandAllGroups", {
                    defaultValue: "展开全部分组",
                  })}
            </TooltipContent>
          </Tooltip>
        )}
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
              onClick={onRescan}
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
              onClick={onToggleSelectAll}
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
              onClick={onClearSelection}
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
          ref={searchInputRef}
          type="search"
          aria-label={t("sessionManager.searchSessions", {
            defaultValue: "搜索会话",
          })}
          placeholder={t("sessionManager.searchPlaceholder", {
            defaultValue: "搜索标题、项目、摘要或 ID",
          })}
          value={searchQuery}
          onChange={(event) => onSearchQueryChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") onSearchQueryChange("");
          }}
        />
        {searchQuery ? (
          <button
            type="button"
            aria-label={t("sessionManager.clearSearch", {
              defaultValue: "清空搜索",
            })}
            onClick={() => onSearchQueryChange("")}
          >
            <X className="size-3.5" aria-hidden="true" />
          </button>
        ) : (
          <kbd className="asm-search-kbd" aria-hidden="true">
            {isMac() ? "⌘K" : "Ctrl K"}
          </kbd>
        )}
      </div>
      {hasStatusOptions && (
        <div
          className="asm-status-filters"
          role="group"
          aria-label={t("sessionManager.statusFilterGroup", {
            defaultValue: "状态筛选",
          })}
        >
          <button
            type="button"
            className={cn(
              "asm-status-pill",
              statusFilter === "all" && "is-active",
            )}
            aria-pressed={statusFilter === "all"}
            onClick={() => onStatusFilterChange("all")}
          >
            {t("sessionManager.statusFilterAll", {
              defaultValue: "全部 {{count}}",
              count: statusCounts.all,
            })}
          </button>
          {(statusCounts.residual > 0 || statusFilter === "residual") && (
            <button
              type="button"
              className={cn(
                "asm-status-pill",
                statusFilter === "residual" && "is-active",
              )}
              aria-pressed={statusFilter === "residual"}
              onClick={() => onStatusFilterChange("residual")}
            >
              {t("sessionManager.statusFilterResidual", {
                defaultValue: "残留 {{count}}",
                count: statusCounts.residual,
              })}
            </button>
          )}
          {(statusCounts.cleanupPending > 0 ||
            statusFilter === "cleanupPending") && (
            <button
              type="button"
              className={cn(
                "asm-status-pill",
                statusFilter === "cleanupPending" && "is-active",
              )}
              aria-pressed={statusFilter === "cleanupPending"}
              onClick={() => onStatusFilterChange("cleanupPending")}
            >
              {t("sessionManager.statusFilterCleanupPending", {
                defaultValue: "待清理 {{count}}",
                count: statusCounts.cleanupPending,
              })}
            </button>
          )}
          {(statusCounts.child > 0 || statusFilter === "child") && (
            <button
              type="button"
              className={cn(
                "asm-status-pill",
                statusFilter === "child" && "is-active",
              )}
              aria-pressed={statusFilter === "child"}
              onClick={() => onStatusFilterChange("child")}
            >
              {t("sessionManager.statusFilterChild", {
                defaultValue: "子会话 {{count}}",
                count: statusCounts.child,
              })}
            </button>
          )}
          {(statusCounts.archived > 0 || statusFilter === "archived") && (
            <button
              type="button"
              className={cn(
                "asm-status-pill",
                statusFilter === "archived" && "is-active",
              )}
              aria-pressed={statusFilter === "archived"}
              onClick={() => onStatusFilterChange("archived")}
            >
              {t("sessionManager.statusFilterArchived", {
                defaultValue: "已归档 {{count}}",
                count: statusCounts.archived,
              })}
            </button>
          )}
        </div>
      )}
      {isSearching && (
        <div className="asm-search-summary" role="status">
          {t("sessionManager.searchResultsCount", {
            defaultValue: "找到 {{count}} 条会话",
            count: filteredSessions.length,
          })}
        </div>
      )}
      <div className="asm-list-scroll">
        {isLoading ? (
          <div className="asm-skeleton-list" aria-busy="true">
            {Array.from({ length: 6 }, (_, index) => (
              <div key={index} className="asm-skeleton-row">
                <div className="asm-skeleton-dot" />
                <div
                  className="asm-skeleton-line"
                  style={{ width: `${55 + ((index * 17) % 30)}%` }}
                />
                <div className="asm-skeleton-time" />
              </div>
            ))}
          </div>
        ) : sessionsError ? (
          <div role="alert" className="asm-error-panel m-3">
            <p>
              {t("sessionManager.sessionsLoadFailed", {
                defaultValue: "无法读取会话列表：{{error}}",
                error: extractErrorMessage(sessionsError),
              })}
            </p>
            <Button variant="outline" size="sm" onClick={onRescan}>
              {t("sessionManager.retryScan", { defaultValue: "重试扫描" })}
            </Button>
          </div>
        ) : scanWarnings.length > 0 && filteredSessions.length === 0 ? (
          <div role="alert" className="asm-scan-warning mx-3 my-2">
            {scanWarningText}
          </div>
        ) : filteredSessions.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-12 text-center">
            <MessageSquare className="size-8 text-muted-foreground/50 mb-2" />
            <p className="text-sm text-muted-foreground">
              {isSearching
                ? t("sessionManager.noMatchingSessions", {
                    defaultValue: "没有匹配的会话",
                  })
                : t("sessionManager.noSessions")}
            </p>
            {isSearching && (
              <div className="asm-empty-search">
                <p>
                  {t("sessionManager.noMatchingSessionsHint", {
                    defaultValue: "试试更短的关键词，或调整 Agent 筛选。",
                  })}
                </p>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => onSearchQueryChange("")}
                >
                  {t("sessionManager.clearSearchConditions", {
                    defaultValue: "清空搜索条件",
                  })}
                </Button>
              </div>
            )}
          </div>
        ) : listViewMode === "grouped" ? (
          <div>
            {scanWarnings.length > 0 && (
              <div role="alert" className="asm-scan-warning mx-2 my-2">
                {scanWarningText}
              </div>
            )}
            {groupedSessions.map((providerGroup) => {
              const providerOpen =
                isFiltering ||
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
                    onToggleProviderGroup(providerGroup.providerId)
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
                        aria-label={t("sessionManager.toggleProviderGroup", {
                          defaultValue: "展开或折叠 {{provider}} 供应商分组",
                          provider: providerLabel,
                        })}
                      >
                        {providerOpen ? (
                          <ChevronDown className="size-3.5 shrink-0 text-muted-foreground/70" />
                        ) : (
                          <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/70" />
                        )}
                        <AgentIcon
                          icon={getProviderIconName(providerGroup.providerId)}
                          name={providerLabel}
                          size={16}
                        />
                        <span className="asm-group-name">{providerLabel}</span>
                        {renderGroupSelectionBadge(
                          providerSelectionState,
                          providerGroup.sessions.length,
                        )}
                      </button>
                    </CollapsibleTrigger>
                  </div>
                  <CollapsibleContent className="asm-group-items">
                    {providerGroup.directories.map((directoryGroup) => {
                      const directoryOpen =
                        isFiltering ||
                        expandedDirectoryGroups.has(directoryGroup.key);
                      const directorySelectionState = getGroupSelectionState(
                        directoryGroup.sessions,
                      );

                      return (
                        <Collapsible
                          key={directoryGroup.key}
                          open={directoryOpen}
                          onOpenChange={() =>
                            onToggleDirectoryGroup(directoryGroup.key)
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
                                        t("sessionManager.unknownDirectory", {
                                          defaultValue: "未知目录",
                                        })}
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
                    })}
                  </CollapsibleContent>
                </Collapsible>
              );
            })}
          </div>
        ) : (
          <div>
            {scanWarnings.length > 0 && (
              <div role="alert" className="asm-scan-warning mx-2 my-2">
                {scanWarningText}
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
  );
}
