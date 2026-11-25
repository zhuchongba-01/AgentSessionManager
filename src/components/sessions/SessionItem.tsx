import { memo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { ContentHighlight, SessionContentHit, SessionMeta } from "@/types";
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

function renderUtf16Snippet(
  text: string,
  highlights: ContentHighlight[],
): ReactNode {
  if (!highlights.length) return text;
  const parts: ReactNode[] = [];
  let cursor = 0;
  highlights.forEach((range, idx) => {
    const start = Math.max(cursor, Math.min(text.length, range.start));
    const end = Math.max(start, Math.min(text.length, range.end));
    if (start > cursor) {
      parts.push(text.slice(cursor, start));
    }
    if (end > start) {
      parts.push(
        <mark key={idx} className="asm-search-mark">
          {text.slice(start, end)}
        </mark>,
      );
    }
    cursor = end;
  });
  if (cursor < text.length) {
    parts.push(text.slice(cursor));
  }
  return parts;
}

interface SessionItemProps {
  session: SessionMeta;
  isSelected: boolean;
  selectionMode: boolean;
  isChecked: boolean;
  isCheckDisabled?: boolean;
  /** 父会话显示名，由列表层解析后传入（见 `resolveParentLabel`） */
  parentLabel?: string;
  highlightTerms?: string[];
  contentHit?: SessionContentHit;
  onSelect: (key: string) => void;
  onSelectSnippet?: (key: string, messageIndex: number) => void;
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
  contentHit,
  onSelect,
  onSelectSnippet,
  onToggleChecked,
}: SessionItemProps) {
  const { t } = useTranslation();
  const title = formatSessionTitle(session);
  const lastActive = session.lastActiveAt || session.createdAt || undefined;
  const sessionKey = getSessionKey(session);
  const providerLabel = getProviderLabel(session.providerId, t);
  const hasSnippets = Boolean(contentHit && contentHit.snippets.length > 0);
  const hasStatusBadges = Boolean(
    session.cleanupPending ||
      session.archived ||
      session.parentSessionId ||
      (session.residual && !session.archived),
  );

  return (
    <div
      className={cn(
        "asm-session-item group",
        isSelected && "is-selected",
        hasStatusBadges && "has-badges",
        hasSnippets && "has-snippets",
      )}
    >
      <div className="flex min-w-0 flex-1 items-start gap-2">
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
            className={cn("shrink-0", hasStatusBadges ? "mt-1" : "self-center")}
          />
        )}
        <button
          type="button"
          aria-current={isSelected ? "true" : undefined}
          onClick={() => onSelect(sessionKey)}
          className="flex min-w-0 flex-1 cursor-pointer flex-col gap-1 text-left"
        >
          <span className="flex w-full min-w-0 items-center gap-2">
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
            <span className="asm-session-time">
              {lastActive
                ? formatRelativeTime(lastActive, t)
                : t("common.unknown")}
            </span>
          </span>
          {hasStatusBadges && (
            <span className="asm-session-badges-row">
              <SessionStatusBadges
                session={session}
                parentLabel={parentLabel}
              />
            </span>
          )}
        </button>
      </div>
      {hasSnippets && contentHit && (
        <div className="asm-session-snippets">
          {contentHit.snippets.slice(0, 2).map((snippet) => (
            <button
              key={snippet.messageIndex}
              type="button"
              className="asm-session-snippet"
              onClick={() => {
                if (onSelectSnippet) {
                  onSelectSnippet(sessionKey, snippet.messageIndex);
                } else {
                  onSelect(sessionKey);
                }
              }}
            >
              <span className="asm-session-snippet-text">
                {renderUtf16Snippet(snippet.text, snippet.highlights)}
              </span>
            </button>
          ))}
          {contentHit.matchCount > 2 && (
            <span className="asm-session-snippet-more">
              {t("sessionManager.moreContentMatches", {
                defaultValue: "共 {{count}} 条消息命中",
                count: contentHit.matchCount,
              })}
            </span>
          )}
        </div>
      )}
    </div>
  );
});
