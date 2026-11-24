import { memo, useCallback, useMemo, useState } from "react";
import { ClipboardList, Copy } from "lucide-react";
import { useTranslation } from "react-i18next";

import { HoverTip } from "@/components/ui/hover-tip";
import { useMoneyFormat } from "@/hooks/useMoneyFormat";
import { cn } from "@/lib/utils";
import type { SessionTextRange } from "@/types";
import { findTextMatches, highlightTextRanges } from "../utils";
import { useReaderContext } from "./context";
import { useReaderT } from "./i18n";
import { SessionImage, SessionImageGrid } from "./SessionImage";
import {
  createCollapsedMarkdownPreview,
  hasHighlightableMarkdownMatch,
  SessionMarkdown,
} from "./SessionMarkdown";
import {
  COLLAPSE_THRESHOLD,
  COLLAPSED_LENGTH,
  CollapseToggle,
  rowIconButton,
} from "./SessionQuestion";
import { formatDuration, formatTokens } from "./toolSummary";
import type { TurnFinal } from "./turns";

const SNIPPET_RADIUS = 60;

/** 截取并压缩空白，把完整原文的命中范围映射到最终摘录的 UTF-16 坐标 */
const createHighlightedSnippet = (
  text: string,
  matches: readonly SessionTextRange[],
  start = 0,
  end = text.length,
): { text: string; highlights: SessionTextRange[] } => {
  const source = text.slice(start, end);
  const starts: number[] = [];
  const ends: number[] = [];
  let snippet = start > 0 ? "…" : "";
  for (const part of source.matchAll(/\s+|\S+/g)) {
    const value = part[0];
    const index = part.index ?? 0;
    const whitespace = /^\s/.test(value);
    const offset = snippet.length;
    snippet += whitespace ? " " : value;
    for (let i = 0; i < value.length; i += 1) {
      starts[index + i] = offset + (whitespace ? 0 : i);
      ends[index + i] = offset + (whitespace ? 1 : i + 1);
    }
  }
  const highlights = matches.flatMap((match) => {
    const left = Math.max(start, match.start);
    const right = Math.min(end, match.end);
    return left < right
      ? [{ start: starts[left - start], end: ends[right - start - 1] }]
      : [];
  });
  return {
    text: snippet + (end < text.length ? "…" : ""),
    highlights,
  };
};

/** 原文中命中处前后各 60 字（命中藏在链接地址等不可见位置时兜底展示） */
const rawSnippets = (text: string, query: string, limit = 3) => {
  const snippets: ReturnType<typeof createHighlightedSnippet>[] = [];
  const matches = findTextMatches(text, query);
  let next = matches.next();
  let highlights: SessionTextRange[] = [];
  while (!next.done && snippets.length < limit) {
    const match = next.value;
    let start = Math.max(0, match.start - SNIPPET_RADIUS);
    let end = Math.min(text.length, match.end + SNIPPET_RADIUS);
    if (start > 0 && text.codePointAt(start - 1)! > 0xffff) start -= 1;
    if (end < text.length && text.codePointAt(end - 1)! > 0xffff) end += 1;
    highlights = highlights.filter((range) => range.end > start);
    while (!next.done && next.value.start < end) {
      highlights.push(next.value);
      next = matches.next();
    }
    snippets.push(createHighlightedSnippet(text, highlights, start, end));
  }
  return snippets;
};

export interface SessionFinalReplyProps {
  final: TurnFinal;
  /** 查找命中：长回复自动展开（规则 11） */
  forceExpanded: boolean;
  /** 本轮是否已有时间线摘要行；无时间线时在底部展示耗时/Token/费用 */
  hasTimeline?: boolean;
  turnKey?: string;
  onCopyTurn?: (turnKey: string) => void;
}

/**
 * 最终回复：Markdown 正文（视觉重点，无底色）、图片网格、
 * 超过 3000 字折叠到 1500 字、悬停「复制回复 / 复制这一轮」按钮，
 * 以及在没有时间线摘要时展示本轮耗时 / Token / 费用信息。
 */
