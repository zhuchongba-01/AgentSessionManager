import { memo, useState } from "react";
import { ChevronDown, ChevronUp, Copy } from "lucide-react";
import { useTranslation } from "react-i18next";

import { cn } from "@/lib/utils";
import type { SessionMessage } from "@/types";
import { formatMessageTimestamp, getRoleLabel } from "./utils";

const COLLAPSE_THRESHOLD = 3000;
const COLLAPSED_LENGTH = 1500;

interface SessionMessageItemProps {
  message: SessionMessage;
  isActive: boolean;
  onCopy: (content: string) => void;
}

export const SessionMessageItem = memo(function SessionMessageItem({
  message,
  isActive,
  onCopy,
}: SessionMessageItemProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);

  const normalizedRole = message.role.toLowerCase();
  const isLong = message.content.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const displayContent = collapsed
    ? message.content.slice(0, COLLAPSED_LENGTH) + "…"
    : message.content;

  return (
    <div
      className={cn(
        "asm-message group min-w-0",
        normalizedRole === "user"
          ? "is-user"
          : normalizedRole === "assistant"
            ? "is-assistant"
            : "is-tool",
        isActive && "is-active",
      )}
    >
      <div className="asm-message-role">
        <span>{getRoleLabel(message.role, t)}</span>
        {message.ts && (
          <span className="asm-message-ts">
            {formatMessageTimestamp(message.ts)}
          </span>
        )}
        <button
          type="button"
          className="asm-message-copy"
          aria-label={t("sessionManager.copyMessage", {
            defaultValue: "复制消息",
          })}
          onClick={() => onCopy(message.content)}
        >
          <Copy className="size-3.5" aria-hidden="true" />
        </button>
      </div>
      <div className="asm-message-body">{displayContent}</div>
      {isLong && (
        <button
          type="button"
          aria-expanded={expanded}
          onClick={() => setExpanded((v) => !v)}
          className="mt-1 flex cursor-pointer items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
        >
          {expanded ? (
            <>
              <ChevronUp className="size-3" />
              {t("sessionManager.collapseContent", {
                defaultValue: "收起",
              })}
            </>
          ) : (
            <>
              <ChevronDown className="size-3" />
              {t("sessionManager.expandContent", {
                defaultValue: "展开完整内容",
              })}
              <span className="text-muted-foreground/60">
                ({Math.round(message.content.length / 1000)}k)
              </span>
            </>
          )}
        </button>
      )}
    </div>
  );
});
