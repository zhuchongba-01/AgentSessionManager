import { memo, useCallback, useState } from "react";
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
  /**
   * 取回被截断消息的全文。后端只下发预览的消息必须靠它才能展开/复制完整内容；
   * 未提供时退化为只显示预览。
   */
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}

export const SessionMessageItem = memo(function SessionMessageItem({
  message,
  isActive,
  onCopy,
  onLoadFullContent,
}: SessionMessageItemProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [fullContent, setFullContent] = useState<string | null>(null);
  const [isLoadingFull, setIsLoadingFull] = useState(false);

  const isTruncated = message.truncated === true && message.index !== undefined;
  // 未截断的长正文仍就地折叠；被截断的正文里只有预览，展开要向后端取
  const isLong = isTruncated || message.content.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const totalChars = message.contentChars ?? message.content.length;

  const loadFullContent = useCallback(async (): Promise<string> => {
    if (fullContent !== null) return fullContent;
    if (!isTruncated || !onLoadFullContent) return message.content;

    setIsLoadingFull(true);
    try {
      const content = await onLoadFullContent(message);
      setFullContent(content);
      return content;
    } finally {
      setIsLoadingFull(false);
    }
  }, [fullContent, isTruncated, message, onLoadFullContent]);

  const handleToggle = useCallback(() => {
    if (expanded) {
      setExpanded(false);
      return;
    }
    if (!isTruncated) {
      setExpanded(true);
      return;
    }
    // 失败提示由调用方给出；这里保持折叠状态，不做假的展开
    void loadFullContent()
      .then(() => setExpanded(true))
      .catch(() => undefined);
  }, [expanded, isTruncated, loadFullContent]);

  const handleCopy = useCallback(() => {
    if (!isTruncated) {
      onCopy(message.content);
      return;
    }
    void loadFullContent()
      .then(onCopy)
      .catch(() => undefined);
  }, [isTruncated, loadFullContent, message.content, onCopy]);

  const loaded = fullContent ?? message.content;
  const displayContent = collapsed
    ? loaded.slice(0, COLLAPSED_LENGTH) + "…"
    : loaded;

  return (
    <div
      className={cn(
        "asm-message group min-w-0",
        message.role.toLowerCase() === "user"
          ? "is-user"
          : message.role.toLowerCase() === "assistant"
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
          onClick={handleCopy}
        >
          <Copy className="size-3.5" aria-hidden="true" />
        </button>
      </div>
      <div className="asm-message-body">{displayContent}</div>
      {isLong && (
        <button
          type="button"
          aria-expanded={expanded}
          aria-busy={isLoadingFull || undefined}
          onClick={handleToggle}
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
              {isLoadingFull
                ? t("sessionManager.loadingFullContent", {
                    defaultValue: "正在读取完整内容…",
                  })
                : t("sessionManager.expandContent", {
                    defaultValue: "展开完整内容",
                  })}
              <span className="text-muted-foreground/60">
                ({Math.round(totalChars / 1000)}k)
              </span>
            </>
          )}
        </button>
      )}
    </div>
  );
});
