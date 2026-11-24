import {
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Check, Copy } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";

const FENCE_OPEN_RE = /^ {0,3}(`{3,}|~{3,})(.*)$/;
const HEADING_RE = /^ {0,3}(#{1,4})\s+(.+)$/;
const HR_RE = /^ {0,3}(?:---+|___+|\*\*\*+)\s*$/;
const QUOTE_RE = /^ {0,3}>\s?(.*)$/;
const UNORDERED_LIST_RE = /^ {0,3}[-*+]\s+(.+)$/;
const ORDERED_LIST_RE = /^ {0,3}\d+[.)]\s+(.+)$/;
const TABLE_SEP_CELL_RE = /^:?-{3,}:?$/;
const TASK_ITEM_RE = /^\[([ xX])\]\s+(.+)$/;

const RICH_MARKDOWN_PATTERNS = [
  /^ {0,3}(?:`{3,}|~{3,})/m,
  /^ {0,3}#{1,4}\s+\S/m,
  /^ {0,3}>\s?\S/m,
  /^ {0,3}(?:[-*+]|\d+[.)])\s+\S/m,
  /^ {0,3}(?:---+|___+|\*\*\*+)\s*$/m,
  /(`+)[^`\n].*?\1/,
  /\*\*[^*\n]+?\*\*/,
  /~~[^~\n]+?~~/,
  /\[[^\]\n]+\]\(https:\/\/[^)\s]+\)/,
  /^ {0,3}\|.*\|.*\n {0,3}\|?[\s:-]+\|/m,
];

export function hasRichMarkdown(content: string): boolean {
  return RICH_MARKDOWN_PATTERNS.some((pattern) => pattern.test(content));
}

function matchInlineCode(
  text: string,
  pos: number,
): { content: string; nextPos: number } | { unmatchedEnd: number } {
  let openEnd = pos;
  while (openEnd < text.length && text[openEnd] === "`") {
    openEnd += 1;
  }
  const tickLen = openEnd - pos;
  let searchPos = openEnd;

  while (searchPos < text.length) {
    if (text[searchPos] === "\n") {
      return { unmatchedEnd: openEnd };
    }
    if (text[searchPos] === "`") {
      let closeEnd = searchPos;
      while (closeEnd < text.length && text[closeEnd] === "`") {
        closeEnd += 1;
      }
      if (closeEnd - searchPos === tickLen) {
        let raw = text.slice(openEnd, searchPos);
        if (
          raw.length >= 2 &&
          raw.startsWith(" ") &&
          raw.endsWith(" ") &&
          raw.trim().length > 0
        ) {
          raw = raw.slice(1, -1);
        }
        if (raw.length > 0) {
          return { content: raw, nextPos: closeEnd };
        }
        return { unmatchedEnd: closeEnd };
      }
      searchPos = closeEnd;
      continue;
    }
    searchPos += 1;
  }

  return { unmatchedEnd: openEnd };
}

function matchMarkdownLink(
  text: string,
  pos: number,
): { label: string; url: string; nextPos: number } | null {
  if (text[pos] !== "[") return null;
  let bracketClose = pos + 1;
  while (bracketClose < text.length && text[bracketClose] !== "]") {
    if (text[bracketClose] === "\n" || text[bracketClose] === "[") return null;
    bracketClose += 1;
  }
  if (bracketClose >= text.length || text[bracketClose + 1] !== "(") {
    return null;
  }
  const label = text.slice(pos + 1, bracketClose).trim();
  if (!label) return null;

  const urlStart = bracketClose + 2;
  let urlEnd = urlStart;
  while (urlEnd < text.length && text[urlEnd] !== ")") {
    if (/\s/.test(text[urlEnd])) return null;
    urlEnd += 1;
  }
  if (urlEnd >= text.length) return null;
  const rawUrl = text.slice(urlStart, urlEnd).trim();
  if (!rawUrl.startsWith("https://")) return null;
  try {
    const parsed = new URL(rawUrl);
    if (parsed.protocol !== "https:") return null;
  } catch {
    return null;
  }
  return { label, url: rawUrl, nextPos: urlEnd + 1 };
}

