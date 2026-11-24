import { memo, useMemo } from "react";
import { ChevronRight } from "lucide-react";

import { cn } from "@/lib/utils";
import { highlightQueryText as highlightText } from "../utils";
import { useReaderContext } from "./context";
import { useReaderT } from "./i18n";
import { GlyphCell, ROW_BUTTON } from "./rowStyles";
import { SessionMarkdown } from "./SessionMarkdown";
import type { NoteStep } from "./turns";

/** 提取单行可读摘要，去掉围栏与多余空白 */
const toNoteSummary = (text: string): string => {
  const flat = text
    .replace(/```[\s\S]*?```/g, " [code] ")
    .replace(/[#>*`~]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return flat || text.trim();
};

export interface SessionNoteRowProps {
  step: NoteStep;
  expanded: boolean;
  onToggle: (id: string, expanded: boolean) => void;
}

/**
 * 过程中的助手说明：时间线内默认占 32px 单行，点击展开完整 SessionMarkdown。
 * 搜索命中落在该说明上时由 flattenRows 自动将 expanded 设为 true。
 */
export const SessionNoteRow = memo(function SessionNoteRow({
  step,
  expanded,
  onToggle,
}: SessionNoteRowProps) {
  const rt = useReaderT();
  const { projectDir, searchQuery } = useReaderContext();
  const summary = useMemo(() => toNoteSummary(step.text), [step.text]);
  const regionId = `reader-note-${step.id}`;

  return (
    <div className="min-w-0">
      <button
        type="button"
        aria-expanded={expanded}
        aria-controls={expanded ? regionId : undefined}
        aria-label={expanded ? rt("collapseContent") : rt("noteExpand")}
        title={summary}
        onClick={() => onToggle(step.id, expanded)}
        className={ROW_BUTTON}
      >
        <GlyphCell glyph="◦" className="text-fg-3" />
        <span className="sr-only">{rt("note")}：</span>
        <span className="min-w-0 flex-1 truncate text-fg-2">
          {searchQuery ? highlightText(summary, searchQuery) : summary}
        </span>
        <ChevronRight
          aria-hidden
          className={cn(
            "ms-auto h-3.5 w-3.5 shrink-0 text-fg-3 opacity-0 transition-[opacity,transform] group-hover/row:opacity-100 group-focus-visible/row:opacity-100 motion-reduce:transition-none",
            expanded && "rotate-90 opacity-100",
          )}
        />
      </button>
      {expanded && (
        <div
          id={regionId}
          role="region"
          aria-label={rt("note")}
          className="pb-2 pe-1.5 ps-6 pt-0.5"
        >
          <SessionMarkdown
            content={step.text}
            variant="note"
            searchQuery={searchQuery}
            projectDir={projectDir}
            className="[&_p]:my-1"
          />
        </div>
      )}
    </div>
  );
});
