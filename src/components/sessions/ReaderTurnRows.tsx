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
  Bookmark,
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Code2,
  Copy,
  ExternalLink,
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
  formatDurationCompact,
  formatTokensCompact,
  shortenProjectPath,
  type AgentReaderStyle,
  type ToolActionKind,
  type ToolRunStatus,
} from "./agentReaderStyles";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";
import {
  extractFullCodeBlock,
  hasRichMarkdown,
  MessageMarkdown,
} from "./MessageMarkdown";
import { extractDiffPayload, SessionDiffView } from "./SessionDiffView";
import type {
  EventItem,
  MergedStep,
  NoteStep,
  ParsedToolCall,
  SessionTurnGroup,
  ThinkingStep,
  TimelineStep,
  ToolStep,
  TurnChapter,
} from "./turnGrouping";
import {
  formatMessageTimestamp,
  getProviderIconName,
  splitCodexIdeContext,
} from "./utils";

const COLLAPSE_THRESHOLD = 3000;
const COLLAPSED_LENGTH = 1500;

const formatCompactChars = (count: number) =>
  count >= 1000 ? `${Number((count / 1000).toFixed(1))}k` : String(count);

const stripBold = (text: string) => text.replace(/\*\*(.+?)\*\*/g, "$1").trim();

const prettyJson = (text: string) => {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
};

function extractFullShellCommand(call?: ParsedToolCall): string | null {
  if (
    !call ||
    call.kind !== "shell" ||
    call.inputTruncated ||
    !call.inputPreview
  ) {
    return null;
  }
  try {
    const parsed = JSON.parse(call.inputPreview) as Record<string, unknown>;
    for (const key of ["command", "cmd", "script", "CommandLine"]) {
      const val = parsed[key];
      if (typeof val === "string" && val.trim()) return val.trim();
      if (Array.isArray(val) && val.every((x) => typeof x === "string")) {
        return val.join(" ").trim();
      }
    }
  } catch {
    // non-JSON input
  }
  return null;
}

