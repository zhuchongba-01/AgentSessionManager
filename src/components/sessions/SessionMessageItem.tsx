import {
  memo,
  useCallback,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  ChevronDown,
  ChevronUp,
  Code2,
  Copy,
  LoaderCircle,
} from "lucide-react";
import { useTranslation } from "react-i18next";

import { cn } from "@/lib/utils";
import type { SessionMessage } from "@/types";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";
import { hasRichMarkdown, MessageMarkdown } from "./MessageMarkdown";
import {
  formatMessageTimestamp,
  getRoleLabel,
  splitCodexIdeContext,
} from "./utils";

const COLLAPSE_THRESHOLD = 3000;
const COLLAPSED_LENGTH = 1500;
const TOOL_FOLD_LINE_LIMIT = 8;
const TOOL_PREVIEW_LINES = 4;

interface SessionMessageItemProps {
  message: SessionMessage;
  messageRevision?: string | number;
  isActive: boolean;
  isCodexSession?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => void;
  /**
   * 取回被截断消息的全文。后端只下发预览的消息必须靠它才能展开/复制完整内容；
   * 未提供时退化为只显示预览。
   */
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}

export const SessionMessageItem = memo(function SessionMessageItem(
  props: SessionMessageItemProps,
) {
  const { message, messageRevision, isCodexSession } = props;
  // 等价对象刷新保留展开；读取批次变化或任一消息字段变化立即隔离旧状态。
  const identity = JSON.stringify([
    messageRevision,
    isCodexSession ?? false,
    message.role,
    message.content,
    message.ts,
    message.index,
    message.truncated,
    message.contentChars,
    message.contentIdentity,
  ]);
  return <SessionMessageContent key={identity} {...props} />;
});

