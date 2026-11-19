import { memo } from "react";
import { useTranslation } from "react-i18next";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { SessionMeta } from "@/types";
import { AgentIcon } from "./AgentIcon";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";
import { SessionStatusBadges } from "./SessionStatusBadges";
import {
  formatRelativeTime,
  formatSessionTitle,
  getProviderIconName,
  getProviderLabel,
  getSessionKey,
} from "./utils";

interface SessionItemProps {
  session: SessionMeta;
  isSelected: boolean;
  selectionMode: boolean;
  isChecked: boolean;
  isCheckDisabled?: boolean;
  /** 父会话显示名，由列表层解析后传入（见 `resolveParentLabel`） */
  parentLabel?: string;
  highlightTerms?: string[];
  onSelect: (key: string) => void;
  onToggleChecked: (session: SessionMeta, checked: boolean) => void;
}

export const SessionItem = memo(function SessionItem({
  session,
  isSelected,
  selectionMode,
  isChecked,
  isCheckDisabled = false,
  parentLabel,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onSelect,
  onToggleChecked,
}: SessionItemProps) {
  const { t } = useTranslation();
  const title = formatSessionTitle(session);
  const lastActive = session.lastActiveAt || session.createdAt || undefined;
  const sessionKey = getSessionKey(session);
  const providerLabel = getProviderLabel(session.providerId, t);

  return (
    <div className={cn("asm-session-item group", isSelected && "is-selected")}>
      {selectionMode && (
        <Checkbox
          checked={isChecked}
          disabled={isCheckDisabled}
          aria-label={t("sessionManager.selectForBatch", {
            defaultValue: "选择会话",
          })}
          onCheckedChange={(checked) =>
            onToggleChecked(session, Boolean(checked))
          }
          className="shrink-0"
        />
      )}
      <button
        type="button"
        aria-current={isSelected ? "true" : undefined}
        onClick={() => onSelect(sessionKey)}
        className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 text-left"
      >
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="asm-provider-icon">
              <AgentIcon
                icon={getProviderIconName(session.providerId)}
                name={providerLabel}
                size={16}
              />
            </span>
          </TooltipTrigger>
          <TooltipContent>
            {getProviderLabel(session.providerId, t)}
          </TooltipContent>
        </Tooltip>
        <span className="asm-session-title">
          {highlightText(title, highlightTerms)}
        </span>
        <SessionStatusBadges session={session} parentLabel={parentLabel} />
        <span className="asm-session-time">
          {lastActive ? formatRelativeTime(lastActive, t) : t("common.unknown")}
        </span>
      </button>
    </div>
  );
});