export const SessionFinalReply = memo(function SessionFinalReply({
  final,
  forceExpanded,
  hasTimeline = false,
  turnKey,
  onCopyTurn,
}: SessionFinalReplyProps) {
  const { t } = useTranslation();
  const rt = useReaderT();
  const { moneyCompact } = useMoneyFormat();
  const { style, projectDir, searchQuery, onCopy } = useReaderContext();
  const [expanded, setExpanded] = useState(false);
  const text = final.text;
  const long = text.length > COLLAPSE_THRESHOLD;
  const open = expanded || forceExpanded;
  const shown =
    long && !open
      ? createCollapsedMarkdownPreview(text, COLLAPSED_LENGTH)
      : text;

  const hiddenMatches = useMemo(() => {
    const query = searchQuery?.trim();
    if (!query || !text.toLowerCase().includes(query.toLowerCase())) return [];
    if (hasHighlightableMarkdownMatch(text, query, projectDir)) return [];
    return rawSnippets(text, query);
  }, [projectDir, searchQuery, text]);

  const renderLocalImage = useCallback(
    (path: string, alt: string) => (
      <SessionImage
        image={{
          source: { kind: "local_file", path },
          mediaType: "",
          size: 0,
          alt,
        }}
        alt={alt || path}
        variant="result"
        className="my-1"
      />
    ),
    [],
  );

  // 计算底部元信息：style.showStepCost 的段费用，或无时间线摘要时的回复 meta（耗时 / tokens / costUsd）
  const footer = useMemo(() => {
    if (style.showStepCost && final.cost) {
      const hasAny =
        final.cost.tokens !== undefined || final.cost.costUsd !== undefined;
      if (hasAny) {
        return {
          durationMs: !hasTimeline ? final.durationMs : undefined,
          tokens: final.cost.tokens,
          costUsd: final.cost.costUsd,
        };
      }
    }
    if (hasTimeline) return null;
    const meta = final.meta;
    const metaTokens = meta
      ? (meta.inputTokens ?? 0) +
        (meta.outputTokens ?? 0) +
        (meta.cacheReadTokens ?? 0) +
        (meta.cacheWriteTokens ?? 0) +
        (meta.reasoningTokens ?? 0)
      : 0;
    const tokens =
      final.cost?.tokens ?? (metaTokens > 0 ? metaTokens : undefined);
    const costUsd = final.cost?.costUsd ?? meta?.costUsd;
    const durationMs =
      final.durationMs !== undefined && final.durationMs > 0
        ? final.durationMs
        : undefined;
    if (
      tokens === undefined &&
      costUsd === undefined &&
      durationMs === undefined
    ) {
      return null;
    }
    return { durationMs, tokens, costUsd };
  }, [
    final.cost,
    final.durationMs,
    final.meta,
    hasTimeline,
    style.showStepCost,
  ]);

  const footerParts: string[] = [];
  if (footer?.durationMs !== undefined && footer.durationMs > 0) {
    footerParts.push(
      rt("summaryDuration", { duration: formatDuration(footer.durationMs) }),
    );
  }
  if (footer?.tokens !== undefined && footer.tokens > 0) {
    footerParts.push(
      rt("summaryTokens", { tokens: formatTokens(footer.tokens) }),
    );
  }
  if (footer?.costUsd !== undefined && footer.costUsd > 0) {
    footerParts.push(moneyCompact(footer.costUsd));
  }

  return (
    <div className="group/final relative min-w-0 px-1.5 py-1">
      <div className="absolute end-0 top-0 flex items-center gap-0.5">
        {text && (
          <HoverTip
            content={t("sessionManager.copyReply", {
              defaultValue: "复制回复",
            })}
          >
            <button
              type="button"
              aria-label={t("sessionManager.copyReply", {
                defaultValue: "复制这条回复",
              })}
              onClick={() =>
                onCopy(
                  text,
                  t("sessionManager.messageCopied", {
                    defaultValue: "已复制这条消息",
                  }),
                )
              }
              className={cn(rowIconButton, "group-hover/final:opacity-100")}
            >
              <Copy aria-hidden className="h-3.5 w-3.5" strokeWidth={1.5} />
            </button>
          </HoverTip>
        )}
        {turnKey && onCopyTurn && (
          <HoverTip content={rt("copyTurn")}>
            <button
              type="button"
              aria-label={rt("copyTurn")}
              onClick={() => onCopyTurn(turnKey)}
              className={cn(rowIconButton, "group-hover/final:opacity-100")}
            >
              <ClipboardList
                aria-hidden
                className="h-3.5 w-3.5"
                strokeWidth={1.5}
              />
            </button>
          </HoverTip>
        )}
      </div>
      <div className={turnKey && onCopyTurn ? "pe-14" : "pe-7"}>
        {text && (
          <SessionMarkdown
            content={shown}
            searchQuery={searchQuery}
            projectDir={projectDir}
            renderLocalImage={renderLocalImage}
          />
        )}
        {long && !forceExpanded && (
          <CollapseToggle
            expanded={expanded}
            total={text.length}
            onToggle={() => setExpanded((value) => !value)}
          />
        )}
        <SessionImageGrid
          images={final.images}
          variant="result"
          className="mt-2"
        />
        {footerParts.length > 0 && (
          <div className="mt-1.5 text-caption tabular-nums text-fg-3">
            {footerParts.join(" · ")}
          </div>
        )}
        {hiddenMatches.length > 0 && searchQuery && (
          <div className="mt-2 rounded-[8px] border border-border px-3 py-2 text-caption text-fg-2">
            <div className="mb-1 font-medium text-fg-3">{rt("rawMatch")}</div>
            {hiddenMatches.map((snippet, index) => (
              <p
                key={index}
                className="m-0 break-words font-mono [overflow-wrap:anywhere]"
              >
                {highlightTextRanges(snippet.text, snippet.highlights)}
              </p>
            ))}
          </div>
        )}
      </div>
    </div>
  );
});
