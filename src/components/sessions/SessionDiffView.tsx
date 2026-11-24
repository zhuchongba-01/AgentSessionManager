import { memo, useMemo } from "react";
import { cn } from "@/lib/utils";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";
import { shortenProjectPath } from "./agentReaderStyles";

export type ParsedDiffLine =
  | { type: "file"; path: string }
  | { type: "gap" }
  | {
      type: "add" | "del" | "context";
      text: string;
      oldNo?: number;
      newNo?: number;
    };

const HUNK_HEADER_RE = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;
const PATCH_FILE_RE = /^\*\*\* (?:Add|Update|Delete) File:\s*(.+)$/;
const FENCED_DIFF_RE = /```(?:diff|patch)\s*\n([\s\S]*?)(?:```|$)/i;

export function extractDiffPayload(content: string): {
  diffText: string;
  added: number;
  removed: number;
} | null {
  const fenced = FENCED_DIFF_RE.exec(content);
  const candidate = fenced ? fenced[1] : content;
  const hasMarkers =
    Boolean(fenced) ||
    /(?:^|\n)(?:@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@|\*\*\* (?:Add|Update|Delete) File:|diff --git )/m.test(
      candidate,
    );
  if (!hasMarkers) return null;

  const lines = candidate.replace(/\r\n/g, "\n").split("\n");
  // 若未用 ```diff 包裹，则从首个 diff/patch 标记行开始截取，跳过头部的 `[Tool: Edit]`
  let startIdx = 0;
  if (!fenced) {
    const markerIdx = lines.findIndex(
      (line) =>
        line.startsWith("diff --git ") ||
        line.startsWith("--- ") ||
        line.startsWith("@@ ") ||
        PATCH_FILE_RE.test(line) ||
        line.startsWith("*** Begin Patch"),
    );
    if (markerIdx >= 0) startIdx = markerIdx;
  }

  const sliced = lines.slice(startIdx).join("\n");
  let added = 0;
  let removed = 0;
  for (let i = startIdx; i < lines.length; i += 1) {
    const raw = lines[i];
    if (raw.startsWith("+") && !raw.startsWith("+++")) added += 1;
    else if (raw.startsWith("-") && !raw.startsWith("---")) removed += 1;
  }
  if (added === 0 && removed === 0) return null;
  return { diffText: sliced, added, removed };
}

export function parseUnifiedDiff(text: string): ParsedDiffLine[] {
  const lines: ParsedDiffLine[] = [];
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
      const patchMatch = PATCH_FILE_RE.exec(raw);
      if (patchMatch) {
        pushFile(patchMatch[1].trim());
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
        const match = HUNK_HEADER_RE.exec(raw);
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

  while (lines.length > 0) {
    const last = lines[lines.length - 1];
    if (last.type === "context" && last.text === "") lines.pop();
    else break;
  }
  return lines;
}

export const SessionDiffView = memo(function SessionDiffView({
  diffText,
  projectDir,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
}: {
  diffText: string;
  projectDir?: string | null;
  highlightTerms?: string[];
}) {
  const parsed = useMemo(() => parseUnifiedDiff(diffText), [diffText]);
  if (parsed.length === 0) return null;

  return (
    <div className="asm-ccs-diff">
      {parsed.map((line, index) => {
        if (line.type === "file") {
          return (
            <div key={index} className="asm-ccs-diff-file">
              {shortenProjectPath(line.path, projectDir)}
            </div>
          );
        }
        if (line.type === "gap") {
          return (
            <div key={index} className="asm-ccs-diff-gap" aria-hidden="true">
              ⋮
            </div>
          );
        }
        return (
          <div
            key={index}
            className={cn(
              "asm-ccs-diff-row",
              line.type === "add" && "is-add",
              line.type === "del" && "is-del",
            )}
          >
            <span className="asm-ccs-diff-ln" aria-hidden="true">
              {line.oldNo ?? ""}
            </span>
            <span className="asm-ccs-diff-ln" aria-hidden="true">
              {line.newNo ?? ""}
            </span>
            <span className="asm-ccs-diff-sign" aria-hidden="true">
              {line.type === "add" ? "+" : line.type === "del" ? "−" : " "}
            </span>
            <span className="asm-ccs-diff-code">
              {line.text ? highlightText(line.text, highlightTerms) : "\u200b"}
            </span>
          </div>
        );
      })}
    </div>
  );
});