function renderInlineCodeOnly(text: string, terms: string[]): ReactNode {
  if (!text.includes("`")) {
    return highlightText(text, terms);
  }
  const nodes: ReactNode[] = [];
  let plain = "";
  let pos = 0;
  let key = 0;

  while (pos < text.length) {
    if (text[pos] === "`") {
      const matched = matchInlineCode(text, pos);
      if ("content" in matched) {
        if (plain) {
          nodes.push(highlightText(plain, terms));
          plain = "";
        }
        nodes.push(
          <code key={key++} className="asm-inline-code">
            {highlightText(matched.content, terms)}
          </code>,
        );
        pos = matched.nextPos;
        continue;
      }
      plain += text.slice(pos, matched.unmatchedEnd);
      pos = matched.unmatchedEnd;
      continue;
    }
    plain += text[pos];
    pos += 1;
  }

  if (plain) {
    nodes.push(highlightText(plain, terms));
  }
  return nodes.length === 1 ? nodes[0] : nodes;
}

function renderInline(
  text: string,
  terms: string[],
  onOpenUrl?: (url: string) => void,
): ReactNode {
  if (
    !text.includes("`") &&
    !text.includes("*") &&
    !text.includes("~~") &&
    !text.includes("[")
  ) {
    return highlightText(text, terms);
  }

  const nodes: ReactNode[] = [];
  let plain = "";
  let pos = 0;
  let key = 0;

  const flushPlain = () => {
    if (!plain) return;
    nodes.push(highlightText(plain, terms));
    plain = "";
  };

  while (pos < text.length) {
    if (text[pos] === "`") {
      const matched = matchInlineCode(text, pos);
      if ("content" in matched) {
        flushPlain();
        nodes.push(
          <code key={`c-${key++}`} className="asm-inline-code">
            {highlightText(matched.content, terms)}
          </code>,
        );
        pos = matched.nextPos;
        continue;
      }
      plain += text.slice(pos, matched.unmatchedEnd);
      pos = matched.unmatchedEnd;
      continue;
    }

    if (text.startsWith("**", pos)) {
      const close = text.indexOf("**", pos + 2);
      if (close > pos + 2 && !text.slice(pos + 2, close).includes("\n")) {
        flushPlain();
        nodes.push(
          <strong key={`b-${key++}`}>
            {renderInlineCodeOnly(text.slice(pos + 2, close), terms)}
          </strong>,
        );
        pos = close + 2;
        continue;
      }
    }

    if (text.startsWith("~~", pos)) {
      const close = text.indexOf("~~", pos + 2);
      if (close > pos + 2 && !text.slice(pos + 2, close).includes("\n")) {
        flushPlain();
        nodes.push(
          <del key={`s-${key++}`} className="asm-md-del">
            {renderInlineCodeOnly(text.slice(pos + 2, close), terms)}
          </del>,
        );
        pos = close + 2;
        continue;
      }
    }

    if (
      text[pos] === "*" &&
      text[pos + 1] !== "*" &&
      text[pos + 1] !== undefined &&
      !/\s/.test(text[pos + 1])
    ) {
      let close = pos + 2;
      let found = -1;
      while (close < text.length && text[close] !== "\n") {
        if (
          text[close] === "*" &&
          text[close - 1] !== "*" &&
          text[close + 1] !== "*" &&
          !/\s/.test(text[close - 1])
        ) {
          found = close;
          break;
        }
        close += 1;
      }
      if (found > pos + 1) {
        flushPlain();
        nodes.push(
          <em key={`i-${key++}`}>
            {renderInlineCodeOnly(text.slice(pos + 1, found), terms)}
          </em>,
        );
        pos = found + 1;
        continue;
      }
    }

    if (text[pos] === "[") {
      const linkMatch = matchMarkdownLink(text, pos);
      if (linkMatch) {
        flushPlain();
        const targetUrl = linkMatch.url;
        nodes.push(
          <a
            key={`l-${key++}`}
            href={targetUrl}
            target="_blank"
            rel="noreferrer noopener"
            className="asm-md-link"
            onClick={(event) => {
              event.preventDefault();
              if (onOpenUrl) {
                onOpenUrl(targetUrl);
              } else {
                void openUrl(targetUrl).catch(() => undefined);
              }
            }}
          >
            {renderInlineCodeOnly(linkMatch.label, terms)}
          </a>,
        );
        pos = linkMatch.nextPos;
        continue;
      }
    }

    plain += text[pos];
    pos += 1;
  }

  flushPlain();
  return nodes.length === 1 ? nodes[0] : nodes;
}

function splitTableRow(line: string): string[] | null {
  const trimmed = line.trim();
  if (!trimmed.includes("|")) return null;
  let inner = trimmed;
  if (inner.startsWith("|")) inner = inner.slice(1);
  if (inner.endsWith("|") && !inner.endsWith("\\|")) inner = inner.slice(0, -1);

  const cells: string[] = [];
  let current = "";
  let inCode = false;
  for (let i = 0; i < inner.length; i += 1) {
    const ch = inner[i];
    if (ch === "\\" && inner[i + 1] === "|") {
      current += "|";
      i += 1;
      continue;
    }
    if (ch === "`") {
      inCode = !inCode;
      current += ch;
      continue;
    }
    if (ch === "|" && !inCode) {
      cells.push(current.trim());
      current = "";
      continue;
    }
    current += ch;
  }
  cells.push(current.trim());
  return cells.length >= 2 ? cells : null;
}

