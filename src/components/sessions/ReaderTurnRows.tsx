import {
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Code2,
  Copy,
  LoaderCircle,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { SessionMessage } from "@/types";
import { AgentIcon } from "./AgentIcon";
import {
  classifyToolKind,
  shortenProjectPath,
  type AgentReaderStyle,
} from "./agentReaderStyles";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";
import {
  extractFullCodeBlock,
  hasRichMarkdown,
  MessageMarkdown,
} from "./MessageMarkdown";
import { extractDiffPayload, SessionDiffView } from "./SessionDiffView";
import {
  extractExitCode,
  extractToolName,
  isErrorToolMessage,
  type SessionTurnGroup,
} from "./turnGrouping";
import {
  formatMessageTimestamp,
  getProviderIconName,
  splitCodexIdeContext,
} from "./utils";

const COLLAPSE_THRESHOLD = 3000;
const COLLAPSED_LENGTH = 1500;
const THINKING_PREVIEW_CHARS = 96;
const TOOL_OUTPUT_PREVIEW_LINES = 8;

const formatCompactChars = (count: number) =>
  count >= 1000 ? `${(count / 1000).toFixed(1)}k` : String(count);

function useLazyFullMessage(
  message: SessionMessage,
  onLoadFullContent?: (message: SessionMessage) => Promise<string>,
) {
  const [fullContent, setFullContent] = useState<string | null>(null);
  const [isLoadingFull, setIsLoadingFull] = useState(false);
  const mountedRef = useRef(false);
  const cachedRef = useRef<string | null>(null);
  const pendingRef = useRef<Promise<string> | null>(null);

  useLayoutEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const isTruncated = message.truncated === true && message.index !== undefined;
  const isUnloadedTruncated = isTruncated && fullContent === null;
  const loaded = fullContent ?? message.content;

  const loadFullContent = useCallback((): Promise<string> => {
    if (cachedRef.current !== null) {
      return Promise.resolve(cachedRef.current);
    }
    if (pendingRef.current) return pendingRef.current;
    if (!isTruncated || !onLoadFullContent) {
      return Promise.resolve(message.content);
    }
    setIsLoadingFull(true);
    const req = Promise.resolve()
      .then(() => onLoadFullContent(message))
      .then((content) => {
        if (mountedRef.current) {
          cachedRef.current = content;
          setFullContent(content);
        }
        return content;
      })
      .finally(() => {
        pendingRef.current = null;
        if (mountedRef.current) setIsLoadingFull(false);
      });
    pendingRef.current = req;
    return req;
  }, [isTruncated, message, onLoadFullContent]);

  return {
    loaded,
    isTruncated,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  };
}

// ── Agent 侧左栏头像与轮次标题 ───────────────────────────────────────────

export const ReaderAgentLane = memo(function ReaderAgentLane({
  providerId,
  providerLabel,
  isAgentStart,
  agentStartTs,
  isActive,
  children,
}: {
  providerId: string;
  providerLabel: string;
  isAgentStart: boolean;
  agentStartTs?: number;
  isActive?: boolean;
  children: ReactNode;
}) {
  return (
    <div className={cn("asm-ccs-agent-lane", isActive && "is-active-row")}>
      {isAgentStart && (
        <>
          <span className="asm-ccs-avatar" aria-hidden="true">
            <AgentIcon
              icon={getProviderIconName(providerId)}
              name={providerLabel}
              size={16}
            />
          </span>
          <div className="asm-ccs-agent-header">
            <span className="asm-ccs-agent-name">{providerLabel}</span>
            {agentStartTs ? (
              <time
                dateTime={new Date(agentStartTs).toISOString()}
                title={new Date(agentStartTs).toLocaleString()}
                className="asm-ccs-time"
              >
                {formatMessageTimestamp(agentStartTs)}
              </time>
            ) : null}
          </div>
        </>
      )}
      {children}
    </div>
  );
});

// ── 用户提问气泡（靠右，气泡外放时间与复制，底色随 Agent 主题浅染） ──────

export const ReaderQuestionRow = memo(function ReaderQuestionRow(props: {
  message: SessionMessage;
  messageRevision?: string | number;
  isCodexSession?: boolean;
  isActive?: boolean;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const identity = JSON.stringify([
    props.messageRevision,
    props.isCodexSession ?? false,
    props.message.content,
    props.message.ts,
    props.message.index,
    props.message.truncated,
    props.message.contentChars,
    props.message.contentIdentity,
  ]);
  return <ReaderQuestionInner key={identity} {...props} />;
});

function ReaderQuestionInner({
  message,
  isCodexSession = false,
  isActive = false,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
  onLoadFullContent,
}: {
  message: SessionMessage;
  isCodexSession?: boolean;
  isActive?: boolean;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [showIdeContext, setShowIdeContext] = useState(false);
  const [isCopying, setIsCopying] = useState(false);
  const {
    loaded,
    isTruncated,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  } = useLazyFullMessage(message, onLoadFullContent);

  const codexSplit = isCodexSession ? splitCodexIdeContext(loaded) : null;
  const primaryText = codexSplit ? codexSplit.prompt : loaded;
  const isLong = isTruncated || primaryText.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const totalChars =
    isTruncated || !codexSplit
      ? (message.contentChars ?? primaryText.length)
      : primaryText.length;

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  useEffect(() => {
    if (
      normalizedTerms.length > 0 &&
      primaryText.length > COLLAPSE_THRESHOLD &&
      normalizedTerms.some((term) =>
        primaryText.slice(COLLAPSED_LENGTH).toLocaleLowerCase().includes(term),
      )
    ) {
      setExpanded(true);
    }
  }, [normalizedTerms, primaryText]);

  useEffect(() => {
    if (
      codexSplit &&
      normalizedTerms.length > 0 &&
      normalizedTerms.some((term) =>
        codexSplit.context.toLocaleLowerCase().includes(term),
      )
    ) {
      setShowIdeContext(true);
    }
  }, [codexSplit, normalizedTerms]);

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    const lower = loaded.toLocaleLowerCase();
    if (normalizedTerms.every((term) => lower.includes(term))) return;
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [
    isActiveSearchMatch,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    mountedRef,
    normalizedTerms,
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
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [expanded, isUnloadedTruncated, loadFullContent, mountedRef]);

  const handleCopy = useCallback(() => {
    if (isCopying) return;
    if (!isUnloadedTruncated) {
      onCopy(loaded);
      return;
    }
    setIsCopying(true);
    void loadFullContent()
      .then((full) => {
        if (mountedRef.current) onCopy(full);
      })
      .catch(() => undefined)
      .finally(() => {
        if (mountedRef.current) setIsCopying(false);
      });
  }, [
    isCopying,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    mountedRef,
    onCopy,
  ]);

  const displaySlice = collapsed
    ? primaryText.slice(0, COLLAPSED_LENGTH)
    : primaryText;

  return (
    <div
      className={cn(
        "asm-ccs-question-row group/q",
        isActive && "is-active-row",
      )}
    >
      <div className="asm-ccs-question-wrap">
        <div className="asm-ccs-question-meta">
          <span className="sr-only">
            {t("sessionManager.roleUser", { defaultValue: "用户" })}
          </span>
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                className="asm-ccs-hover-icon"
                aria-label={t("sessionManager.copyMessage", {
                  defaultValue: "复制消息",
                })}
                disabled={isCopying}
                onClick={handleCopy}
              >
                {isCopying ? (
                  <LoaderCircle className="size-3.5 animate-spin" />
                ) : (
                  <Copy className="size-3.5" />
                )}
              </button>
            </TooltipTrigger>
            <TooltipContent>
              {t("sessionManager.copyMessage", { defaultValue: "复制消息" })}
            </TooltipContent>
          </Tooltip>
          {message.ts ? (
            <time
              dateTime={new Date(message.ts).toISOString()}
              title={new Date(message.ts).toLocaleString()}
              className="asm-ccs-time"
            >
              {formatMessageTimestamp(message.ts)}
            </time>
          ) : null}
        </div>
        <div className="asm-ccs-question-bubble">
          {hasRichMarkdown(displaySlice) ? (
            <MessageMarkdown
              content={displaySlice}
              collapsed={collapsed}
              highlightTerms={highlightTerms}
              onCopyCode={(code, blockIdx) => {
                const target = onCopyCode ?? onCopy;
                if (!collapsed) return target(code);
                if (!isUnloadedTruncated) {
                  return target(
                    extractFullCodeBlock(primaryText, blockIdx, code),
                  );
                }
                return loadFullContent().then((full) =>
                  target(extractFullCodeBlock(full, blockIdx, code)),
                );
              }}
              onOpenUrl={onOpenUrl}
            />
          ) : (
            <div className="asm-ccs-question-text">
              {highlightText(
                collapsed ? `${displaySlice}…` : displaySlice,
                highlightTerms,
              )}
            </div>
          )}
          {isLong && (
            <button
              type="button"
              aria-expanded={expanded}
              onClick={handleToggle}
              className="asm-ccs-inline-expand"
            >
              {expanded ? (
                t("sessionManager.collapseContent", { defaultValue: "收起" })
              ) : isLoadingFull ? (
                t("sessionManager.loadingFullContent", {
                  defaultValue: "正在读取完整内容…",
                })
              ) : (
                <>
                  {t("sessionManager.expandContent", {
                    defaultValue: "展开完整内容",
                  })}
                  <span className="asm-ccs-muted-num">
                    ({formatCompactChars(totalChars)})
                  </span>
                </>
              )}
            </button>
          )}
          {codexSplit && (
            <div className="asm-ide-context mt-2">
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
        </div>
      </div>
    </div>
  );
}

// ── 执行过程折叠摘要条（cc-switch 单行无框风格 + 悬停复制本轮） ───────────

export const ReaderTimelineRow = memo(function ReaderTimelineRow({
  turn,
  expanded,
  onToggle,
  onCopyTurn,
}: {
  turn: SessionTurnGroup;
  expanded: boolean;
  onToggle: () => void;
  onCopyTurn?: () => void;
}) {
  const { t } = useTranslation();
  const metaParts: Array<{
    key: string;
    text: string;
    tone?: "change" | "error";
  }> = [];
  if (turn.commandCount > 0) {
    metaParts.push({
      key: "cmd",
      text: `${turn.commandCount} 个命令`,
    });
  }
  if (turn.editCount > 0) {
    metaParts.push({
      key: "edit",
      text: t("sessionManager.changesCount", {
        defaultValue: "{{count}} 处改动",
        count: turn.editCount,
      }),
      tone: "change",
    });
  }
  if (turn.errorCount > 0) {
    metaParts.push({
      key: "err",
      text: t("sessionManager.errorsCount", {
        defaultValue: "{{count}} 处异常",
        count: turn.errorCount,
      }),
      tone: "error",
    });
  }

  return (
    <div className="asm-ccs-timeline-wrap group/tl">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={onToggle}
        className="asm-ccs-row-btn"
      >
        <ChevronRight
          aria-hidden="true"
          className={cn("asm-ccs-chevron", expanded && "is-rotated")}
        />
        <span className="asm-ccs-timeline-summary">
          <span className="asm-ccs-timeline-main">
            {t("sessionManager.executionSteps", {
              defaultValue: "执行过程 · {{count}} 步",
              count: turn.stepIndexes.length,
            })}
          </span>
          {metaParts.map((part) => (
            <span key={part.key}>
              <span aria-hidden="true" className="asm-ccs-dot">
                ·
              </span>
              <span
                className={cn(
                  part.tone === "change" && "asm-ccs-tone-success",
                  part.tone === "error" && "asm-ccs-tone-danger",
                )}
              >
                {part.text}
              </span>
            </span>
          ))}
          {metaParts.length === 0 &&
            turn.toolChips.slice(0, 3).map((chip) => (
              <span key={chip.name}>
                <span aria-hidden="true" className="asm-ccs-dot">
                  ·
                </span>
                <span className="font-mono text-[11.5px]">
                  {chip.name}
                  {chip.count > 1 ? ` ×${chip.count}` : ""}
                </span>
              </span>
            ))}
        </span>
      </button>
      {onCopyTurn && (
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              aria-label="复制这一轮对话"
              onClick={onCopyTurn}
              className="asm-ccs-hover-icon opacity-0 group-hover/tl:opacity-100"
            >
              <Copy className="size-3.5" />
            </button>
          </TooltipTrigger>
          <TooltipContent>复制这一轮为 Markdown</TooltipContent>
        </Tooltip>
      )}
    </div>
  );
});

// ── 单步渲染：思考过程单行 / 工具调用单行（支持展开 ⎿ 输出与行号 Diff） ───

function parseStructuredToolStep(
  content: string,
  style: AgentReaderStyle,
  projectDir?: string | null,
) {
  const rawToolName = extractToolName(content);
  const lines = content.replace(/\r\n/g, "\n").split("\n");
  const bodyLines = rawToolName ? lines.slice(1) : lines;

  if (!rawToolName) {
    const firstNonEmpty =
      lines.find((line) => line.trim().length > 0)?.trim() ?? "工具输出";
    return {
      hasHeader: false,
      verb: "输出",
      target:
        firstNonEmpty.length > 72
          ? `${firstNonEmpty.slice(0, 72)}…`
          : firstNonEmpty,
      bodyText: content,
      totalLines: lines.length,
    };
  }

  const kind = classifyToolKind(rawToolName);
  const customVerb = style.verbs[kind];
  const verb = customVerb || rawToolName;

  // 提取第一行关键目标（路径或命令），剩余作为展开详情
  let target = "";
  let detailStartIdx = 0;
  for (let i = 0; i < bodyLines.length; i += 1) {
    const trimmed = bodyLines[i].trim();
    if (!trimmed) continue;
    if (
      trimmed.startsWith("```") ||
      trimmed.startsWith("@@ ") ||
      trimmed.startsWith("--- ") ||
      trimmed.startsWith("*** Begin Patch") ||
      /^\*\*\* (?:Add|Update|Delete) File:/.test(trimmed) ||
      trimmed.startsWith("diff --git ")
    ) {
      const patchFile = bodyLines.find((l) =>
        /^\*\*\* (?:Add|Update|Delete) File:\s*(.+)$/.test(l),
      );
      if (patchFile) {
        target = patchFile.replace(
          /^\*\*\* (?:Add|Update|Delete) File:\s*/,
          "",
        );
      }
      detailStartIdx = i;
      break;
    }
    target = trimmed.replace(/^\$\s+/, "");
    detailStartIdx = i + 1;
    break;
  }

  if (["read", "edit", "write", "search"].includes(kind) && target) {
    target = shortenProjectPath(target, projectDir);
  } else if (target.length > 84) {
    target = `${target.slice(0, 84)}…`;
  }

  const remainingBody = bodyLines.slice(detailStartIdx).join("\n").trim();

  return {
    hasHeader: true,
    verb,
    target,
    bodyText: remainingBody,
    totalLines: remainingBody ? remainingBody.split("\n").length : 0,
  };
}

export const ReaderStepRow = memo(function ReaderStepRow(props: {
  message: SessionMessage;
  messageRevision?: string | number;
  style: AgentReaderStyle;
  projectDir?: string | null;
  inTimeline?: boolean;
  standaloneOpen?: boolean;
  isSearchMatch?: boolean;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const identity = JSON.stringify([
    props.messageRevision,
    props.message.role,
    props.message.content,
    props.message.ts,
    props.message.index,
    props.message.truncated,
    props.message.contentChars,
    props.message.contentIdentity,
    props.standaloneOpen ?? false,
  ]);
  return <ReaderStepInner key={identity} {...props} />;
});

function ReaderStepInner({
  message,
  style,
  projectDir,
  inTimeline = false,
  standaloneOpen = false,
  isSearchMatch = false,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
  onLoadFullContent,
}: {
  message: SessionMessage;
  style: AgentReaderStyle;
  projectDir?: string | null;
  inTimeline?: boolean;
  standaloneOpen?: boolean;
  isSearchMatch?: boolean;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const { t } = useTranslation();
  const roleLower = message.role.toLowerCase();
  const isThinking = roleLower === "thinking";
  const isIntermediateNote = roleLower === "assistant";

  const [expanded, setExpanded] = useState(standaloneOpen);
  const [outputShowAll, setOutputShowAll] = useState(false);

  const {
    loaded,
    isTruncated,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  } = useLazyFullMessage(message, onLoadFullContent);

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  useEffect(() => {
    if (isSearchMatch && normalizedTerms.length > 0) {
      setExpanded(true);
      setOutputShowAll(true);
    }
  }, [isSearchMatch, normalizedTerms.length]);

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    const lower = loaded.toLocaleLowerCase();
    if (normalizedTerms.every((term) => lower.includes(term))) return;
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) {
          setExpanded(true);
          setOutputShowAll(true);
        }
      })
      .catch(() => undefined);
  }, [
    isActiveSearchMatch,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    mountedRef,
    normalizedTerms,
  ]);

  if (isIntermediateNote) {
    return (
      <div className={cn("asm-ccs-note", inTimeline && "is-in-timeline")}>
        <MessageMarkdown
          content={loaded}
          highlightTerms={highlightTerms}
          onCopyCode={(code) => (onCopyCode ?? onCopy)(code)}
          onOpenUrl={onOpenUrl}
        />
      </div>
    );
  }

  if (isThinking) {
    const singleLine = loaded
      .replace(/\*\*(.+?)\*\*/g, "$1")
      .replace(/\s+/g, " ")
      .trim();
    const preview =
      singleLine.length > THINKING_PREVIEW_CHARS
        ? `${singleLine.slice(0, THINKING_PREVIEW_CHARS)}…`
        : singleLine;
    const totalChars = message.contentChars ?? loaded.length;

    const handleThinkingToggle = () => {
      if (expanded) {
        setExpanded(false);
        return;
      }
      if (!isUnloadedTruncated) {
        setExpanded(true);
        return;
      }
      void loadFullContent()
        .then(() => {
          if (mountedRef.current) setExpanded(true);
        })
        .catch(() => undefined);
    };

    return (
      <div className={cn("min-w-0", inTimeline && "asm-ccs-in-timeline")}>
        <button
          type="button"
          aria-expanded={expanded}
          aria-label={
            expanded
              ? t("sessionManager.collapseThinking", {
                  defaultValue: "收起思考过程",
                })
              : t("sessionManager.expandThinking", {
                  defaultValue: "展开思考过程",
                })
          }
          onClick={handleThinkingToggle}
          className="asm-ccs-row-btn"
        >
          <span
            className="asm-ccs-glyph text-muted-foreground"
            aria-hidden="true"
          >
            {style.thinkingGlyph || "✻"}
          </span>
          <span className="shrink-0 italic text-foreground/75">
            {t("sessionManager.roleThinking", { defaultValue: "思考过程" })}
          </span>
          {!expanded && preview && (
            <span className="min-w-0 flex-1 truncate text-muted-foreground">
              {highlightText(preview, highlightTerms)}
            </span>
          )}
          <span className="asm-ccs-step-meta">
            {isLoadingFull
              ? t("sessionManager.loadingFullContent", {
                  defaultValue: "正在读取完整内容…",
                })
              : `${formatCompactChars(totalChars)} 字`}
          </span>
        </button>
        {expanded && (
          <div className="asm-ccs-thinking-body">
            <MessageMarkdown
              content={loaded}
              highlightTerms={highlightTerms}
              onCopyCode={(code) => (onCopyCode ?? onCopy)(code)}
              onOpenUrl={onOpenUrl}
            />
          </div>
        )}
      </div>
    );
  }

  // Tool 调用与结果行
  const parsed = parseStructuredToolStep(loaded, style, projectDir);
  const diffInfo = extractDiffPayload(loaded);
  const isError = isErrorToolMessage(message);
  const exitCode = extractExitCode(loaded);
  const glyph = isError ? style.stepGlyph.error : style.stepGlyph.success;
  const totalChars = message.contentChars ?? loaded.length;

  const handleLoadFull = () => {
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) {
          setExpanded(true);
          setOutputShowAll(true);
        }
      })
      .catch(() => undefined);
  };

  const bodyLines = parsed.bodyText ? parsed.bodyText.split("\n") : [];
  const hasMoreLines =
    !diffInfo && !outputShowAll && bodyLines.length > TOOL_OUTPUT_PREVIEW_LINES;
  const displayedBody = hasMoreLines
    ? bodyLines.slice(0, TOOL_OUTPUT_PREVIEW_LINES).join("\n")
    : parsed.bodyText;

  const canExpand = Boolean(parsed.bodyText || diffInfo || isTruncated);

  return (
    <div className={cn("min-w-0", inTimeline && "asm-ccs-in-timeline")}>
      <button
        type="button"
        aria-expanded={canExpand ? expanded : undefined}
        onClick={() => {
          if (canExpand) setExpanded((prev) => !prev);
        }}
        className={cn("asm-ccs-row-btn", !canExpand && "is-static")}
      >
        <span
          className={cn(
            "asm-ccs-glyph",
            isError ? "asm-ccs-tone-danger" : "asm-ccs-tone-success",
          )}
          aria-hidden="true"
        >
          {glyph}
        </span>
        <span
          className={cn(
            "min-w-0 truncate",
            style.monoTitles ? "font-mono text-[12.5px]" : "text-[13px]",
          )}
        >
          <span className="font-semibold text-foreground/90">
            {!parsed.hasHeader && expanded
              ? t("sessionManager.roleTool", { defaultValue: "工具" })
              : parsed.verb}
          </span>
          {parsed.target &&
            (parsed.hasHeader || !expanded) &&
            (style.titleFormat === "call" && parsed.hasHeader ? (
              <>
                <span className="text-muted-foreground">(</span>
                <span className="text-foreground/85">
                  {highlightText(parsed.target, highlightTerms)}
                </span>
                <span className="text-muted-foreground">)</span>
              </>
            ) : (
              <>
                {" "}
                <span className="font-mono text-[12px] text-foreground/85">
                  {highlightText(parsed.target, highlightTerms)}
                </span>
              </>
            ))}
        </span>
        <span className="asm-ccs-step-meta">
          {exitCode !== undefined && exitCode !== 0 && (
            <span className="asm-ccs-tone-danger">exit {exitCode}</span>
          )}
          {diffInfo && (
            <span>
              <span className="asm-ccs-tone-success">+{diffInfo.added}</span>{" "}
              <span className="asm-ccs-tone-danger">−{diffInfo.removed}</span>
            </span>
          )}
          {!diffInfo && parsed.totalLines > 1 && (
            <span>{parsed.totalLines} 行</span>
          )}
          {canExpand && (
            <ChevronRight
              aria-hidden="true"
              className={cn(
                "asm-ccs-chevron is-subtle",
                expanded && "is-rotated",
              )}
            />
          )}
        </span>
      </button>
      {expanded && canExpand && (
        <div className="asm-ccs-step-details">
          {style.resultConnector && (
            <span className="asm-ccs-connector" aria-hidden="true">
              {style.resultConnector}
            </span>
          )}
          <div className="min-w-0 flex-1">
            {diffInfo ? (
              <SessionDiffView
                diffText={diffInfo.diffText}
                projectDir={projectDir}
                highlightTerms={highlightTerms}
              />
            ) : (
              <pre className={cn("asm-ccs-tool-pre", isError && "is-error")}>
                {highlightText(displayedBody, highlightTerms)}
                {isUnloadedTruncated && "…"}
              </pre>
            )}
            {(hasMoreLines || isUnloadedTruncated) && (
              <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
                {hasMoreLines && !isUnloadedTruncated && (
                  <button
                    type="button"
                    onClick={() => setOutputShowAll(true)}
                    className="asm-ccs-inline-expand"
                  >
                    显示全部（共 {bodyLines.length} 行）
                  </button>
                )}
                {isUnloadedTruncated && (
                  <button
                    type="button"
                    aria-expanded={false}
                    onClick={handleLoadFull}
                    className="asm-ccs-inline-expand"
                  >
                    {isLoadingFull
                      ? t("sessionManager.loadingFullContent", {
                          defaultValue: "正在读取完整内容…",
                        })
                      : `${t("sessionManager.expandContent", {
                          defaultValue: "展开完整内容",
                        })} (${formatCompactChars(totalChars)})`}
                  </button>
                )}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

// ── 最终回复行（无底色、无边框文档流，右上悬停操作） ─────────────────────

export const ReaderFinalReplyRow = memo(function ReaderFinalReplyRow(props: {
  message: SessionMessage;
  messageRevision?: string | number;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const identity = JSON.stringify([
    props.messageRevision,
    props.message.content,
    props.message.ts,
    props.message.index,
    props.message.truncated,
    props.message.contentChars,
    props.message.contentIdentity,
  ]);
  return <ReaderFinalReplyInner key={identity} {...props} />;
});

function ReaderFinalReplyInner({
  message,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
  onLoadFullContent,
}: {
  message: SessionMessage;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [showRaw, setShowRaw] = useState(false);
  const [isCopying, setIsCopying] = useState(false);
  const {
    loaded,
    isTruncated,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  } = useLazyFullMessage(message, onLoadFullContent);

  const isLong = isTruncated || loaded.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const totalChars = message.contentChars ?? loaded.length;
  const displaySlice = collapsed ? loaded.slice(0, COLLAPSED_LENGTH) : loaded;
  const canRenderMarkdown = hasRichMarkdown(displaySlice);

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  useEffect(() => {
    if (
      normalizedTerms.length > 0 &&
      loaded.length > COLLAPSE_THRESHOLD &&
      normalizedTerms.some((term) =>
        loaded.slice(COLLAPSED_LENGTH).toLocaleLowerCase().includes(term),
      )
    ) {
      setExpanded(true);
    }
  }, [loaded, normalizedTerms]);

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    const lower = loaded.toLocaleLowerCase();
    if (normalizedTerms.every((term) => lower.includes(term))) return;
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [
    isActiveSearchMatch,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    mountedRef,
    normalizedTerms,
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
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [expanded, isUnloadedTruncated, loadFullContent, mountedRef]);

  const handleCopy = useCallback(() => {
    if (isCopying) return;
    if (!isUnloadedTruncated) {
      onCopy(loaded);
      return;
    }
    setIsCopying(true);
    void loadFullContent()
      .then((full) => {
        if (mountedRef.current) onCopy(full);
      })
      .catch(() => undefined)
      .finally(() => {
        if (mountedRef.current) setIsCopying(false);
      });
  }, [
    isCopying,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    mountedRef,
    onCopy,
  ]);

  const handleCodeCopy = useCallback(
    (code: string, blockIdx: number): Promise<void> | void => {
      const target = onCopyCode ?? onCopy;
      if (!collapsed) return target(code);
      if (!isUnloadedTruncated) {
        return target(extractFullCodeBlock(loaded, blockIdx, code));
      }
      return loadFullContent().then((full) => {
        if (!mountedRef.current) return;
        return target(extractFullCodeBlock(full, blockIdx, code));
      });
    },
    [
      collapsed,
      isUnloadedTruncated,
      loadFullContent,
      loaded,
      mountedRef,
      onCopy,
      onCopyCode,
    ],
  );

  return (
    <div className="asm-ccs-final group/final">
      <div className="asm-ccs-final-actions">
        {canRenderMarkdown && (
          <button
            type="button"
            className="asm-ccs-hover-pill"
            aria-pressed={showRaw}
            aria-label={
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
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className="asm-ccs-hover-icon"
              aria-label={t("sessionManager.copyMessage", {
                defaultValue: "复制消息",
              })}
              disabled={isCopying}
              onClick={handleCopy}
            >
              {isCopying ? (
                <LoaderCircle className="size-3.5 animate-spin" />
              ) : (
                <Copy className="size-3.5" />
              )}
            </button>
          </TooltipTrigger>
          <TooltipContent>
            {t("sessionManager.copyMessage", { defaultValue: "复制消息" })}
          </TooltipContent>
        </Tooltip>
      </div>
      <div className="asm-ccs-final-body">
        {!showRaw ? (
          <MessageMarkdown
            content={displaySlice}
            collapsed={collapsed}
            highlightTerms={highlightTerms}
            onCopyCode={handleCodeCopy}
            onOpenUrl={onOpenUrl}
          />
        ) : (
          <div className="whitespace-pre-wrap break-words font-mono text-[12.5px]">
            {highlightText(
              collapsed ? `${displaySlice}…` : displaySlice,
              highlightTerms,
            )}
          </div>
        )}
        {isLong && (
          <button
            type="button"
            aria-expanded={expanded}
            onClick={handleToggle}
            className="asm-ccs-inline-expand mt-1.5"
          >
            {expanded ? (
              t("sessionManager.collapseContent", { defaultValue: "收起" })
            ) : isLoadingFull ? (
              t("sessionManager.loadingFullContent", {
                defaultValue: "正在读取完整内容…",
              })
            ) : (
              <>
                {t("sessionManager.expandContent", {
                  defaultValue: "展开完整内容",
                })}
                <span className="asm-ccs-muted-num">
                  ({formatCompactChars(totalChars)})
                </span>
              </>
            )}
          </button>
        )}
      </div>
    </div>
  );
}
