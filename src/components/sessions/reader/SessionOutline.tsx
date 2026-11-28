import { memo, useEffect, useMemo, useRef } from "react";
import { Bookmark, X } from "lucide-react";
import { useTranslation } from "react-i18next";

import { OverlayScrollArea } from "@/components/ui/overlay-scroll-area";
import { cn } from "@/lib/utils";
import { formatMessageTime } from "../utils";
import { useReaderT } from "./i18n";
import { SessionAgentAvatar } from "./SessionAgentAvatar";
import { isFailureStep } from "./toolSummary";
import type { SessionTurn, TurnChapter } from "./turns";

/** 目录里每条最多显示的字数：再多也只是两行截断，没必要整段传给 DOM */
const SNIPPET_CHARS = 160;

/** 去掉 Markdown 记号，只留可读文字作目录摘要 */
const toSnippet = (text: string) =>
  text
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    // 只去掉 Markdown 记号；_ 和 - 常在标识符里（tokio_util、foo-bar），保留
    .replace(/[#>*`~|]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, SNIPPET_CHARS);

interface OutlineEntry {
  turn: SessionTurn;
  question?: string;
  reply?: string;
  steps: number;
  failed: boolean;
}

export interface SessionOutlineProps {
  turns: SessionTurn[];
  /** 正文当前停在哪一轮（turns 下标），高亮并滚到可见 */
  activeTurn: number;
  onJumpQuestion: (turn: SessionTurn) => void;
  onJumpReply: (turn: SessionTurn) => void;
  onJumpChapter: (turn: SessionTurn, chapter: TurnChapter) => void;
  onClose?: () => void;
}

/**
 * 右侧对话目录：每轮拆成「你」和 Agent 两条，和正文一样人靠右、Agent 靠左，
 * 一眼分清谁说了什么；点哪条跳到正文对应位置，滚动正文时跟着高亮当前轮。
 * 窄窗口下自动转为右侧浮层抽屉，并提供右上角关闭按钮。
 */
export const SessionOutline = memo(function SessionOutline({
  turns,
  activeTurn,
  onJumpQuestion,
  onJumpReply,
  onJumpChapter,
  onClose,
}: SessionOutlineProps) {
  const { t } = useTranslation();
  const rt = useReaderT();
  const listRef = useRef<HTMLOListElement>(null);

  const entries = useMemo<OutlineEntry[]>(
    () =>
      turns
        .filter(
          (turn) =>
            turn.question ||
            turn.final ||
            turn.steps.length > 0 ||
            turn.chapters.length > 0 ||
            turn.aborted,
        )
        .map((turn) => ({
          turn,
          question: turn.question ? toSnippet(turn.question.text) : undefined,
          reply: turn.final ? toSnippet(turn.final.text) : undefined,
          steps: turn.steps.length,
          failed: turn.aborted || turn.steps.some(isFailureStep),
        })),
    [turns],
  );

  // 正文滚到别的轮时，把目录里对应的条目滚进视野；只滚目录滚动视口自己，不带动外层容器
  useEffect(() => {
    const list = listRef.current;
    const item = list?.querySelector<HTMLElement>(
      `[data-outline-turn="${activeTurn}"]`,
    );
    const viewport = list?.closest<HTMLElement>(".asm-overlay-scroll-viewport");
    if (!viewport || !item) return;
    const viewportRect = viewport.getBoundingClientRect();
    const itemRect = item.getBoundingClientRect();
    if (itemRect.top < viewportRect.top + 6) {
      viewport.scrollTop -= viewportRect.top + 6 - itemRect.top;
    } else if (itemRect.bottom > viewportRect.bottom - 6) {
      viewport.scrollTop += itemRect.bottom - (viewportRect.bottom - 6);
    }
  }, [activeTurn]);

  return (
    <nav aria-label={rt("outlineTitle")} className="asm-ccs-outline">
      <div className="flex h-9 shrink-0 items-center justify-between border-b border-border/60 pe-2 ps-3.5 text-caption font-semibold text-fg-2">
        <span className="flex items-center">
          {rt("outlineTitle")}
          <span className="ms-1.5 rounded-full bg-subtle px-1.5 py-px text-[11px] font-medium tabular-nums text-fg-3">
            {entries.length}
          </span>
        </span>
        {onClose && (
          <button
            type="button"
            aria-label={t("sessionManager.closeOutline", {
              defaultValue: "关闭对话目录",
            })}
            onClick={onClose}
            className="inline-flex h-6 w-6 items-center justify-center rounded-control text-fg-3 transition-colors hover:bg-subtle hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            <X className="h-3.5 w-3.5" aria-hidden="true" />
          </button>
        )}
      </div>
      <OverlayScrollArea
        wrapperClassName="min-h-0 flex-1"
        className="px-2 py-2 pb-4"
      >
        <ol ref={listRef} className="m-0 list-none p-0">
          {entries.map((entry) => {
            const active = entry.turn.index === activeTurn;
            return (
              <li
                key={entry.turn.key}
                data-outline-turn={entry.turn.index}
                className={cn(
                  "relative mb-1 flex flex-col gap-1 rounded-panel px-1.5 py-1.5 transition-colors",
                  active && "bg-subtle",
                )}
              >
                {active && (
                  <span
                    aria-hidden
                    className="absolute inset-y-2 start-0 w-[2px] rounded-full bg-[var(--reader-accent)]"
                  />
                )}

                {entry.turn.chapters.map((chapter) => (
                  <button
                    key={chapter.id}
                    type="button"
                    onClick={() => onJumpChapter(entry.turn, chapter)}
                    title={chapter.summary}
                    className="group/outline flex min-w-0 items-center gap-1.5 rounded-control py-0.5 text-start focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <Bookmark
                      aria-hidden
                      className="h-3 w-3 shrink-0 text-[var(--reader-accent)]"
                      strokeWidth={2}
                    />
                    <span className="sr-only">{rt("chapter")}：</span>
                    <span className="min-w-0 truncate text-caption font-semibold text-fg-1 group-hover/outline:underline">
                      {chapter.title}
                    </span>
                  </button>
                ))}

                {entry.question !== undefined && (
                  <button
                    type="button"
                    onClick={() => onJumpQuestion(entry.turn)}
                    aria-current={active ? "true" : undefined}
                    className="group/outline flex min-w-0 flex-col items-end gap-0.5 self-end rounded-control text-end focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <span className="text-[11px] tabular-nums text-fg-3">
                      {t("sessionManager.you", { defaultValue: "你" })}
                      {entry.turn.question?.ts
                        ? ` · ${formatMessageTime(entry.turn.question.ts)}`
                        : ""}
                    </span>
                    <span
                      className="line-clamp-2 max-w-[200px] break-words rounded-[10px] rounded-se-[3px] px-2 py-1 text-start text-caption text-fg-1 transition-[filter] group-hover/outline:brightness-110"
                      style={{
                        background:
                          "color-mix(in srgb, var(--reader-accent) 14%, var(--bg-subtle))",
                      }}
                    >
                      {entry.question || rt("outlineEmptyQuestion")}
                    </span>
                  </button>
                )}

                <button
                  type="button"
                  onClick={() => onJumpReply(entry.turn)}
                  className="group/outline flex min-w-0 items-start gap-1.5 rounded-control text-start focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  <span className="mt-0.5 origin-top-left scale-[0.72]">
                    <SessionAgentAvatar />
                  </span>
                  <span className="-ms-1.5 flex min-w-0 flex-1 flex-col gap-0.5">
                    <span className="flex items-center gap-1 text-[11px] tabular-nums text-fg-3">
                      {entry.steps > 0 &&
                        rt("outlineSteps", { count: entry.steps })}
                      {entry.failed && (
                        <span
                          aria-label={rt("outlineFailed")}
                          className="h-1.5 w-1.5 rounded-full bg-danger"
                        />
                      )}
                    </span>
                    <span
                      className={cn(
                        "line-clamp-2 break-words text-caption transition-colors group-hover/outline:text-fg-1",
                        entry.reply ? "text-fg-2" : "italic text-fg-3",
                      )}
                    >
                      {entry.reply ||
                        (entry.turn.aborted
                          ? rt("outlineAborted")
                          : rt("outlineNoReply"))}
                    </span>
                  </span>
                </button>
              </li>
            );
          })}
        </ol>
      </OverlayScrollArea>
    </nav>
  );
});
