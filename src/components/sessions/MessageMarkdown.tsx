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
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { EMPTY_HIGHLIGHT_TERMS, highlightText } from "./highlightText";

const FENCE_OPEN_RE = /^ {0,3}(`{3,}|~{3,})(.*)$/;
const HEADING_RE = /^ {0,3}(#{1,4})\s+(.+)$/;
const HR_RE = /^ {0,3}(?:---+|___+|\*\*\*+)\s*$/;
const QUOTE_RE = /^ {0,3}>\s?(.*)$/;
const UNORDERED_LIST_RE = /^ {0,3}[-*+]\s+(.+)$/;
const ORDERED_LIST_RE = /^ {0,3}\d+[.)]\s+(.+)$/;

const RICH_MARKDOWN_PATTERNS = [
  /^ {0,3}(?:`{3,}|~{3,})/m,
  /^ {0,3}#{1,4}\s+\S/m,
  /^ {0,3}>\s?\S/m,
  /^ {0,3}(?:[-*+]|\d+[.)])\s+\S/m,
  /^ {0,3}(?:---+|___+|\*\*\*+)\s*$/m,
  /`[^`\n]+`/,
  /\*\*[^*\n]+?\*\*/,
];

export function hasRichMarkdown(content: string): boolean {
  return RICH_MARKDOWN_PATTERNS.some((pattern) => pattern.test(content));
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
      const close = text.indexOf("`", pos + 1);
      if (close > pos + 1) {
        if (plain) {
          nodes.push(highlightText(plain, terms));
          plain = "";
        }
        nodes.push(
          <code key={key++} className="asm-inline-code">
            {highlightText(text.slice(pos + 1, close), terms)}
          </code>,
        );
        pos = close + 1;
        continue;
      }
    }
    plain += text[pos];
    pos += 1;
  }

  if (plain) {
    nodes.push(highlightText(plain, terms));
  }
  return nodes.length === 1 ? nodes[0] : nodes;
}

function renderInline(text: string, terms: string[]): ReactNode {
  if (!text.includes("`") && !text.includes("**")) {
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
      const close = text.indexOf("`", pos + 1);
      if (close > pos + 1 && !text.slice(pos + 1, close).includes("\n")) {
        flushPlain();
        nodes.push(
          <code key={`c-${key++}`} className="asm-inline-code">
            {highlightText(text.slice(pos + 1, close), terms)}
          </code>,
        );
        pos = close + 1;
        continue;
      }
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

    plain += text[pos];
    pos += 1;
  }

  flushPlain();
  return nodes.length === 1 ? nodes[0] : nodes;
}

function CodeBlock({
  lang,
  code,
  highlightTerms,
  onCopyCode,
}: {
  lang: string;
  code: string;
  highlightTerms: string[];
  onCopyCode: (code: string) => void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
      }
    };
  }, []);

  const handleCopy = useCallback(() => {
    onCopyCode(code);
    setCopied(true);
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
    }
    timerRef.current = setTimeout(() => {
      setCopied(false);
      timerRef.current = null;
    }, 1500);
  }, [code, onCopyCode]);

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
    </div>
  );
}

type MarkdownBlock =
  | { kind: "code"; lang: string; code: string }
  | { kind: "heading"; level: number; text: string }
  | { kind: "hr" }
  | { kind: "quote"; lines: string[] }
  | { kind: "list"; ordered: boolean; items: string[] }
  | { kind: "paragraph"; text: string };

function parseBlocks(content: string): MarkdownBlock[] {
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
      if (index < lines.length) {
        index += 1;
      }
      blocks.push({ kind: "code", lang, code: codeLines.join("\n") });
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
      if (
        !current.trim() ||
        FENCE_OPEN_RE.test(current) ||
        HR_RE.test(current) ||
        HEADING_RE.test(current) ||
        QUOTE_RE.test(current) ||
        UNORDERED_LIST_RE.test(current) ||
        ORDERED_LIST_RE.test(current)
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

export const MessageMarkdown = memo(function MessageMarkdown({
  content,
  highlightTerms = EMPTY_HIGHLIGHT_TERMS,
  onCopyCode,
}: {
  content: string;
  highlightTerms?: string[];
  onCopyCode: (code: string) => void;
}) {
  const blocks = useMemo(() => parseBlocks(content), [content]);

  if (blocks.length === 0) {
    return <p className="asm-md-p">{highlightText(content, highlightTerms)}</p>;
  }

  return (
    <div className="asm-md">
      {blocks.map((block, index) => {
        switch (block.kind) {
          case "code":
            return (
              <CodeBlock
                key={index}
                lang={block.lang}
                code={block.code}
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
                {renderInline(block.text, highlightTerms)}
              </div>
            );
          case "hr":
            return <hr key={index} className="asm-md-hr" />;
          case "quote":
            return (
              <blockquote key={index} className="asm-md-quote">
                {block.lines.map((line, lineIndex) => (
                  <p key={lineIndex} className="asm-md-p">
                    {line ? renderInline(line, highlightTerms) : "\u200b"}
                  </p>
                ))}
              </blockquote>
            );
          case "list": {
            const ListTag = block.ordered ? "ol" : "ul";
            return (
              <ListTag
                key={index}
                className={cn("asm-md-list", block.ordered && "is-ordered")}
              >
                {block.items.map((item, itemIndex) => (
                  <li key={itemIndex}>{renderInline(item, highlightTerms)}</li>
                ))}
              </ListTag>
            );
          }
          case "paragraph":
            return (
              <p key={index} className="asm-md-p">
                {renderInline(block.text, highlightTerms)}
              </p>
            );
        }
      })}
    </div>
  );
});
