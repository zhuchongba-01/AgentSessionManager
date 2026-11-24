import { memo, useEffect, useMemo, useRef } from "react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import type { SessionMessage } from "@/types";
import { AgentIcon } from "./AgentIcon";
import type { SessionTurnGroup } from "./turnGrouping";
import {
  extractCodexPromptPreview,
  formatMessageTimestamp,
  getProviderIconName,
} from "./utils";

const SNIPPET_MAX_CHARS = 140;

function cleanMarkdownSnippet(raw: string): string {
  return raw
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[#>*`~|]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, SNIPPET_MAX_CHARS);
}

interface SessionOutlineProps {
  turns: SessionTurnGroup[];
  messages: SessionMessage[];
  providerId: string;
  providerLabel: string;
  isCodexSession: boolean;
  activeTurnIndex: number;
  onJumpToMessage: (messageIndex: number) => void;
  onJumpToTurnReply: (turnIndex: number) => void;
}

export const SessionOutline = memo(function SessionOutline({
  turns,
  messages,
  providerId,
  providerLabel,
  isCodexSession,
  activeTurnIndex,
  onJumpToMessage,
  onJumpToTurnReply,
}: SessionOutlineProps) {
  const { t } = useTranslation();
  const listRef = useRef<HTMLOListElement | null>(null);

  const entries = useMemo(
    () =>
      turns.map((turn) => {
        const userMsg =
          turn.userIndex !== null ? messages[turn.userIndex] : undefined;
        const rawQuestion = userMsg
          ? isCodexSession
            ? extractCodexPromptPreview(userMsg.content)
            : userMsg.content
          : undefined;
        const replyMsg =
          turn.finalAssistantIndex !== null
            ? messages[turn.finalAssistantIndex]
            : undefined;
        return {
          turn,
          questionText:
            rawQuestion !== undefined
              ? cleanMarkdownSnippet(rawQuestion)
              : undefined,
          questionTs: userMsg?.ts,
          replyText: replyMsg ? cleanMarkdownSnippet(replyMsg.content) : "",
          hasReplyTarget:
            turn.finalAssistantIndex !== null || turn.stepIndexes.length > 0,
        };
      }),
    [isCodexSession, messages, turns],
  );

  useEffect(() => {
    const list = listRef.current;
    const item = list?.querySelector<HTMLElement>(
      `[data-outline-turn="${activeTurnIndex}"]`,
    );
    if (!list || !item) return;
    const top = item.offsetTop - list.offsetTop;
    const bottom = top + item.offsetHeight;
    if (top < list.scrollTop) {
      list.scrollTop = top;
    } else if (bottom > list.scrollTop + list.clientHeight) {
      list.scrollTop = bottom - list.clientHeight;
    }
  }, [activeTurnIndex]);

  return (
    <nav
      aria-label={t("sessionManager.tocTitle", { defaultValue: "对话目录" })}
      className="asm-ccs-outline"
    >
      <div className="asm-ccs-outline-head">
        <span>
          {t("sessionManager.tocTitle", { defaultValue: "对话目录" })}
        </span>
        <span className="asm-ccs-outline-count">{entries.length}</span>
      </div>
      <ol ref={listRef} className="asm-ccs-outline-list">
        {entries.map((entry) => {
          const isActive = entry.turn.turnIndex === activeTurnIndex;
          return (
            <li
              key={entry.turn.turnKey}
              data-outline-turn={entry.turn.turnIndex}
              className={cn("asm-ccs-outline-item", isActive && "is-active")}
            >
              {isActive && (
                <span className="asm-ccs-outline-bar" aria-hidden="true" />
              )}
              {entry.questionText !== undefined &&
                entry.turn.userIndex !== null && (
                  <button
                    type="button"
                    aria-label={
                      entry.turn.userTurnOrdinal !== null
                        ? t("sessionManager.jumpToUserTurn", {
                            defaultValue: "跳转到第 {{index}} 个用户问题",
                            index: entry.turn.userTurnOrdinal,
                          })
                        : undefined
                    }
                    aria-current={isActive ? "true" : undefined}
                    onClick={() => onJumpToMessage(entry.turn.userIndex!)}
                    className="asm-ccs-outline-q"
                  >
                    <span className="asm-ccs-outline-q-meta">
                      {t("sessionManager.roleUser", { defaultValue: "你" })}
                      {entry.questionTs
                        ? ` · ${formatMessageTimestamp(entry.questionTs)}`
                        : ""}
                    </span>
                    <span className="asm-ccs-outline-q-bubble">
                      {entry.questionText || "（提问）"}
                    </span>
                  </button>
                )}
              {entry.hasReplyTarget && (
                <button
                  type="button"
                  onClick={() => onJumpToTurnReply(entry.turn.turnIndex)}
                  className="asm-ccs-outline-a"
                >
                  <span className="asm-ccs-outline-avatar" aria-hidden="true">
                    <AgentIcon
                      icon={getProviderIconName(providerId)}
                      name={providerLabel}
                      size={12}
                    />
                  </span>
                  <span className="min-w-0 flex-1">
                    {entry.turn.stepIndexes.length > 0 && (
                      <span className="asm-ccs-outline-steps">
                        {entry.turn.stepIndexes.length} 步
                        {entry.turn.errorCount > 0 && (
                          <span className="asm-ccs-outline-err-dot" />
                        )}
                      </span>
                    )}
                    <span
                      className={cn(
                        "asm-ccs-outline-a-text",
                        !entry.replyText && "is-empty",
                      )}
                    >
                      {entry.replyText ||
                        t("sessionManager.noReplyYet", {
                          defaultValue: "（暂无最终回复）",
                        })}
                    </span>
                  </span>
                </button>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
});
