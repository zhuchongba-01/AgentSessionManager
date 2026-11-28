import { memo, type KeyboardEvent, type RefObject } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronUp, Search, X } from "lucide-react";

export interface SessionDetailFindBarProps {
  inputRef: RefObject<HTMLInputElement>;
  query: string;
  activeQuery: string;
  totalMatchCount: number;
  safeMatchCursor: number;
  onQueryChange: (nextQuery: string) => void;
  onInputKeyDown: (event: KeyboardEvent<HTMLInputElement>) => void;
  onNavigateMatch: (delta: number) => void;
  onClose: () => void;
}

export const SessionDetailFindBar = memo(function SessionDetailFindBar({
  inputRef,
  query,
  activeQuery,
  totalMatchCount,
  safeMatchCursor,
  onQueryChange,
  onInputKeyDown,
  onNavigateMatch,
  onClose,
}: SessionDetailFindBarProps) {
  const { t } = useTranslation();

  return (
    <div className="asm-detail-search-bar">
      <Search
        className="size-3.5 shrink-0 text-muted-foreground"
        aria-hidden="true"
      />
      <input
        ref={inputRef}
        type="search"
        aria-label={t("sessionManager.searchInSession", {
          defaultValue: "在当前会话中搜索",
        })}
        placeholder={t("sessionManager.searchInSessionPlaceholder", {
          defaultValue: "在当前会话中搜索...",
        })}
        value={query}
        onChange={(event) => onQueryChange(event.target.value)}
        onKeyDown={onInputKeyDown}
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
        onClick={() => onNavigateMatch(-1)}
      >
        <ChevronUp className="size-3.5" aria-hidden="true" />
      </button>
      <button
        type="button"
        aria-label={t("sessionManager.nextMatch", {
          defaultValue: "下一处匹配",
        })}
        disabled={totalMatchCount === 0}
        onClick={() => onNavigateMatch(1)}
      >
        <ChevronDown className="size-3.5" aria-hidden="true" />
      </button>
      <button
        type="button"
        aria-label={t("sessionManager.closeSessionSearch", {
          defaultValue: "关闭会话内搜索",
        })}
        onClick={onClose}
      >
        <X className="size-3.5" aria-hidden="true" />
      </button>
    </div>
  );
});