function SessionMessageContent({
  message,
  isActive,
  isCodexSession = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onLoadFullContent,
}: SessionMessageItemProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [showIdeContext, setShowIdeContext] = useState(false);
  const [showRaw, setShowRaw] = useState(false);
  const [toolExpanded, setToolExpanded] = useState(false);
  const [fullContent, setFullContent] = useState<string | null>(null);
  const [isLoadingFull, setIsLoadingFull] = useState(false);
  const [isCopying, setIsCopying] = useState(false);
  const mounted = useRef(false);
  const cachedContent = useRef<string | null>(null);
  const pendingContent = useRef<Promise<string> | null>(null);
  const copying = useRef(false);

  useLayoutEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const loaded = fullContent ?? message.content;
  const codexSplit =
    isCodexSession && message.role.toLowerCase() === "user"
      ? splitCodexIdeContext(loaded)
      : null;
  const primaryText = codexSplit ? codexSplit.prompt : loaded;

  const isTruncated = message.truncated === true && message.index !== undefined;
  // 未截断的长正文仍就地折叠；被截断的正文里只有预览，展开要向后端取
  const isLong = isTruncated || primaryText.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const totalChars =
    isTruncated || !codexSplit
      ? (message.contentChars ?? primaryText.length)
      : primaryText.length;

  const loadFullContent = useCallback((): Promise<string> => {
    if (cachedContent.current !== null) {
      return Promise.resolve(cachedContent.current);
    }
    if (pendingContent.current) return pendingContent.current;
    if (!isTruncated || !onLoadFullContent) {
      return Promise.resolve(message.content);
    }

    setIsLoadingFull(true);
    // 延后调用也使同步抛错走统一失败清理，展开和复制始终共享同一请求。
    const request = Promise.resolve()
      .then(() => onLoadFullContent(message))
      .then((content) => {
        if (mounted.current) {
          cachedContent.current = content;
          setFullContent(content);
        }
        return content;
      })
      .finally(() => {
        pendingContent.current = null;
        if (mounted.current) setIsLoadingFull(false);
      });
    pendingContent.current = request;
    return request;
  }, [isTruncated, message, onLoadFullContent]);

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
      .then(() => {
        if (mounted.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [expanded, isTruncated, loadFullContent]);

  const handleCopy = useCallback(() => {
    if (copying.current) return;
    if (!isTruncated) {
      onCopy(loaded);
      return;
    }
    copying.current = true;
    setIsCopying(true);
    void loadFullContent()
      .then((content) => {
        if (mounted.current) onCopy(content);
      })
      .catch(() => undefined)
      .finally(() => {
        copying.current = false;
        if (mounted.current) setIsCopying(false);
      });
  }, [isTruncated, loadFullContent, loaded, onCopy]);

  const displayContent = collapsed
    ? primaryText.slice(0, COLLAPSED_LENGTH) + "…"
    : primaryText;

  const roleLower = message.role.toLowerCase();
  const isTool = roleLower !== "user" && roleLower !== "assistant";
  const toolLines = useMemo(
    () => (isTool && !isTruncated ? primaryText.split(/\r?\n/) : []),
    [isTool, isTruncated, primaryText],
  );
  const isToolLineFoldable =
    toolLines.length > TOOL_FOLD_LINE_LIMIT &&
    primaryText.length <= COLLAPSE_THRESHOLD;
  const hiddenToolText = useMemo(
    () =>
      isToolLineFoldable
        ? toolLines.slice(TOOL_PREVIEW_LINES).join("\n").toLocaleLowerCase()
        : "",
    [isToolLineFoldable, toolLines],
  );
  const hasHiddenSearchHit =
    isToolLineFoldable &&
    highlightTerms.some(
      (term) =>
        term.trim() !== "" &&
        hiddenToolText.includes(term.trim().toLocaleLowerCase()),
    );
  const isToolPreviewFolded =
    isToolLineFoldable && !toolExpanded && !hasHiddenSearchHit;
  const renderedContent = isToolPreviewFolded
    ? toolLines.slice(0, TOOL_PREVIEW_LINES).join("\n")
    : displayContent;
  const canRenderMarkdown = !isTool && hasRichMarkdown(renderedContent);
  const handleCodeCopy = onCopyCode ?? onCopy;

  return (
    <div
      className={cn(
        "asm-message group min-w-0",
        roleLower === "user"
          ? "is-user"
          : roleLower === "assistant"
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
        <span className="asm-message-role-actions">
          {canRenderMarkdown && (
            <button
              type="button"
              className="asm-message-raw-toggle"
              aria-pressed={showRaw}
              aria-label={
                showRaw
                  ? t("sessionManager.viewMarkdown", {
                      defaultValue: "Markdown 视图",
                    })
                  : t("sessionManager.viewRaw", { defaultValue: "查看原文" })
              }
              title={
                showRaw
                  ? t("sessionManager.viewMarkdown", {
                      defaultValue: "Markdown 视图",
                    })
                  : t("sessionManager.viewRaw", { defaultValue: "查看原文" })
              }
              onClick={() => setShowRaw((prev) => !prev)}
            >
              <Code2 className="size-3" aria-hidden="true" />
              <span>
                {showRaw
                  ? t("sessionManager.markdownBadge", { defaultValue: "渲染" })
                  : t("sessionManager.rawBadge", { defaultValue: "原文" })}
              </span>
            </button>
          )}
          <button
            type="button"
            className="asm-message-copy"
            aria-label={
              isCopying
                ? t("sessionManager.loadingFullContent", {
                    defaultValue: "正在读取完整内容…",
                  })
                : t("sessionManager.copyMessage", {
                    defaultValue: "复制消息",
                  })
            }
            aria-busy={isCopying || undefined}
            disabled={isCopying}
            onClick={handleCopy}
          >
            {isCopying ? (
              <>
                <LoaderCircle
                  className="size-3.5 animate-spin"
                  aria-hidden="true"
                />
                <span>
                  {t("sessionManager.loadingFullContent", {
                    defaultValue: "正在读取完整内容…",
                  })}
                </span>
              </>
            ) : (
              <Copy className="size-3.5" aria-hidden="true" />
            )}
          </button>
        </span>
      </div>
      <div
        className={cn(
          "asm-message-body",
          isToolPreviewFolded && "is-tool-preview",
        )}
      >
        {!isTool && !showRaw ? (
          <MessageMarkdown
            content={renderedContent}
            highlightTerms={highlightTerms}
            onCopyCode={handleCodeCopy}
          />
        ) : (
          highlightText(renderedContent, highlightTerms)
        )}
      </div>
      {isToolLineFoldable && !hasHiddenSearchHit && (
        <button
          type="button"
          aria-expanded={toolExpanded}
          onClick={() => setToolExpanded((prev) => !prev)}
          className="asm-tool-fold-toggle"
        >
          {toolExpanded ? (
            <>
              <ChevronUp className="size-3" aria-hidden="true" />
              {t("sessionManager.collapseToolOutput", {
                defaultValue: "收起工具输出",
              })}
            </>
          ) : (
            <>
              <ChevronDown className="size-3" aria-hidden="true" />
              {t("sessionManager.expandToolOutput", {
                defaultValue: "展开工具输出（共 {{count}} 行）",
                count: toolLines.length,
              })}
            </>
          )}
        </button>
      )}
      {codexSplit && (
        <div className="asm-ide-context">
          <button
            type="button"
            aria-expanded={showIdeContext}
            onClick={() => setShowIdeContext((prev) => !prev)}
            className="asm-ide-context-toggle"
          >
            {showIdeContext ? (
              <ChevronUp className="size-3" aria-hidden="true" />
            ) : (
              <ChevronDown className="size-3" aria-hidden="true" />
            )}
            <span>
              {showIdeContext
                ? t("sessionManager.hideIdeContext", {
                    defaultValue: "收起 IDE 上下文",
                  })
                : t("sessionManager.showIdeContext", {
                    defaultValue: "查看 IDE 上下文",
                  })}
            </span>
          </button>
          {showIdeContext && (
            <pre className="asm-ide-context-body">
              {highlightText(codexSplit.context, highlightTerms)}
            </pre>
          )}
        </div>
      )}
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
}