export type TableAlign = "left" | "center" | "right" | null;

function parseTableSeparator(
  line: string,
  expectedCols: number,
): TableAlign[] | null {
  const cells = splitTableRow(line);
  if (!cells || cells.length !== expectedCols) return null;
  const aligns: TableAlign[] = [];
  for (const cell of cells) {
    if (!TABLE_SEP_CELL_RE.test(cell)) return null;
    const left = cell.startsWith(":");
    const right = cell.endsWith(":");
    aligns.push(
      left && right ? "center" : right ? "right" : left ? "left" : null,
    );
  }
  return aligns;
}

function CodeBlock({
  lang,
  code,
  blockIndex,
  unclosed = false,
  highlightTerms,
  onCopyCode,
}: {
  lang: string;
  code: string;
  blockIndex: number;
  unclosed?: boolean;
  highlightTerms: string[];
  onCopyCode: (code: string, blockIndex: number) => Promise<void> | void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const [isCopying, setIsCopying] = useState(false);
  const mountedRef = useRef(true);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
      }
    };
  }, []);

  const handleCopy = useCallback(() => {
    if (isCopying) return;
    let result: Promise<void> | void;
    try {
      result = onCopyCode(code, blockIndex);
    } catch {
      return;
    }

    const markCopied = () => {
      if (!mountedRef.current) return;
      setCopied(true);
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
      }
      timerRef.current = setTimeout(() => {
        if (mountedRef.current) setCopied(false);
        timerRef.current = null;
      }, 1500);
    };

    if (result && typeof (result as Promise<void>).then === "function") {
      setIsCopying(true);
      void (result as Promise<void>)
        .then(() => {
          markCopied();
        })
        .catch(() => undefined)
        .finally(() => {
          if (mountedRef.current) setIsCopying(false);
        });
    } else {
      markCopied();
    }
  }, [blockIndex, code, isCopying, onCopyCode]);

  const normalizedLang = lang.trim().toLocaleLowerCase();
  const isDiff = normalizedLang === "diff" || normalizedLang === "patch";
  const displayLang = normalizedLang || "text";

  return (
    <div className="asm-code-block">
      <div className="asm-code-header">
        <span className="asm-code-lang">{displayLang}</span>
        <button
          type="button"
          className="asm-code-copy"
          disabled={isCopying}
          aria-busy={isCopying || undefined}
          aria-label={
            copied
              ? t("sessionManager.copied", { defaultValue: "已复制" })
              : t("sessionManager.copyCode", { defaultValue: "复制代码" })
          }
          onClick={handleCopy}
        >
          {copied ? (
            <>
              <Check className="size-3" aria-hidden="true" />
              <span>
                {t("sessionManager.copied", { defaultValue: "已复制" })}
              </span>
            </>
          ) : (
            <>
              <Copy className="size-3" aria-hidden="true" />
              <span>
                {t("sessionManager.copyCode", { defaultValue: "复制代码" })}
              </span>
            </>
          )}
        </button>
      </div>
      <pre className="asm-code-pre">
        <code>
          {isDiff
            ? code.split("\n").map((line, index) => {
                const lineClass = line.startsWith("@@")
                  ? "is-hunk"
                  : line.startsWith("+") && !line.startsWith("+++")
                    ? "is-add"
                    : line.startsWith("-") && !line.startsWith("---")
                      ? "is-del"
                      : "";
                return (
                  <span key={index} className={cn("asm-diff-line", lineClass)}>
                    {line ? highlightText(line, highlightTerms) : "\u200b"}
                  </span>
                );
              })
            : highlightText(code, highlightTerms)}
        </code>
      </pre>
      {unclosed && (
        <div className="asm-code-truncated" aria-hidden="true">
          …
        </div>
      )}
    </div>
  );
}

export type MarkdownBlock =
  | { kind: "code"; lang: string; code: string; closed: boolean }
  | { kind: "heading"; level: number; text: string }
  | { kind: "hr" }
  | { kind: "quote"; lines: string[] }
  | { kind: "list"; ordered: boolean; items: string[] }
  | {
      kind: "table";
      headers: string[];
      aligns: TableAlign[];
      rows: string[][];
    }
  | { kind: "paragraph"; text: string };

