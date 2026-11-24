import { memo, useMemo, useState } from "react";

import { SegmentedControl } from "@/components/ui/segmented-control";
import { useBlockContent } from "@/lib/query/sessions";
import { cn } from "@/lib/utils";
import type { ContentRef, ToolCallBlock } from "@/types";
import { highlightQueryText as highlightText } from "../utils";
import { useReaderContext } from "./context";
import { useReaderT } from "./i18n";
import { PathChip } from "./PathChip";

/**
 * 改动渲染：unified diff / apply_patch 文本 →
 * 行号 + `+` success-soft / `-` danger-soft，hunk 之间用 `⋮` 隔开，
 * 多文件按文件头展示 PathChip；有完整补丁引用时支持「统一 Diff / 原始补丁」视图切换。
 */

export type DiffLine =
  | { type: "file"; path: string }
  | { type: "gap" }
  | {
      type: "add" | "del" | "context";
      text: string;
      oldNo?: number;
      newNo?: number;
    };

export type DiffViewMode = "unified" | "raw";

const HUNK_HEADER = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;
const PATCH_FILE = /^\*\*\* (?:Add|Update|Delete) File: (.+)$/;

/** 解析 unified diff（git diff、Codex FileChange）与 Codex apply_patch 文本 */
export const parseDiff = (text: string): DiffLine[] => {
  const lines: DiffLine[] = [];
  let oldNo: number | undefined;
  let newNo: number | undefined;
  let hunks = 0;
  const pushFile = (path: string) => {
    const last = lines[lines.length - 1];
    if (last?.type === "file" && last.path === path) return;
    lines.push({ type: "file", path });
    hunks = 0;
  };

  text
    .replace(/\r\n/g, "\n")
    .split("\n")
    .forEach((raw, index, all) => {
      // `--- a` / `+++ b` 只在成对出现时算文件头，否则是内容以 `--`、`++` 开头的改动行
      const fileHeaderOld =
        raw.startsWith("--- ") && all[index + 1]?.startsWith("+++ ");
      const fileHeaderNew =
        raw.startsWith("+++ ") && all[index - 1]?.startsWith("--- ");
      if (raw.startsWith("diff --git ")) {
        hunks = 0;
        return;
      }
      if (
        (raw.startsWith("index ") && hunks === 0) ||
        fileHeaderOld ||
        raw.startsWith("*** Begin Patch") ||
        raw.startsWith("*** End Patch") ||
        raw.startsWith("\\ No newline")
      ) {
        return;
      }
      if (fileHeaderNew) {
        const path = raw.slice(4).replace(/^b\//, "").trim();
        if (path && path !== "/dev/null") pushFile(path);
        return;
      }
      const patchFile = PATCH_FILE.exec(raw);
      if (patchFile) {
        pushFile(patchFile[1].trim());
        oldNo = undefined;
        newNo = undefined;
        return;
      }
      if (raw.startsWith("*** Move to: ")) {
        pushFile(raw.slice("*** Move to: ".length).trim());
        return;
      }
      if (raw.startsWith("@@")) {
        if (hunks > 0) lines.push({ type: "gap" });
        hunks += 1;
        const match = HUNK_HEADER.exec(raw);
        oldNo = match ? Number(match[1]) : undefined;
        newNo = match ? Number(match[2]) : undefined;
        return;
      }
      if (raw.startsWith("+")) {
        lines.push({ type: "add", text: raw.slice(1), newNo });
        if (newNo !== undefined) newNo += 1;
        return;
      }
      if (raw.startsWith("-")) {
        lines.push({ type: "del", text: raw.slice(1), oldNo });
        if (oldNo !== undefined) oldNo += 1;
        return;
      }
      if (raw === "" && lines.length === 0) return;
      lines.push({
        type: "context",
        text: raw.startsWith(" ") ? raw.slice(1) : raw,
        oldNo,
        newNo,
      });
      if (oldNo !== undefined) oldNo += 1;
      if (newNo !== undefined) newNo += 1;
    });

  // 末尾的空上下文行（文本以换行结尾）不显示
  while (lines.length > 0) {
    const last = lines[lines.length - 1];
    if (last.type === "context" && last.text === "") lines.pop();
    else break;
  }
  return lines;
};

const pickString = (input: Record<string, unknown>, keys: string[]) => {
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string") return value;
  }
  return undefined;
};