function useLazyFullMessage(
  message: SessionMessage | undefined,
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

  const isTruncated =
    Boolean(message?.truncated) && message?.index !== undefined;
  const isUnloadedTruncated = isTruncated && fullContent === null;
  const loaded = fullContent ?? message?.content ?? "";

  const loadFullContent = useCallback((): Promise<string> => {
    if (!message) return Promise.resolve("");
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

// ── 行首固定 16px 符号格（保证所有步骤标题像素级左对齐，同 cc-switch GlyphCell） ──

function GlyphCell({
  glyph,
  className,
}: {
  glyph: string;
  className?: string;
}) {
  return (
    <span aria-hidden="true" className={cn("asm-ccs-glyph", className)}>
      {glyph}
    </span>
  );
}

// ── Agent 侧左栏头像与轮次标题（含模型标识 Agent · 时间 · model） ────────────

export const ReaderAgentLane = memo(function ReaderAgentLane({
  providerId,
  providerLabel,
  isAgentStart,
  agentStartTs,
  model,
  isActive,
  children,
}: {
  providerId: string;
  providerLabel: string;
  isAgentStart: boolean;
  agentStartTs?: number;
  model?: string;
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
            {model && <span className="asm-ccs-model">· {model}</span>}
          </div>
        </>
      )}
      {children}
    </div>
  );
});

// ── 用户提问气泡（靠右，气泡外上方放时间与复制，底色随 Agent 主题浅染） ──────

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

// ── 执行过程折叠摘要行（对齐 cc-switch SessionTimeline） ───────────────────

export const ReaderTimelineRow = memo(function ReaderTimelineRow({
  turn,
  expanded,
  hiddenFailures = 0,
  onToggle,
  onCopyTurn,
}: {
  turn: SessionTurnGroup;
  expanded: boolean;
  hiddenFailures?: number;
  onToggle: () => void;
  onCopyTurn?: () => void;
}) {
  const { t } = useTranslation();
  const stepTotal = turn.steps.reduce(
    (sum, s) => sum + (s.kind === "merged" ? s.children.length : 1),
    0,
  );
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
  if (turn.durationMs !== undefined && turn.durationMs > 0) {
    metaParts.push({
      key: "dur",
      text: `⏱ ${formatDurationCompact(turn.durationMs)}`,
    });
  }
  if (turn.tokens !== undefined && turn.tokens > 0) {
    metaParts.push({
      key: "tok",
      text: `${formatTokensCompact(turn.tokens)} tok`,
    });
  }

  const noFinal = turn.finalAssistantIndex === null && !turn.aborted;

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
              count: stepTotal,
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
          {hiddenFailures > 0 && (
            <span className="ml-2 text-muted-foreground">
              还有 {hiddenFailures} 处异常
            </span>
          )}
          {noFinal && (
            <span className="ml-2 text-muted-foreground">
              这一轮没有最终回复
            </span>
          )}
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

// ── 章节行（Claude Code desktop mark_chapter 书签小标题） ──────────────────

export const ReaderChapterRow = memo(function ReaderChapterRow({
  chapter,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
}: {
  chapter: TurnChapter;
  highlightTerms?: string[];
}) {
  return (
    <div className="asm-ccs-chapter">
      <Bookmark
        aria-hidden="true"
        className="size-3.5 shrink-0 text-[var(--reader-accent)]"
      />
      <span className="sr-only">章节：</span>
      <span className="asm-ccs-chapter-title">
        {highlightText(chapter.title, highlightTerms)}
      </span>
      {chapter.summary && (
        <span className="asm-ccs-chapter-summary" title={chapter.summary}>
          {highlightText(chapter.summary, highlightTerms)}
        </span>
      )}
    </div>
  );
});

// ── 事件分隔行（上下文压缩 / 中断 / PR 链接 / Hook 报错等，同 SessionEventRow） ──

export const ReaderEventRow = memo(function ReaderEventRow({
  event,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  variant = "divider",
  onOpenUrl,
}: {
  event: EventItem;
  highlightTerms?: string[];
  variant?: "divider" | "inline";
  onOpenUrl?: (url: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const isDanger = event.tone === "danger";
  const labelNode = highlightText(event.label, highlightTerms);

  const inner =
    event.url && onOpenUrl ? (
      <button
        type="button"
        onClick={() => onOpenUrl(event.url!)}
        className="asm-ccs-event-btn is-link"
      >
        <span className="truncate">{labelNode}</span>
        <ExternalLink className="size-3 shrink-0" aria-hidden="true" />
      </button>
    ) : event.detail ? (
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((prev) => !prev)}
        className="asm-ccs-event-btn"
      >
        <span className="truncate">{labelNode}</span>
        <ChevronRight
          aria-hidden="true"
          className={cn(
            "size-3 shrink-0 transition-transform",
            open && "rotate-90",
          )}
        />
      </button>
    ) : (
      <span className="truncate">{labelNode}</span>
    );

  const detailBox = open && event.detail && (
    <pre className={cn("asm-ccs-event-detail", isDanger && "is-danger")}>
      {highlightText(event.detail, highlightTerms)}
    </pre>
  );

  if (variant === "inline") {
    return (
      <div className="min-w-0 py-0.5">
        <div
          className={cn(
            "asm-ccs-event-inline",
            isDanger && "asm-ccs-tone-danger",
          )}
        >
          <GlyphCell glyph="·" className="text-muted-foreground" />
          {inner}
        </div>
        {detailBox && <div className="pl-6">{detailBox}</div>}
      </div>
    );
  }

  return (
    <div className="asm-ccs-event-divider-wrap">
      <div
        role="note"
        className={cn(
          "asm-ccs-event-divider",
          isDanger && "asm-ccs-tone-danger",
        )}
      >
        <span aria-hidden="true" className="asm-ccs-event-line" />
        <span className="asm-ccs-event-label">{inner}</span>
        <span aria-hidden="true" className="asm-ccs-event-line" />
      </div>
      {detailBox && <div className="mx-auto max-w-[640px]">{detailBox}</div>}
    </div>
  );
});

// ── 注入上下文行（AGENTS.md / environment_context，菜单开启时展示） ──────────

export const ReaderInjectedRow = memo(function ReaderInjectedRow({
  message,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
}: {
  message: SessionMessage;
  highlightTerms?: string[];
}) {
  const [open, setOpen] = useState(false);
  const trimmed = message.content.trim();
  const agentsMatch = /^#\s*(AGENTS\.md|CLAUDE\.md|GEMINI\.md)\b/.exec(trimmed);
  const tagMatch = /^<([a-zA-Z][\w-]*)/.exec(trimmed);
  const label =
    agentsMatch?.[1] ??
    tagMatch?.[1] ??
    (message.role !== "user" ? message.role : trimmed.slice(0, 28) || "上下文");
  const chars = (
    message.contentChars ?? message.content.length
  ).toLocaleString();

  return (
    <div className="min-w-0 px-1.5 py-0.5">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((prev) => !prev)}
        className="asm-ccs-injected-btn"
      >
        <ChevronRight
          aria-hidden="true"
          className={cn(
            "size-3.5 shrink-0 transition-transform",
            open && "rotate-90",
          )}
        />
        <span className="truncate">
          上下文 · {label} · {chars} 字
        </span>
      </button>
      {open && (
        <pre className="asm-ccs-tool-pre mt-1">
          {highlightText(message.content, highlightTerms)}
        </pre>
      )}
    </div>
  );
});

// ── 思维链 / 思考过程行（1:1 复刻 cc-switch SessionThinkingRow） ─────────────
// - 各 Agent 原生标签（✻ 思考中… / • 推理 / ✦ 想法 / Thought / Thinking… / • 思考）
// - 有摘要（Codex summary_text / Gemini 粗体主题）时同行展示摘要；无摘要时仅展示「· 1.2k 字 · 8s」，干净不杂乱
// - 加密/隐藏思考（redacted_thinking / encrypted reasoning）静态展示「思考内容不可见」，不可点开
// - 点开后为次级斜体文本区（whitespace-pre-wrap italic），支持全文懒加载与「收起」

function ThinkingStepView({
  step,
  style,
  expanded,
  onToggle,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onLoadFullContent,
}: {
  step: ThinkingStep;
  style: AgentReaderStyle;
  expanded: boolean;
  onToggle: () => void;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const { t } = useTranslation();
  const {
    loaded,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  } = useLazyFullMessage(step.message, onLoadFullContent);

  const label = style.thinkingLabel;
  const summary = step.summary ? stripBold(step.summary) : "";
  const duration =
    step.durationMs !== undefined && step.durationMs > 0
      ? formatDurationCompact(step.durationMs)
      : null;

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    if (
      normalizedTerms.every((term) => loaded.toLocaleLowerCase().includes(term))
    ) {
      return;
    }
    void loadFullContent().catch(() => undefined);
  }, [
    isActiveSearchMatch,
    isUnloadedTruncated,
    loadFullContent,
    loaded,
    normalizedTerms,
  ]);

  const fullBodyText = useMemo(() => {
    if (loaded && loaded !== step.message.content) {
      return loaded;
    }
    return step.text || step.summary || loaded;
  }, [loaded, step.message.content, step.summary, step.text]);

  if (
    step.redacted ||
    (!step.text && !step.summary && !step.message.truncated)
  ) {
    return (
      <div className="asm-ccs-row-static">
        <GlyphCell
          glyph={style.thinkingGlyph}
          className="text-muted-foreground"
        />
        <span className="shrink-0 italic text-foreground/75">{label}</span>
        <span className="min-w-0 truncate text-muted-foreground">
          {summary || "思考内容不可见"}
        </span>
        {duration && (
          <span className="ml-auto shrink-0 tabular-nums text-muted-foreground">
            {duration}
          </span>
        )}
      </div>
    );
  }

  const totalChars =
    step.message.contentChars ??
    Math.max(fullBodyText.length, step.message.content.length);
  const charsText = `${formatCompactChars(totalChars)} 字`;
  const metaText = duration ? `${charsText} · ${duration}` : charsText;
  const regionId = `reader-thinking-${step.id}`;

  const handleToggle = () => {
    if (expanded) {
      onToggle();
      return;
    }
    onToggle();
    if (isUnloadedTruncated) {
      void loadFullContent().catch(() => undefined);
    }
  };

  return (
    <div className="min-w-0">
      <button
        type="button"
        aria-expanded={expanded}
        aria-controls={expanded ? regionId : undefined}
        aria-label={
          expanded
            ? t("sessionManager.collapseThinking", {
                defaultValue: "收起思考过程",
              })
            : t("sessionManager.expandThinking", {
                defaultValue: "展开思考过程",
              })
        }
        onClick={handleToggle}
        className="asm-ccs-row-btn group/row"
      >
        <GlyphCell
          glyph={style.thinkingGlyph}
          className="text-muted-foreground"
        />
        <span className="shrink-0 italic text-foreground/75">{label}</span>
        {summary && (
          <span className="min-w-0 truncate text-foreground/78" title={summary}>
            {highlightText(summary, highlightTerms)}
          </span>
        )}
        <span
          className={cn(
            "shrink-0 tabular-nums text-muted-foreground",
            summary ? "ml-auto" : "",
          )}
        >
          {isLoadingFull
            ? t("sessionManager.loadingFullContent", {
                defaultValue: "正在读取完整内容…",
              })
            : summary
              ? metaText
              : `· ${metaText}`}
        </span>
        <ChevronRight
          aria-hidden="true"
          className={cn(
            "asm-ccs-chevron is-hover-only",
            !summary && "ml-auto",
            expanded && "is-rotated is-forced-visible",
          )}
        />
      </button>
      {expanded && (
        <div
          id={regionId}
          role="region"
          aria-label={label}
          className="asm-ccs-thinking-region"
        >
          <pre className="asm-ccs-thinking-pre">
            {highlightText(fullBodyText, highlightTerms)}
            {isUnloadedTruncated && "…"}
          </pre>
          <div className="mt-1 flex items-center gap-3 text-xs text-muted-foreground">
            {isUnloadedTruncated && (
              <button
                type="button"
                onClick={() => {
                  void loadFullContent()
                    .then(() => {
                      if (!mountedRef.current) return;
                    })
                    .catch(() => undefined);
                }}
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
            <button
              type="button"
              onClick={onToggle}
              className="asm-ccs-subtle-link"
            >
              {t("sessionManager.collapseContent", { defaultValue: "收起" })}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ── 中间过程说明行（1:1 复刻 cc-switch SessionNoteRow：◦ 符号 + 3 行渐隐折叠） ──

function NoteStepView({
  step,
  expanded,
  onToggle,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
}: {
  step: NoteStep;
  expanded: boolean;
  onToggle: () => void;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
}) {
  const isLong = step.text.length > 240 || step.text.split("\n").length > 3;
  const clamped = isLong && !expanded;
  const regionId = `reader-note-${step.id}`;

  return (
    <div className="asm-ccs-note-row">
      <GlyphCell glyph="◦" className="mt-1 text-muted-foreground" />
      <div className="min-w-0 flex-1">
        <span className="sr-only">说明：</span>
        <div
          id={regionId}
          className={cn("asm-ccs-note-body", clamped && "is-clamped")}
        >
          <MessageMarkdown
            content={step.text}
            highlightTerms={highlightTerms}
            onCopyCode={(code) => (onCopyCode ?? onCopy)(code)}
            onOpenUrl={onOpenUrl}
          />
        </div>
        {isLong && (
          <button
            type="button"
            aria-expanded={expanded}
            aria-controls={regionId}
            onClick={onToggle}
            className="asm-ccs-subtle-link mt-0.5"
          >
            {expanded ? "收起" : "展开说明"}
          </button>
        )}
      </div>
    </div>
  );
}

// ── 工具步骤与合并步骤行（1:1 复刻 cc-switch SessionStep + SessionStepDetails） ──

const OPENCODE_KIND_ICONS: Record<ToolActionKind, string> = {
  shell: "$",
  read: "→",
  search: "✱",
  edit: "←",
  write: "←",
  web: "%",
  mcp: "⚙",
  agent: "│",
  ask: "→",
  todo: "⚙",
  other: "⚙",
};

function formatToolDisplayTitle(
  step: ToolStep | MergedStep,
  style: AgentReaderStyle,
  projectDir?: string | null,
  expanded = false,
): {
  icon: string;
  verb: string;
  target: string;
  detail?: string;
} {
  if (step.kind === "merged") {
    if (step.mergeKind === "explored") {
      const reads = new Set(
        step.children
          .filter((c) => c.call?.kind === "read" && c.call.title)
          .map((c) => c.call!.title),
      ).size;
      const searches = step.children.filter(
        (c) => c.call?.kind === "search",
      ).length;
      const parts: string[] = [];
      if (reads > 0) parts.push(`查看了 ${reads} 个文件`);
      if (searches > 0) parts.push(`搜索 ${searches} 次`);
      return {
        icon: "",
        verb: "Explored",
        target: "",
        detail: parts.join("、") || `${step.children.length} 步`,
      };
    }
    return {
      icon: "",
      verb: `Called ${step.server ?? "MCP"} ${step.children.length} times`,
      target: "",
    };
  }

  if (!step.call) {
    const rawOut = step.result?.output ?? step.sourceMessage.content ?? "";
    const firstNonEmpty =
      rawOut
        .split(/\r?\n/)
        .find((l) => l.trim().length > 0)
        ?.trim() ?? "";
    return {
      icon: "",
      verb: "工具输出",
      target:
        !expanded && firstNonEmpty
          ? firstNonEmpty.length > 72
            ? `${firstNonEmpty.slice(0, 72)}…`
            : firstNonEmpty
          : "",
    };
  }

  const { call } = step;
  const pathKinds: ReadonlySet<ToolActionKind> = new Set([
    "read",
    "search",
    "edit",
    "write",
  ]);
  let verb = style.verbs[call.kind] ?? call.rawName;
  if (style.titleFormat === "icon") {
    // OpenCode 动词已拆成符号 + 词，去掉前置符号以免重复
    verb = verb.replace(/^[$→✱←%⚙│]\s*/, "");
  }
  let target = pathKinds.has(call.kind)
    ? shortenProjectPath(call.title, projectDir)
    : call.title;
  let detail = call.detail
    ? shortenProjectPath(call.detail, projectDir)
    : undefined;
  if (detail === ".") detail = undefined;

  if (call.kind === "read" && detail?.startsWith(":")) {
    target += detail;
    detail = undefined;
  }

  if (call.kind === "mcp" && style.titleFormat !== "icon" && call.title) {
    if (style.titleFormat === "call") {
      verb = call.title;
      target = detail ?? "";
    } else {
      target = detail ? `${call.title}(${detail})` : call.title;
    }
    detail = undefined;
  }

  if (target === verb) target = "";
  const icon =
    style.titleFormat === "icon" ? OPENCODE_KIND_ICONS[call.kind] : "";

  return { icon, verb, target, detail };
}

function CollapsibleDetailSection({
  label,
  defaultOpen = false,
  children,
}: {
  label: string;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="min-w-0">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((prev) => !prev)}
        className="asm-ccs-subsec-btn"
      >
        <ChevronRight
          aria-hidden="true"
          className={cn("size-3.5 transition-transform", open && "rotate-90")}
        />
        <span>{label}</span>
      </button>
      {open && children}
    </section>
  );
}

function ToolStepView({
  step,
  style,
  projectDir,
  expanded,
  previewLines,
  onToggle,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onLoadFullContent,
}: {
  step: ToolStep | MergedStep;
  style: AgentReaderStyle;
  projectDir?: string | null;
  expanded: boolean;
  previewLines: number;
  onToggle: () => void;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const { t } = useTranslation();
  const [outputShowAll, setOutputShowAll] = useState(false);

  const lazyTargetMsg =
    step.kind === "tool"
      ? (step.result?.message ?? step.sourceMessage)
      : undefined;
  const {
    loaded,
    isTruncated,
    isUnloadedTruncated,
    isLoadingFull,
    loadFullContent,
    mountedRef,
  } = useLazyFullMessage(lazyTargetMsg, onLoadFullContent);

  const normalizedTerms = useMemo(
    () =>
      highlightTerms
        .map((term) => term.trim().toLocaleLowerCase())
        .filter(Boolean),
    [highlightTerms],
  );

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    if (
      normalizedTerms.every((term) => loaded.toLocaleLowerCase().includes(term))
    ) {
      return;
    }
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setOutputShowAll(true);
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

  const titleInfo = useMemo(
    () => formatToolDisplayTitle(step, style, projectDir, expanded),
    [expanded, projectDir, step, style],
  );

  const status: ToolRunStatus = step.status;
  const glyph =
    status === "unknown"
      ? style.stepGlyph.success
      : style.stepGlyph[status] || style.stepGlyph.success;
  const isFailure = status === "error" || status === "interrupted";

  const diffInfo = useMemo(() => {
    if (step.kind !== "tool") return null;
    if (step.call?.diffText) {
      return {
        diffText: step.call.diffText,
        added: step.call.diffAdded ?? 0,
        removed: step.call.diffRemoved ?? 0,
      };
    }
    const fromOut = step.result?.output
      ? extractDiffPayload(step.result.output)
      : null;
    if (fromOut) return fromOut;
    return extractDiffPayload(step.sourceMessage.content);
  }, [step]);

  const exitCode = step.kind === "tool" ? step.result?.exitCode : undefined;
  const durationMs =
    step.kind === "merged"
      ? step.children.reduce((s, c) => s + (c.result?.durationMs ?? 0), 0) ||
        undefined
      : step.result?.durationMs;

  const rawOutputText = useMemo(() => {
    if (step.kind !== "tool") return "";
    if (loaded && lazyTargetMsg && loaded !== lazyTargetMsg.content) {
      return loaded;
    }
    return step.result?.output ?? "";
  }, [lazyTargetMsg, loaded, step]);

  const outputLines = useMemo(
    () =>
      rawOutputText ? rawOutputText.replace(/\r\n/g, "\n").split("\n") : [],
    [rawOutputText],
  );
  const lineCount =
    step.kind === "tool"
      ? Math.max(step.result?.lineCount ?? 0, outputLines.length)
      : 0;

  const fullShellCmd =
    step.kind === "tool" ? extractFullShellCommand(step.call) : null;
  const showFullShellCmd = Boolean(
    step.kind === "tool" &&
      fullShellCmd &&
      step.call &&
      fullShellCmd.trim() !== step.call.title.trim() &&
      fullShellCmd.includes("\n"),
  );

  const hasParamsSection = Boolean(
    step.kind === "tool" &&
      step.call?.inputPreview &&
      !diffInfo &&
      (step.call.kind !== "shell" || !fullShellCmd || step.call.inputTruncated),
  );

  const hasExpandableContent =
    step.kind === "merged" ||
    Boolean(diffInfo) ||
    Boolean(rawOutputText) ||
    hasParamsSection ||
    showFullShellCmd ||
    isTruncated;

  const effectiveLimit = outputShowAll ? Number.MAX_SAFE_INTEGER : previewLines;
  const hiddenLines =
    !diffInfo && outputLines.length > effectiveLimit
      ? outputLines.length - effectiveLimit
      : 0;
  const displayedOutput =
    hiddenLines > 0
      ? outputLines.slice(0, effectiveLimit).join("\n")
      : rawOutputText;

  const totalChars =
    lazyTargetMsg?.contentChars ??
    Math.max(rawOutputText.length, lazyTargetMsg?.content.length ?? 0);

  const statusBadgeText =
    status === "error" ? "失败" : status === "interrupted" ? "已中断" : null;

  return (
    <div className="min-w-0">
      <button
        type="button"
        aria-expanded={hasExpandableContent ? expanded : undefined}
        onClick={() => {
          if (hasExpandableContent) onToggle();
        }}
        className={cn(
          "asm-ccs-row-btn group/row",
          !hasExpandableContent && "is-static",
        )}
      >
        <GlyphCell
          glyph={glyph}
          className={cn(
            status === "error"
              ? "asm-ccs-tone-danger"
              : status === "interrupted"
                ? "asm-ccs-tone-warning"
                : status === "pending"
                  ? "text-muted-foreground"
                  : "asm-ccs-tone-success",
          )}
        />
        <span
          className={cn(
            "min-w-0 truncate",
            style.monoTitles ? "font-mono text-[12.5px]" : "text-[13px]",
          )}
        >
          {titleInfo.icon && (
            <span className="mr-1.5 text-muted-foreground">
              {titleInfo.icon}
            </span>
          )}
          <span className="font-semibold text-foreground/90">
            {titleInfo.verb}
          </span>
          {titleInfo.target &&
            (style.titleFormat === "call" &&
            step.kind === "tool" &&
            Boolean(step.call) ? (
              <>
                <span className="text-muted-foreground">(</span>
                <span className="text-foreground/85">
                  {highlightText(titleInfo.target, highlightTerms)}
                </span>
                <span className="text-muted-foreground">)</span>
              </>
            ) : (
              <>
                {" "}
                <span
                  className={cn(
                    "text-foreground/85",
                    !style.monoTitles && "font-mono text-[12px]",
                  )}
                >
                  {highlightText(titleInfo.target, highlightTerms)}
                </span>
              </>
            ))}
        </span>
        {titleInfo.detail && (
          <span className="hidden min-w-0 max-w-[38%] shrink truncate text-muted-foreground sm:inline">
            {titleInfo.detail}
          </span>
        )}
        <span className="asm-ccs-step-meta">
          {isFailure && statusBadgeText && (
            <span
              className={cn(
                "font-medium",
                status === "error"
                  ? "asm-ccs-tone-danger"
                  : "asm-ccs-tone-warning",
              )}
            >
              {statusBadgeText}
            </span>
          )}
          {exitCode !== undefined && exitCode !== 0 && (
            <span className="asm-ccs-tone-danger">exit {exitCode}</span>
          )}
          {diffInfo && diffInfo.added + diffInfo.removed > 0 && (
            <span>
              <span className="asm-ccs-tone-success">+{diffInfo.added}</span>{" "}
              <span className="asm-ccs-tone-danger">−{diffInfo.removed}</span>
            </span>
          )}
          {durationMs !== undefined && durationMs > 0 && (
            <span>{formatDurationCompact(durationMs)}</span>
          )}
          {!diffInfo && lineCount > 1 && <span>{lineCount} 行</span>}
          {hasExpandableContent && (
            <ChevronRight
              aria-hidden="true"
              className={cn(
                "asm-ccs-chevron is-hover-only",
                expanded && "is-rotated is-forced-visible",
              )}
            />
          )}
        </span>
      </button>

      {expanded && step.kind === "tool" && hasExpandableContent && (
        <div
          className={cn(
            "asm-ccs-step-details-frame",
            style.resultFrame === "box"
              ? cn(
                  "is-frame-box",
                  status === "error"
                    ? "is-status-error"
                    : status === "interrupted"
                      ? "is-status-warning"
                      : "is-status-success",
                )
              : style.resultFrame === "none"
                ? cn(
                    "is-frame-stripe",
                    status === "error"
                      ? "is-status-error"
                      : status === "interrupted"
                        ? "is-status-warning"
                        : "is-status-success",
                  )
                : "is-frame-tree",
          )}
        >
          {showFullShellCmd && fullShellCmd && (
            <pre className="asm-ccs-tool-pre">{fullShellCmd}</pre>
          )}

          {hasParamsSection && step.call && (
            <CollapsibleDetailSection
              label="参数"
              defaultOpen={isFailure || !step.result}
            >
              <pre className="asm-ccs-tool-pre">
                {prettyJson(step.call.inputPreview)}
                {step.call.inputTruncated && "\n…"}
              </pre>
            </CollapsibleDetailSection>
          )}

          {diffInfo && (
            <div className="min-w-0">
              <div className="asm-ccs-subsec-title">改动</div>
              <SessionDiffView
                diffText={diffInfo.diffText}
                projectDir={projectDir}
                highlightTerms={highlightTerms}
              />
            </div>
          )}

          {!diffInfo && (displayedOutput || isTruncated) && (
            <div className="flex min-w-0 items-start gap-2">
              {style.resultFrame === "tree" && style.resultConnector && (
                <span className="asm-ccs-connector" aria-hidden="true">
                  {style.resultConnector}
                </span>
              )}
              <div className="min-w-0 flex-1">
                <pre
                  className={cn("asm-ccs-tool-pre", isFailure && "is-error")}
                >
                  {highlightText(displayedOutput, highlightTerms)}
                  {isUnloadedTruncated && "…"}
                </pre>
                {(hiddenLines > 0 || outputShowAll || isUnloadedTruncated) && (
                  <div className="mt-1 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs text-muted-foreground">
                    {hiddenLines > 0 && !outputShowAll && (
                      <span className="tabular-nums">
                        …还有 {hiddenLines} 行
                      </span>
                    )}
                    {hiddenLines > 0 && !outputShowAll && (
                      <button
                        type="button"
                        onClick={() => setOutputShowAll(true)}
                        className="asm-ccs-subtle-link"
                      >
                        显示全部
                      </button>
                    )}
                    {outputShowAll && outputLines.length > previewLines && (
                      <button
                        type="button"
                        onClick={() => setOutputShowAll(false)}
                        className="asm-ccs-subtle-link"
                      >
                        {t("sessionManager.collapseContent", {
                          defaultValue: "收起",
                        })}
                      </button>
                    )}
                    {isUnloadedTruncated && (
                      <button
                        type="button"
                        aria-expanded={false}
                        onClick={() => {
                          void loadFullContent()
                            .then(() => {
                              if (mountedRef.current) setOutputShowAll(true);
                            })
                            .catch(() => undefined);
                        }}
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
      )}
    </div>
  );
}

// ── 统一过程步骤分派入口（Thinking / Note / Tool / Merged / Inline Event） ──

export const ReaderStepRow = memo(function ReaderStepRow({
  step,
  messageRevision,
  style,
  projectDir,
  depth = 0,
  inTimeline = false,
  expanded,
  previewLines,
  onToggleStep,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
  onLoadFullContent,
}: {
  step: TimelineStep;
  messageRevision?: string | number;
  style: AgentReaderStyle;
  projectDir?: string | null;
  depth?: 0 | 1;
  inTimeline?: boolean;
  expanded: boolean;
  previewLines: number;
  onToggleStep: (stepId: string, currentlyExpanded: boolean) => void;
  isActiveSearchMatch?: boolean;
  highlightTerms?: string[];
  onCopy: (content: string) => void;
  onCopyCode?: (code: string) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
  onLoadFullContent?: (message: SessionMessage) => Promise<string>;
}) {
  const handleToggle = useCallback(
    () => onToggleStep(step.id, expanded),
    [expanded, onToggleStep, step.id],
  );

  const stepIdentity = useMemo(() => {
    if (step.kind === "thinking") {
      return JSON.stringify([
        messageRevision,
        step.id,
        step.message.content,
        step.message.index,
        step.message.truncated,
        step.message.contentChars,
        step.message.contentIdentity,
      ]);
    }
    if (step.kind === "tool") {
      const target = step.result?.message ?? step.sourceMessage;
      return JSON.stringify([
        messageRevision,
        step.id,
        target.content,
        target.index,
        target.truncated,
        target.contentChars,
        target.contentIdentity,
      ]);
    }
    return `${messageRevision ?? ""}:${step.id}`;
  }, [messageRevision, step]);

  return (
    <div
      className={cn(
        "min-w-0",
        inTimeline && "asm-ccs-in-timeline",
        depth === 1 && "asm-ccs-child-step",
      )}
    >
      {step.kind === "thinking" ? (
        <ThinkingStepView
          key={stepIdentity}
          step={step}
          style={style}
          expanded={expanded}
          onToggle={handleToggle}
          isActiveSearchMatch={isActiveSearchMatch}
          highlightTerms={highlightTerms}
          onLoadFullContent={onLoadFullContent}
        />
      ) : step.kind === "note" ? (
        <NoteStepView
          key={stepIdentity}
          step={step}
          expanded={expanded}
          onToggle={handleToggle}
          highlightTerms={highlightTerms}
          onCopy={onCopy}
          onCopyCode={onCopyCode}
          onOpenUrl={onOpenUrl}
        />
      ) : step.kind === "event" ? (
        <ReaderEventRow
          key={stepIdentity}
          event={step.event}
          variant="inline"
          highlightTerms={highlightTerms}
          onOpenUrl={onOpenUrl}
        />
      ) : (
        <ToolStepView
          key={stepIdentity}
          step={step}
          style={style}
          projectDir={projectDir}
          expanded={expanded}
          previewLines={previewLines}
          onToggle={handleToggle}
          isActiveSearchMatch={isActiveSearchMatch}
          highlightTerms={highlightTerms}
          onLoadFullContent={onLoadFullContent}
        />
      )}
    </div>
  );
});

// ── 最终回复行（无底色、无边框文档流，右上悬停操作） ─────────────────────

export const ReaderFinalReplyRow = memo(function ReaderFinalReplyRow(props: {
  message: SessionMessage;
  overrideText?: string;
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
    props.overrideText ?? "",
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
  overrideText,
  isActiveSearchMatch = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopy,
  onCopyCode,
  onOpenUrl,
  onLoadFullContent,
}: {
  message: SessionMessage;
  overrideText?: string;
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

  const effectiveText =
    overrideText && loaded === message.content ? overrideText : loaded;
  const isLong = isTruncated || effectiveText.length > COLLAPSE_THRESHOLD;
  const collapsed = isLong && !expanded;
  const totalChars = message.contentChars ?? effectiveText.length;
  const displaySlice = collapsed
    ? effectiveText.slice(0, COLLAPSED_LENGTH)
    : effectiveText;
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
      effectiveText.length > COLLAPSE_THRESHOLD &&
      normalizedTerms.some((term) =>
        effectiveText
          .slice(COLLAPSED_LENGTH)
          .toLocaleLowerCase()
          .includes(term),
      )
    ) {
      setExpanded(true);
    }
  }, [effectiveText, normalizedTerms]);

  useEffect(() => {
    if (
      !isActiveSearchMatch ||
      !isUnloadedTruncated ||
      normalizedTerms.length === 0
    ) {
      return;
    }
    const lower = effectiveText.toLocaleLowerCase();
    if (normalizedTerms.every((term) => lower.includes(term))) return;
    void loadFullContent()
      .then(() => {
        if (mountedRef.current) setExpanded(true);
      })
      .catch(() => undefined);
  }, [
    effectiveText,
    isActiveSearchMatch,
    isUnloadedTruncated,
    loadFullContent,
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
      onCopy(effectiveText);
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
    effectiveText,
    isCopying,
    isUnloadedTruncated,
    loadFullContent,
    mountedRef,
    onCopy,
  ]);

  const handleCodeCopy = useCallback(
    (code: string, blockIdx: number): Promise<void> | void => {
      const target = onCopyCode ?? onCopy;
      if (!collapsed) return target(code);
      if (!isUnloadedTruncated) {
        return target(extractFullCodeBlock(effectiveText, blockIdx, code));
      }
      return loadFullContent().then((full) => {
        if (!mountedRef.current) return;
        return target(extractFullCodeBlock(full, blockIdx, code));
      });
    },
    [
      collapsed,
      effectiveText,
      isUnloadedTruncated,
      loadFullContent,
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