export function parseBlocks(content: string): MarkdownBlock[] {
  const lines = content.replace(/\r\n?/g, "\n").split("\n");
  const blocks: MarkdownBlock[] = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      index += 1;
      continue;
    }

    const fenceMatch = line.match(FENCE_OPEN_RE);
    if (fenceMatch) {
      const markerChar = fenceMatch[1][0];
      const markerLen = fenceMatch[1].length;
      const lang = fenceMatch[2].trim().split(/\s+/)[0] ?? "";
      const closeRe = new RegExp(
        `^ {0,3}${markerChar === "`" ? "`" : "~"}{${markerLen},}\\s*$`,
      );
      const codeLines: string[] = [];
      index += 1;
      while (index < lines.length && !closeRe.test(lines[index])) {
        codeLines.push(lines[index]);
        index += 1;
      }
      const closed = index < lines.length;
      if (closed) {
        index += 1;
      }
      blocks.push({ kind: "code", lang, code: codeLines.join("\n"), closed });
      continue;
    }

    if (HR_RE.test(line)) {
      blocks.push({ kind: "hr" });
      index += 1;
      continue;
    }

    const headingMatch = line.match(HEADING_RE);
    if (headingMatch) {
      blocks.push({
        kind: "heading",
        level: headingMatch[1].length,
        text: headingMatch[2],
      });
      index += 1;
      continue;
    }

    if (QUOTE_RE.test(line)) {
      const quoteLines: string[] = [];
      while (index < lines.length) {
        const match = lines[index].match(QUOTE_RE);
        if (!match) break;
        quoteLines.push(match[1]);
        index += 1;
      }
      blocks.push({ kind: "quote", lines: quoteLines });
      continue;
    }

    if (index + 1 < lines.length) {
      const headerCells = splitTableRow(line);
      const aligns = headerCells
        ? parseTableSeparator(lines[index + 1], headerCells.length)
        : null;
      if (headerCells && aligns) {
        const rows: string[][] = [];
        index += 2;
        while (index < lines.length) {
          const rowLine = lines[index];
          if (!rowLine.trim() || FENCE_OPEN_RE.test(rowLine)) break;
          const rowCells = splitTableRow(rowLine);
          if (!rowCells) break;
          const normalized = Array.from(
            { length: headerCells.length },
            (_, col) => rowCells[col] ?? "",
          );
          rows.push(normalized);
          index += 1;
        }
        blocks.push({
          kind: "table",
          headers: headerCells,
          aligns,
          rows,
        });
        continue;
      }
    }

    if (UNORDERED_LIST_RE.test(line)) {
      const items: string[] = [];
      while (index < lines.length) {
        const match = lines[index].match(UNORDERED_LIST_RE);
        if (!match) break;
        items.push(match[1]);
        index += 1;
      }
      blocks.push({ kind: "list", ordered: false, items });
      continue;
    }

    if (ORDERED_LIST_RE.test(line)) {
      const items: string[] = [];
      while (index < lines.length) {
        const match = lines[index].match(ORDERED_LIST_RE);
        if (!match) break;
        items.push(match[1]);
        index += 1;
      }
      blocks.push({ kind: "list", ordered: true, items });
      continue;
    }

    const paragraphLines: string[] = [];
    while (index < lines.length) {
      const current = lines[index];
      const startsTable =
        index + 1 < lines.length &&
        (() => {
          const hc = splitTableRow(current);
          return hc
            ? Boolean(parseTableSeparator(lines[index + 1], hc.length))
            : false;
        })();
      if (
        !current.trim() ||
        FENCE_OPEN_RE.test(current) ||
        HR_RE.test(current) ||
        HEADING_RE.test(current) ||
        QUOTE_RE.test(current) ||
        UNORDERED_LIST_RE.test(current) ||
        ORDERED_LIST_RE.test(current) ||
        startsTable
      ) {
        break;
      }
      paragraphLines.push(current);
      index += 1;
    }
    blocks.push({ kind: "paragraph", text: paragraphLines.join("\n") });
  }

  return blocks;
}

export function extractFullCodeBlock(
  fullText: string,
  blockIndex: number,
  previewCode: string,
): string {
  const blocks = parseBlocks(fullText);
  const candidate = blocks[blockIndex];
  if (
    candidate?.kind === "code" &&
    (!previewCode || candidate.code.startsWith(previewCode))
  ) {
    return candidate.code;
  }
  if (previewCode) {
    const prefixMatch = blocks.find(
      (block): block is Extract<MarkdownBlock, { kind: "code" }> =>
        block.kind === "code" && block.code.startsWith(previewCode),
    );
    if (prefixMatch) {
      return prefixMatch.code;
    }
  }
  if (candidate?.kind === "code") {
    return candidate.code;
  }
  return previewCode || fullText;
}