const splitLines = (text: string) =>
  text === "" ? [] : text.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n");

const OLD_KEYS = [
  "old_string",
  "oldString",
  "oldText",
  "old_str",
  "TargetContent",
  "Target",
];

const NEW_KEYS = [
  "new_string",
  "newString",
  "newText",
  "new_str",
  "ReplacementContent",
  "Replacement",
];

const CONTENT_KEYS = [
  "content",
  "contents",
  "file_text",
  "text",
  "Content",
  "CodeContent",
];

/**
 * 没有 unified diff 全文时，从调用参数还原改动：Edit 类的 old/new 字符串、Write 类的 content。
 * 参数预览被截断（JSON 不完整）时返回 null。
 */
export const buildInputDiff = (call: ToolCallBlock): string | null => {
  if (call.kind !== "edit" && call.kind !== "write") return null;
  if (call.inputPreview.length < call.inputTotalLen) return null;
  let input: unknown;
  try {
    input = JSON.parse(call.inputPreview);
  } catch {
    return null;
  }
  if (!input || typeof input !== "object") return null;
  const record = input as Record<string, unknown>;
  const pairs: { old: string; next: string }[] = [];
  if (Array.isArray(record.ReplacementChunks)) {
    for (const chunk of record.ReplacementChunks) {
      if (!chunk || typeof chunk !== "object") return null;
      const item = chunk as Record<string, unknown>;
      const oldText = pickString(item, OLD_KEYS);
      const newText = pickString(item, NEW_KEYS);
      if (oldText === undefined || newText === undefined) return null;
      pairs.push({ old: oldText, next: newText });
    }
    if (pairs.length === 0) return null;
  } else {
    const edits = Array.isArray(record.edits) ? record.edits : [record];
    edits.forEach((edit) => {
      if (!edit || typeof edit !== "object") return;
      const item = edit as Record<string, unknown>;
      const oldText = pickString(item, OLD_KEYS);
      const newText = pickString(item, NEW_KEYS);
      if (oldText !== undefined || newText !== undefined) {
        pairs.push({ old: oldText ?? "", next: newText ?? "" });
      }
    });
  }
  if (pairs.length === 0) {
    const content = pickString(record, CONTENT_KEYS);
    if (content === undefined) return null;
    pairs.push({ old: "", next: content });
  }
  if (pairs.every(({ old, next }) => old === "" && next === "")) return null;
  return pairs
    .map(
      ({ old, next }) =>
        `@@\n${[
          ...splitLines(old).map((line) => `-${line}`),
          ...splitLines(next).map((line) => `+${line}`),
        ].join("\n")}`,
    )
    .join("\n");
};

const ROW_TONE = {
  add: "bg-diff-add",
  del: "bg-diff-del",
  context: "",
} as const;

const SIGN = { add: "+", del: "−", context: " " } as const;

export interface SessionDiffProps {
  /** unified diff / apply_patch 文本 */
  text: string;
  searchQuery?: string;
  projectDir?: string;
  /** 文件小标题显示用（相对路径） */
  formatPath?: (path: string) => string;
  /** 单文件改动没有内嵌文件头时的补充路径 */
  singleFilePath?: string;
  /** 是否展示「统一 Diff / 原始补丁」切换（如 diff.full 可用时） */
  showViewToggle?: boolean;
  className?: string;
}

