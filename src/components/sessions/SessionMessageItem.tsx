import {
  memo,
  useCallback,
  useEffect,
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
import {
  extractFullCodeBlock,
  hasRichMarkdown,
  MessageMarkdown,
} from "./MessageMarkdown";
import {
  formatMessageTimestamp,
  getRoleLabel,
  splitCodexIdeContext,
} from "./utils";

const COLLAPSE_THRESHOLD = 3000;
const COLLAPSED_LENGTH = 1500;
const THINKING_PREVIEW_LENGTH = 100;
const TOOL_FOLD_LINE_LIMIT = 8;
const TOOL_PREVIEW_LINES = 4;

interface SessionMessageItemProps {
  message: SessionMessage;
  messageRevision?: string | number;
  isActive: boolean;
  isCodexSession?: boolean;
  highlightTerms?: string[];
  isSearchMatch?: boolean;
  isActiveSearchMatch?: boolean;
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
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
  isSearchMatch = false,
  isActiveSearchMatch = false,
  onCopy,
  onCopyCode,
  onLoadFullContent,
}: SessionMessageItemProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [thinkingExpanded, setThinkingExpanded] = useState(false);
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

  const roleLower = message.role.toLowerCase();
  const isThinking = roleLower === "thinking";
  const isTool =
    roleLower !== "user" && roleLower !== "assistant" && !isThinking;

  const loaded = fullContent ?? message.content;
  const codexSplit =
    isCodexSession && roleLower === "user"
      ? splitCodexIdeContext(loaded)
      : null;
  const primaryText = codexSplit ? codexSplit.prompt : loaded;

  const isTruncated = message.truncated === true && message.index !== undefined;
  const isUnloadedTruncated = isTruncated && fullContent === null;
  // 未截断的长正文仍就地折叠；被截断的正文里只有预览，展开要向后端取
  const isLong =
    !isThinking && (isTruncated || primaryText.length > COLLAPSE_THRESHOLD);
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

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  const previewHasAllTerms = useMemo(() => {
    if (normalizedTerms.length === 0) return false;
    const lowerLoaded = loaded.toLocaleLowerCase();
    return normalizedTerms.every((term) => lowerLoaded.includes(term));
  }, [loaded, normalizedTerms]);

  const hasHiddenLongHit = useMemo(() => {
    if (
      isThinking ||
      normalizedTerms.length === 0 ||
      primaryText.length <= COLLAPSE_THRESHOLD
    ) {
      return false;
    }
    const hiddenTail = primaryText.slice(COLLAPSED_LENGTH).toLocaleLowerCase();
    return normalizedTerms.some((term) => hiddenTail.includes(term));
  }, [isThinking, normalizedTerms, primaryText]);

  const hasIdeContextSearchHit = useMemo(() => {
    if (!codexSplit || normalizedTerms.length === 0) return false;
    const lowerContext = codexSplit.context.toLocaleLowerCase();
    return normalizedTerms.some((term) => lowerContext.includes(term));
  }, [codexSplit, normalizedTerms]);

  const hasLocalThinkingHit = useMemo(() => {
    if (!isThinking || normalizedTerms.length === 0) return false;
    const lowerText = primaryText.toLocaleLowerCase();
    return normalizedTerms.every((term) => lowerText.includes(term));
  }, [isThinking, normalizedTerms, primaryText]);

  useEffect(() => {
    if (hasHiddenLongHit) {
      setExpanded(true);
    }
  }, [hasHiddenLongHit]);

  useEffect(() => {
    if (hasIdeContextSearchHit) {
      setShowIdeContext(true);
    }
  }, [hasIdeContextSearchHit]);

  useEffect(() => {
    if (isThinking && (isSearchMatch || hasLocalThinkingHit)) {
      if (!isUnloadedTruncated || previewHasAllTerms) {
        setThinkingExpanded(true);
      }
    }
  }, [
    hasLocalThinkingHit,
    isSearchMatch,
    isThinking,
    isUnloadedTruncated,
    previewHasAllTerms,
  ]);

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0 ||
      previewHasAllTerms
    ) {
      return;
    }
    void loadFullContent()
      .then(() => {
        if (!mounted.current) return;
        if (isThinking) {
          setThinkingExpanded(true);
        } else {
          setExpanded(true);
        }
      })
      .catch(() => undefined);
  }, [
    isActiveSearchMatch,
    isThinking,
    isUnloadedTruncated,
    loadFullContent,
    normalizedTerms.length,
    previewHasAllTerms,
  ]);

  const handleToggle = useCallback(() => {
    if (expanded) {
      setExpanded(false);
      return;
    }
    if (!isUnloadedTruncated) {
      setExpanded(true);
      return;
    }
    // 失败提示由调用方给出；这里保持折叠状态，不做假的展开
    void loadFullContent()
      .then(() => {
        if (mounted.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [expanded, isUnloadedTruncated, loadFullContent]);

  const handleThinkingToggle = useCallback(() => {
    if (thinkingExpanded) {
      setThinkingExpanded(false);
      return;
    }
    if (!isUnloadedTruncated) {
      setThinkingExpanded(true);
      return;
    }
    void loadFullContent()
      .then(() => {
        if (mounted.current) setThinkingExpanded(true);
      })
      .catch(() => undefined);
  }, [isUnloadedTruncated, loadFullContent, thinkingExpanded]);

  const handleCopy = useCallback(() => {
    if (copying.current) return;
    if (!isUnloadedTruncated) {
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
  }, [isUnloadedTruncated, loadFullContent, loaded, onCopy]);

  const collapsedSlice = useMemo(
    () => (collapsed ? primaryText.slice(0, COLLAPSED_LENGTH) : primaryText),
    [collapsed, primaryText],
  );

  const thinkingPreview = useMemo(() => {
    if (!isThinking) return "";
    const singleLine = primaryText.replace(/\s+/g, " ").trim();
    return singleLine.length > THINKING_PREVIEW_LENGTH
      ? `${singleLine.slice(0, THINKING_PREVIEW_LENGTH)}…`
      : singleLine;
  }, [isThinking, primaryText]);

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
    normalizedTerms.some((term) => hiddenToolText.includes(term));
  const isToolPreviewFolded =
    isToolLineFoldable && !toolExpanded && !hasHiddenSearchHit;

  const markdownSource = isThinking ? primaryText : collapsedSlice;
  const plainDisplayContent = isToolPreviewFolded
    ? toolLines.slice(0, TOOL_PREVIEW_LINES).join("\n")
    : collapsed
      ? `${collapsedSlice}…`
      : primaryText;

  const canRenderMarkdown =
    !isTool &&
    (!isThinking || thinkingExpanded) &&
    hasRichMarkdown(markdownSource);

  const handleCodeCopy = useCallback(
    (code: string, blockIndex: number): Promise<void> | void => {
      const copyTarget = onCopyCode ?? onCopy;
      if (!collapsed) {
        return copyTarget(code);
      }
      if (!isUnloadedTruncated) {
        const fullCode = extractFullCodeBlock(primaryText, blockIndex, code);
        return copyTarget(fullCode);
      }
      return loadFullContent().then((full) => {
        if (!mounted.current) return;
        const fullPrimary =
          isCodexSession && roleLower === "user"
            ? (splitCodexIdeContext(full)?.prompt ?? full)
            : full;
        const fullCode = extractFullCodeBlock(fullPrimary, blockIndex, code);
        return copyTarget(fullCode);
      });
    },
    [
      collapsed,
      isCodexSession,
      isUnloadedTruncated,
      loadFullContent,
      onCopy,
      onCopyCode,
      primaryText,
      roleLower,
    ],
  );

  return (
    <div
      className={cn(
        "asm-message group min-w-0",
        roleLower === "user"
          ? "is-user"
          : roleLower === "assistant"
            ? "is-assistant"
            : isThinking
              ? "is-thinking"
              : "is-tool",
        isActive && "is-active",
      )}
    >
      <div className="asm-message-role">
        <span className="asm-role-badge">
          <span className="asm-role-dot" aria-hidden="true" />
          <span>{getRoleLabel(message.role, t)}</span>
        </span>
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
      {isThinking && !thinkingExpanded ? (
        <div className="asm-thinking-preview">
          {highlightText(thinkingPreview, highlightTerms)}
        </div>
      ) : (
        <div
          className={cn(
            "asm-message-body",
            isToolPreviewFolded && "is-tool-preview",
          )}
        >
          {!isTool && !showRaw ? (
            <MessageMarkdown
              content={markdownSource}
              collapsed={collapsed}
              highlightTerms={highlightTerms}
              onCopyCode={handleCodeCopy}
            />
          ) : (
            highlightText(plainDisplayContent, highlightTerms)
          )}
        </div>
      )}
      {isThinking && (
        <button
          type="button"
          aria-expanded={thinkingExpanded}
          aria-busy={isLoadingFull || undefined}
          onClick={handleThinkingToggle}
          className="asm-thinking-toggle"
        >
          {thinkingExpanded ? (
            <>
              <ChevronUp className="size-3" aria-hidden="true" />
              {t("sessionManager.collapseThinking", {
                defaultValue: "收起思考过程",
              })}
            </>
          ) : (
            <>
              <ChevronDown className="size-3" aria-hidden="true" />
              {isLoadingFull
                ? t("sessionManager.loadingFullContent", {
                    defaultValue: "正在读取完整内容…",
                  })
                : t("sessionManager.expandThinking", {
                    defaultValue: "展开思考过程",
                  })}
            </>
          )}
        </button>
      )}
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
          className="asm-tool-fold-toggle"
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