export const MessageMarkdown = memo(function MessageMarkdown({
  content,
  collapsed = false,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopyCode,
  onOpenUrl,
}: {
  content: string;
  collapsed?: boolean;
  highlightTerms?: string[];
  onCopyCode: (code: string, blockIndex: number) => Promise<void> | void;
  onOpenUrl?: (url: string) => void;
}) {
  const blocks = useMemo(() => parseBlocks(content), [content]);

  if (blocks.length === 0) {
    return (
      <p className="asm-md-p">
        {highlightText(content, highlightTerms)}
        {collapsed && "…"}
      </p>
    );
  }

  return (
    <div className="asm-md">
      {blocks.map((block, index) => {
        const isLastCollapsed = collapsed && index === blocks.length - 1;
        switch (block.kind) {
          case "code":
            return (
              <CodeBlock
                key={index}
                lang={block.lang}
                code={block.code}
                blockIndex={index}
                unclosed={isLastCollapsed && !block.closed}
                highlightTerms={highlightTerms}
                onCopyCode={onCopyCode}
              />
            );
          case "heading":
            return (
              <div
                key={index}
                className={cn("asm-md-heading", `is-h${block.level}`)}
              >
                {renderInline(block.text, highlightTerms, onOpenUrl)}
                {isLastCollapsed && "…"}
              </div>
            );
          case "hr":
            return <hr key={index} className="asm-md-hr" />;
          case "quote":
            return (
              <blockquote key={index} className="asm-md-quote">
                {block.lines.map((line, lineIndex) => (
                  <p key={lineIndex} className="asm-md-p">
                    {line
                      ? renderInline(line, highlightTerms, onOpenUrl)
                      : "\u200b"}
                    {isLastCollapsed &&
                      lineIndex === block.lines.length - 1 &&
                      "…"}
                  </p>
                ))}
              </blockquote>
            );
          case "table":
            return (
              <div key={index} className="asm-md-table-wrap">
                <table className="asm-md-table">
                  <thead>
                    <tr>
                      {block.headers.map((header, colIdx) => (
                        <th
                          key={colIdx}
                          style={
                            block.aligns[colIdx]
                              ? { textAlign: block.aligns[colIdx] ?? undefined }
                              : undefined
                          }
                        >
                          {renderInline(header, highlightTerms, onOpenUrl)}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {block.rows.map((row, rowIdx) => (
                      <tr key={rowIdx}>
                        {row.map((cell, colIdx) => (
                          <td
                            key={colIdx}
                            style={
                              block.aligns[colIdx]
                                ? {
                                    textAlign:
                                      block.aligns[colIdx] ?? undefined,
                                  }
                                : undefined
                            }
                          >
                            {renderInline(cell, highlightTerms, onOpenUrl)}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            );
          case "list": {
            const ListTag = block.ordered ? "ol" : "ul";
            return (
              <ListTag
                key={index}
                className={cn("asm-md-list", block.ordered && "is-ordered")}
              >
                {block.items.map((item, itemIndex) => {
                  const taskMatch = !block.ordered
                    ? item.match(TASK_ITEM_RE)
                    : null;
                  if (taskMatch) {
                    const checked = taskMatch[1].toLowerCase() === "x";
                    return (
                      <li key={itemIndex} className="asm-md-task-item">
                        <span
                          className={cn(
                            "asm-md-task-box",
                            checked && "is-checked",
                          )}
                          aria-hidden="true"
                        >
                          {checked ? "✓" : ""}
                        </span>
                        <span>
                          {renderInline(
                            taskMatch[2],
                            highlightTerms,
                            onOpenUrl,
                          )}
                          {isLastCollapsed &&
                            itemIndex === block.items.length - 1 &&
                            "…"}
                        </span>
                      </li>
                    );
                  }
                  return (
                    <li key={itemIndex}>
                      {renderInline(item, highlightTerms, onOpenUrl)}
                      {isLastCollapsed &&
                        itemIndex === block.items.length - 1 &&
                        "…"}
                    </li>
                  );
                })}
              </ListTag>
            );
          }
          case "paragraph":
            return (
              <p key={index} className="asm-md-p">
                {renderInline(block.text, highlightTerms, onOpenUrl)}
                {isLastCollapsed && "…"}
              </p>
            );
        }
      })}
    </div>
  );
});
