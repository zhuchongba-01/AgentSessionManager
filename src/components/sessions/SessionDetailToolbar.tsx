import { memo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  CheckSquare,
  ChevronsDownUp,
  ChevronsUpDown,
  Copy,
  Download,
  PanelRight,
  Search,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useCopiedFeedback } from "@/hooks/useCopiedFeedback";
import { isMac } from "@/lib/platform";
import { cn } from "@/lib/utils";
import type { SessionMeta } from "@/types";
import { AgentIcon } from "./AgentIcon";
import type { SessionReaderViewMode } from "./types";
import { getBaseName, getProviderIconName, getProviderLabel } from "./utils";

export interface SessionDetailToolbarProps {
  selectedSession: SessionMeta | null;
  selectionMode: boolean;
  deletableFilteredCount: number;
  hasMessages: boolean;
  hasAnyChanges: boolean;
  collapsibleTurnCount: number;
  areAllTimelinesExpanded: boolean;
  readerViewMode: SessionReaderViewMode;
  detailSearchOpen: boolean;
  hasOutlineTurns: boolean;
  outlineOpen: boolean;
  batchActionControls: ReactNode;
  onEnterSelectionMode?: () => void;
  onSelectReaderViewMode: (mode: SessionReaderViewMode) => void;
  onToggleExpandAllTimelines: () => void;
  onCopyMarkdown: () => void;
  onExportMarkdown: () => void;
  onToggleDetailSearch: () => void;
  onToggleOutline: () => void;
}

export const SessionDetailToolbar = memo(function SessionDetailToolbar({
  selectedSession,
  selectionMode,
  deletableFilteredCount,
  hasMessages,
  hasAnyChanges,
  collapsibleTurnCount,
  areAllTimelinesExpanded,
  readerViewMode,
  detailSearchOpen,
  hasOutlineTurns,
  outlineOpen,
  batchActionControls,
  onEnterSelectionMode,
  onSelectReaderViewMode,
  onToggleExpandAllTimelines,
  onCopyMarkdown,
  onExportMarkdown,
  onToggleDetailSearch,
  onToggleOutline,
}: SessionDetailToolbarProps) {
  const { t } = useTranslation();
  const { copied: mdCopied, triggerCopied: triggerMdCopied } =
    useCopiedFeedback();

  const handleCopyClick = () => {
    triggerMdCopied();
    onCopyMarkdown();
  };

  return (
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
      {!selectedSession && selectionMode && (
        <div className="asm-detail-actions">{batchActionControls}</div>
      )}
      {!selectedSession &&
        !selectionMode &&
        onEnterSelectionMode &&
        deletableFilteredCount > 0 && (
          <Button
            type="button"
            size="sm"
            variant="ghost"
            className="asm-batch-enter-btn h-7 gap-1.5 px-2.5 text-xs"
            onClick={onEnterSelectionMode}
          >
            <CheckSquare className="size-3.5" aria-hidden="true" />
            <span>
              {t("sessionManager.multiSelectDelete", {
                defaultValue: "多选删除",
              })}
            </span>
          </Button>
        )}
      {selectedSession && hasMessages && (
        <div
          className="asm-reader-view-switch"
          role="group"
          aria-label="阅读视图筛选"
        >
          <button
            type="button"
            className="asm-list-view-option"
            aria-pressed={readerViewMode === "all"}
            onClick={() => onSelectReaderViewMode("all")}
          >
            {t("sessionManager.readerViewAll", { defaultValue: "全部" })}
          </button>
          <button
            type="button"
            className="asm-list-view-option"
            aria-pressed={readerViewMode === "dialogue"}
            onClick={() => onSelectReaderViewMode("dialogue")}
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
              onClick={() => onSelectReaderViewMode("changes")}
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
            aria-pressed={areAllTimelinesExpanded}
            onClick={onToggleExpandAllTimelines}
          >
            {areAllTimelinesExpanded ? (
              <ChevronsDownUp className="size-3.5" aria-hidden="true" />
            ) : (
              <ChevronsUpDown className="size-3.5" aria-hidden="true" />
            )}
            <span>
              {areAllTimelinesExpanded
                ? t("sessionManager.collapseAllSteps", {
                    defaultValue: "收起过程",
                  })
                : t("sessionManager.expandAllSteps", {
                    defaultValue: "展开过程",
                  })}
            </span>
          </button>
        )}
      {selectedSession && hasMessages && (
        <>
          <span className="asm-toolbar-divider" aria-hidden="true" />
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className={cn(
                  "asm-detail-icon-tool",
                  mdCopied && "text-success-text",
                )}
                aria-label={t("sessionManager.copyMarkdown", {
                  defaultValue: "复制 Markdown",
                })}
                onClick={handleCopyClick}
              >
                {mdCopied ? (
                  <Check className="size-3.5" />
                ) : (
                  <Copy className="size-3.5" />
                )}
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
                className="asm-detail-tool asm-detail-export-btn"
                aria-label={t("sessionManager.exportMarkdown", {
                  defaultValue: "导出 .md",
                })}
                onClick={onExportMarkdown}
              >
                <Download className="size-3.5" aria-hidden="true" />
                <span>
                  {t("sessionManager.exportMarkdown", {
                    defaultValue: "导出 .md",
                  })}
                </span>
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.exportMarkdownSaveAs", {
                defaultValue: "选择位置导出为 Markdown (.md)",
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
              onClick={onToggleDetailSearch}
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
      {selectedSession && hasOutlineTurns && (
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className={cn("asm-detail-icon-tool", outlineOpen && "is-active")}
              aria-label={t("sessionManager.toggleOutline", {
                defaultValue: "切换对话目录",
              })}
              aria-pressed={outlineOpen}
              onClick={onToggleOutline}
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
  );
});