/** unified diff 渲染：只读、横向滚动、最高 480px，支持按文件头显示 PathChip 与原始补丁切换 */
export const SessionDiff = memo(function SessionDiff({
  text,
  searchQuery,
  projectDir,
  formatPath,
  singleFilePath,
  showViewToggle = false,
  className,
}: SessionDiffProps) {
  const rt = useReaderT();
  const [viewMode, setViewMode] = useState<DiffViewMode>("unified");
  const lines = useMemo(() => {
    const parsed = parseDiff(text);
    if (
      singleFilePath &&
      parsed.length > 0 &&
      !parsed.some((line) => line.type === "file")
    ) {
      return [{ type: "file" as const, path: singleFilePath }, ...parsed];
    }
    return parsed;
  }, [singleFilePath, text]);

  const hasNumbers = lines.some(
    (line) =>
      (line.type === "add" || line.type === "del" || line.type === "context") &&
      (line.oldNo !== undefined || line.newNo !== undefined),
  );

  const viewOptions = useMemo(
    () =>
      [
        { value: "unified", label: rt("diffUnified") },
        { value: "raw", label: rt("diffRaw") },
      ] as const,
    [rt],
  );

  return (
    <div
      className={cn(
        "overflow-hidden rounded-[8px] border border-border bg-surface font-mono text-caption",
        className,
      )}
    >
      {showViewToggle && (
        <div className="flex items-center justify-end border-b border-border bg-subtle px-2.5 py-1 font-sans">
          <SegmentedControl<DiffViewMode>
            size="compact"
            value={viewMode}
            onValueChange={setViewMode}
            options={viewOptions}
            aria-label={rt("diff")}
          />
        </div>
      )}
      <div
        tabIndex={0}
        className="max-h-[480px] overflow-auto outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
      >
        {showViewToggle && viewMode === "raw" ? (
          <pre className="m-0 whitespace-pre px-3 py-2 font-mono text-caption text-fg-1">
            {searchQuery ? highlightText(text, searchQuery) : text}
          </pre>
        ) : (
          <div className="min-w-max py-1">
            {lines.map((line, index) => {
              if (line.type === "file") {
                const formatted = formatPath
                  ? formatPath(line.path)
                  : undefined;
                return (
                  <div
                    key={index}
                    className="sticky left-0 flex items-center border-b border-border bg-subtle px-3 py-1 font-sans text-fg-2 [&:not(:first-child)]:mt-1 [&:not(:first-child)]:border-t"
                  >
                    <PathChip
                      path={line.path}
                      projectDir={projectDir}
                      label={formatted}
                      searchQuery={searchQuery}
                    />
                  </div>
                );
              }
              if (line.type === "gap") {
                return (
                  <div
                    key={index}
                    aria-hidden
                    className="select-none px-3 leading-5 text-fg-3"
                  >
                    ⋮
                  </div>
                );
              }
              return (
                <div
                  key={index}
                  className={cn("flex leading-5", ROW_TONE[line.type])}
                >
                  {hasNumbers && (
                    <>
                      <span className="w-10 shrink-0 select-none pe-2 text-right tabular-nums text-fg-3">
                        {line.type === "add" ? "" : (line.oldNo ?? "")}
                      </span>
                      <span className="w-10 shrink-0 select-none pe-2 text-right tabular-nums text-fg-3">
                        {line.type === "del" ? "" : (line.newNo ?? "")}
                      </span>
                    </>
                  )}
                  <span
                    aria-hidden
                    className={cn(
                      "w-5 shrink-0 select-none text-center",
                      line.type === "add" && "text-success-text",
                      line.type === "del" && "text-danger-text",
                    )}
                  >
                    {SIGN[line.type]}
                  </span>
                  <span className="sr-only">
                    {line.type === "add" ? "+" : line.type === "del" ? "-" : ""}
                  </span>
                  <span className="whitespace-pre pe-3 text-fg-1">
                    {searchQuery
                      ? highlightText(line.text, searchQuery)
                      : line.text}
                  </span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
});

/** 按 ContentRef 异步拉取完整补丁并渲染（带「统一 Diff / 原始补丁」切换与加载更多） */
export const SessionDiffFromRef = memo(function SessionDiffFromRef({
  diffRef,
  formatPath,
  singleFilePath,
}: {
  diffRef: ContentRef;
  formatPath?: (path: string) => string;
  singleFilePath?: string;
}) {
  const rt = useReaderT();
  const { providerId, sourcePath, projectDir, searchQuery } =
    useReaderContext();
  const content = useBlockContent(providerId, sourcePath, diffRef);

  if (content.isLoading) {
    return (
      <div className="h-16 rounded-[8px] border border-border bg-subtle motion-safe:animate-pulse" />
    );
  }
  if (content.error || !content.text) {
    return <p className="m-0 text-caption text-fg-3">{rt("loadFailed")}</p>;
  }
  return (
    <div className="min-w-0 space-y-1">
      <SessionDiff
        text={content.text}
        searchQuery={searchQuery}
        projectDir={projectDir}
        formatPath={formatPath}
        singleFilePath={singleFilePath}
        showViewToggle
      />
      {content.hasMore && (
        <button
          type="button"
          disabled={content.isFetchingMore}
          onClick={content.loadMore}
          className="rounded-[2px] text-caption text-fg-2 underline decoration-border-strong underline-offset-[3px] transition-colors hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {rt("loadMore")}
        </button>
      )}
    </div>
  );
});
