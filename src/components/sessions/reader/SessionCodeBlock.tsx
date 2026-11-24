import { memo, useCallback, useMemo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { copyText } from "@/lib/clipboard";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { highlightQueryText as highlightText } from "../utils";

/** 行内 `code` 与代码块共用的边框与底色，保证两处观感一致 */
export const SESSION_CODE_SURFACE = "border border-border bg-subtle";

/** 行内代码样式（Markdown 正文与纯文本提问共用） */
export const SESSION_INLINE_CODE_CLASS = cn(
  SESSION_CODE_SURFACE,
  "rounded-[4px] px-1 py-px font-mono text-[0.92em] text-fg-1",
);

/** 超长代码不做语法分词，避免阻塞滚动帧 */
const TOKENIZE_MAX_CHARS = 32_768;

const HASH_COMMENT_LANGS = new Set([
  "sh",
  "bash",
  "zsh",
  "shell",
  "powershell",
  "ps1",
  "pwsh",
  "py",
  "python",
  "rb",
  "ruby",
  "yaml",
  "yml",
  "toml",
  "dockerfile",
  "makefile",
  "conf",
  "ini",
  "env",
]);

const CODE_KEYWORDS = new Set([
  "as",
  "async",
  "await",
  "break",
  "case",
  "catch",
  "class",
  "const",
  "continue",
  "crate",
  "def",
  "default",
  "defer",
  "delete",
  "do",
  "elif",
  "else",
  "enum",
  "Err",
  "except",
  "export",
  "extends",
  "extern",
  "false",
  "False",
  "final",
  "finally",
  "fn",
  "for",
  "from",
  "func",
  "function",
  "go",
  "if",
  "impl",
  "implements",
  "import",
  "in",
  "interface",
  "is",
  "lambda",
  "let",
  "loop",
  "match",
  "mod",
  "move",
  "mut",
  "namespace",
  "new",
  "nil",
  "None",
  "not",
  "null",
  "of",
  "Ok",
  "or",
  "override",
  "package",
  "pass",
  "private",
  "protected",
  "pub",
  "public",
  "raise",
  "readonly",
  "ref",
  "return",
  "select",
  "self",
  "Self",
  "Some",
  "static",
  "struct",
  "super",
  "switch",
  "then",
  "this",
  "throw",
  "trait",
  "true",
  "True",
  "try",
  "type",
  "typeof",
  "undefined",
  "unsafe",
  "use",
  "var",
  "void",
  "where",
  "while",
  "with",
  "yield",
]);

type TokenKind = "plain" | "comment" | "string" | "number" | "keyword";

const TOKEN_CLASS: Record<Exclude<TokenKind, "plain">, string> = {
  comment: "italic text-fg-3",
  string: "text-success-text",
  number: "text-warning-text",
  keyword: "font-medium text-action-text",
};

const isIdentStart = (ch: string) =>
  (ch >= "a" && ch <= "z") ||
  (ch >= "A" && ch <= "Z") ||
  ch === "_" ||
  ch === "$";

const isIdentPart = (ch: string) =>
  isIdentStart(ch) || (ch >= "0" && ch <= "9");

const isDigit = (ch: string) => ch >= "0" && ch <= "9";

/**
 * 零依赖轻量纯 TS 语法着色：仅识别注释、字符串、数字与关键字，
 * 保留全部原始字符不改动。
 */
const highlightCodeSyntax = (code: string, lang: string): ReactNode => {
  if (!code || code.length > TOKENIZE_MAX_CHARS) return code;
  const normalized = lang.toLowerCase();
  if (normalized === "text" || normalized === "plain" || normalized === "txt") {
    return code;
  }

  if (normalized === "diff" || normalized === "patch") {
    const lines = code.split("\n");
    return lines.map((line, index) => {
      const suffix = index < lines.length - 1 ? "\n" : "";
      if (line.startsWith("@@")) {
        return (
          <span key={index} className="text-action-text">
            {line}
            {suffix}
          </span>
        );
      }
      if (line.startsWith("+") && !line.startsWith("+++")) {
        return (
          <span key={index} className="bg-diff-add text-success-text">
            {line}
            {suffix}
          </span>
        );
      }
      if (line.startsWith("-") && !line.startsWith("---")) {
        return (
          <span key={index} className="bg-diff-del text-danger-text">
            {line}
            {suffix}
          </span>
        );
      }
      return (
        <span key={index}>
          {line}
          {suffix}
        </span>
      );
    });
  }

  const allowHashComment = HASH_COMMENT_LANGS.has(normalized);
  const allowSlashComment = !HASH_COMMENT_LANGS.has(normalized);
  const nodes: ReactNode[] = [];
  let plain = "";
  let pos = 0;
  let key = 0;

  const flushPlain = () => {
    if (!plain) return;
    nodes.push(plain);
    plain = "";
  };

  const pushToken = (kind: Exclude<TokenKind, "plain">, value: string) => {
    flushPlain();
    nodes.push(
      <span key={key++} className={TOKEN_CLASS[kind]}>
        {value}
      </span>,
    );
  };

  while (pos < code.length) {
    const ch = code[pos];
    const next = code[pos + 1];

    if (allowSlashComment && ch === "/" && next === "/") {
      let end = code.indexOf("\n", pos + 2);
      if (end < 0) end = code.length;
      pushToken("comment", code.slice(pos, end));
      pos = end;
      continue;
    }

    if (allowSlashComment && ch === "/" && next === "*") {
      let end = code.indexOf("*/", pos + 2);
      end = end < 0 ? code.length : end + 2;
      pushToken("comment", code.slice(pos, end));
      pos = end;
      continue;
    }

    if (allowHashComment && ch === "#") {
      let end = code.indexOf("\n", pos + 1);
      if (end < 0) end = code.length;
      pushToken("comment", code.slice(pos, end));
      pos = end;
      continue;
    }

    if (ch === '"' || ch === "'" || ch === "`") {
      if (
        ch === "'" &&
        next !== undefined &&
        isIdentStart(next) &&
        code[pos + 2] !== "'"
      ) {
        plain += ch;
        pos += 1;
        continue;
      }
      let end = pos + 1;
      while (end < code.length) {
        if (code[end] === "\\") {
          end += 2;
          continue;
        }
        if (code[end] === "\n" && ch !== "`") {
          break;
        }
        if (code[end] === ch) {
          end += 1;
          break;
        }
        end += 1;
      }
      pushToken("string", code.slice(pos, Math.min(end, code.length)));
      pos = Math.min(end, code.length);
      continue;
    }

    if (isDigit(ch) && (pos === 0 || !isIdentPart(code[pos - 1]))) {
      let end = pos + 1;
      if (
        ch === "0" &&
        (next === "x" || next === "X" || next === "b" || next === "o")
      ) {
        end += 1;
      }
      while (end < code.length && /[0-9a-fA-F_.]/.test(code[end])) {
        end += 1;
      }
      pushToken("number", code.slice(pos, end));
      pos = end;
      continue;
    }

    if (isIdentStart(ch)) {
      let end = pos + 1;
      while (end < code.length && isIdentPart(code[end])) {
        end += 1;
      }
      const word = code.slice(pos, end);
      let after = end;
      while (after < code.length && code[after] === " ") after += 1;
      if (CODE_KEYWORDS.has(word) && code[after] !== ":") {
        pushToken("keyword", word);
      } else {
        plain += word;
      }
      pos = end;
      continue;
    }

    plain += ch;
    pos += 1;
  }

  flushPlain();
  return nodes.length === 1 ? nodes[0] : nodes;
};

export interface SessionCodeBlockProps {
  /** 代码原文（不含围栏） */
  code: string;
  /** 围栏上的语言标记；为空时标签显示 `text` */
  language?: string;
  /** 搜索词：命中部分用 `<mark>` 高亮 */
  searchQuery?: string;
  /** 自定义复制行为；不传时写入剪贴板并弹 toast */
  onCopy?: (code: string) => void;
  className?: string;
}

/**
 * 会话阅读页的代码块：30px 顶栏显示语言标签与复制按钮，正文横向滚动。
 * 搜索词非空时优先 `<mark>` 高亮，否则使用轻量纯 TS 语法着色。
 */
export const SessionCodeBlock = memo(function SessionCodeBlock({
  code,
  language,
  searchQuery,
  onCopy,
  className,
}: SessionCodeBlockProps) {
  const { t } = useTranslation();
  const label = language?.trim() || "text";
  const trimmedQuery = searchQuery?.trim() ?? "";

  const handleCopy = useCallback(async () => {
    if (onCopy) {
      onCopy(code);
      return;
    }
    const ok = await copyText(code);
    if (ok) {
      toast.success(
        t("sessionManager.codeCopied", { defaultValue: "已复制代码" }),
      );
    } else {
      toast.error(t("sessionManager.copyFailed", { defaultValue: "复制失败" }));
    }
  }, [code, onCopy, t]);

  const body = useMemo<ReactNode>(() => {
    if (trimmedQuery) return highlightText(code, trimmedQuery);
    return highlightCodeSyntax(code, label);
  }, [code, label, trimmedQuery]);

  return (
    <div
      className={cn(
        "my-2 max-w-full overflow-hidden rounded-[8px]",
        SESSION_CODE_SURFACE,
        className,
      )}
    >
      <div className="flex h-[30px] items-center justify-between border-b border-border pe-1 ps-3">
        <span className="truncate font-mono text-caption text-fg-2">
          {label}
        </span>
        <button
          type="button"
          aria-label={t("sessionManager.copyCode", {
            defaultValue: "复制代码",
          })}
          onClick={() => void handleCopy()}
          className="h-6 shrink-0 rounded-control px-2 text-caption text-fg-2 transition-colors hover:bg-selected hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {t("sessionManager.copyShort", { defaultValue: "复制" })}
        </button>
      </div>
      <div
        tabIndex={0}
        role="region"
        aria-label={t("sessionManager.codeRegion", {
          defaultValue: "{{lang}} 代码",
          lang: label,
        })}
        className="overflow-x-auto px-3 py-2 outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
      >
        <pre className="m-0 whitespace-pre font-mono text-caption text-fg-1">
          <code data-language={language || undefined}>{body}</code>
        </pre>
      </div>
    </div>
  );
});
