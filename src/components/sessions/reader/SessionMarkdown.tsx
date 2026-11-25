import { ExternalLink as ExternalLinkIcon } from "lucide-react";
import {
  createContext,
  Fragment,
  memo,
  useContext,
  useMemo,
  useState,
  type MouseEvent,
  type ReactNode,
} from "react";
import { useTranslation } from "react-i18next";

import { sessionsApi } from "@/lib/api/sessions";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";
import { highlightQueryText as highlightText } from "../utils";
import { useReaderContext } from "./context";
import { fileUrlToPath, PathChip, shortenPath } from "./PathChip";
import {
  SESSION_INLINE_CODE_CLASS,
  SessionCodeBlock,
} from "./SessionCodeBlock";

/** 超过该长度不做 Markdown 解析，退化为「纯文本 + 围栏代码」（§7.1 单块 ≤ 64KB） */
export const MARKDOWN_PARSE_LIMIT = 65536;

export interface SessionMarkdownProps {
  content: string;
  searchQuery?: string;
  /** 会话项目目录：本地路径显示为相对路径 */
  projectDir?: string;
  /** body：最终回复正文；note：过程中的说明（小字、次级色） */
  variant?: "body" | "note";
  /** 打开外链（http/https/mailto）；不传时调用 `openSessionLink` */
  onOpenLink?: (url: string) => void;
  /** 「在文件管理器中定位」；不传时调用 `sessionsApi.revealPath` */
  onRevealPath?: (path: string) => void;
  /** 远程图片是否直接加载；默认 false，需用户点击后才发请求 */
  autoLoadRemoteImages?: boolean;
  /** 本地图片（`![](/abs/path.png)`）的渲染；不传时显示为路径 chip */
  renderLocalImage?: (path: string, alt: string) => ReactNode;
  className?: string;
}

// ---------------------------------------------------------------------------
// URL 与本地路径识别
// ---------------------------------------------------------------------------

const stripAngleBrackets = (value: string) => {
  const trimmed = value.trim();
  return trimmed.length > 1 && trimmed.startsWith("<") && trimmed.endsWith(">")
    ? trimmed.slice(1, -1).trim()
    : trimmed;
};

const safeExternalUrl = (value: string): string | null => {
  const candidate = stripAngleBrackets(value);
  const normalized = /^www\./i.test(candidate)
    ? `https://${candidate}`
    : /^[^\s@/:]+@[^\s@/:]+\.[^\s@/:]+$/.test(candidate)
      ? `mailto:${candidate}`
      : candidate;
  try {
    const parsed = new URL(normalized);
    if (
      parsed.protocol === "http:" ||
      parsed.protocol === "https:" ||
      parsed.protocol === "mailto:"
    ) {
      return normalized;
    }
    return null;
  } catch {
    return null;
  }
};

const safeRemoteImageUrl = (value: string): string | null => {
  const candidate = stripAngleBrackets(value);
  const normalized = /^www\./i.test(candidate)
    ? `https://${candidate}`
    : candidate;
  try {
    const url = new URL(normalized);
    return url.protocol === "http:" || url.protocol === "https:"
      ? normalized
      : null;
  } catch {
    return null;
  }
};

/** 链接/图片目标里的本地路径：`file://`、绝对路径或 `./` `../` 相对路径 */
const localPathTarget = (value: string): string | null => {
  const target = stripAngleBrackets(value);
  if (!target || /\s/.test(target)) return null;
  if (/^file:\/\//i.test(target)) {
    const path = fileUrlToPath(target);
    return path || null;
  }
  if (
    /^(\/(?!\/)\S*|~[\\/]\S*|[A-Za-z]:[\\/]\S*|\.\.?[\\/]\S*|\\\\[^\\/]+[\\/]\S*)$/.test(
      target,
    )
  ) {
    return target;
  }
  return null;
};

const KNOWN_PATH_EXT_RE =
  /\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs|json|jsonc|jsonl|rs|py|go|java|kt|swift|c|cc|cpp|cxx|h|hpp|cs|rb|php|lua|zig|sh|bash|zsh|ps1|psm1|bat|cmd|toml|yaml|yml|ini|conf|cfg|env|lock|md|mdx|txt|log|csv|tsv|sql|sqlite|db|css|scss|less|html|htm|xml|svg|vue|svelte|astro|png|jpe?g|gif|webp|ico|pdf|wasm)(?::\d+(?::\d+)?|#L\d+(?:-L?\d+)?)?$/i;

/**
 * 行内反引号代码是否为本地文件路径：
 * 支持 `/abs/foo.ts`、`C:\foo\bar.ts`、`~/foo.ts`、`./foo.ts`、`src/foo.ts:42` 等。
 */
export const isInlineCodeLocalPath = (raw: string): boolean => {
  const trimmed = raw.trim();
  if (
    !trimmed ||
    trimmed.length > 260 ||
    /\s/.test(trimmed) ||
    /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(trimmed)
  ) {
    return false;
  }
  if (/^[A-Za-z]:[\\/][^*?"<>|]+$/.test(trimmed)) return true;
  if (/^\\\\[^\\/:*?"<>|]+\\[^*?"<>|]+$/.test(trimmed)) return true;
  if (/^~[\\/][^*?"<>|]+$/.test(trimmed)) return true;
  if (/^\.\.?[\\/][^*?"<>|]+$/.test(trimmed)) return true;
  if (trimmed.startsWith("/") && !trimmed.startsWith("//")) {
    if (/[*?"<>|]/.test(trimmed)) return false;
    const segments = trimmed.split("/").filter(Boolean);
    return segments.length >= 2 || KNOWN_PATH_EXT_RE.test(trimmed);
  }
  if (
    (trimmed.includes("/") || trimmed.includes("\\")) &&
    !/[*?"<>|]/.test(trimmed) &&
    !/^[#@]/.test(trimmed) &&
    KNOWN_PATH_EXT_RE.test(trimmed)
  ) {
    return true;
  }
  return false;
};

type LinkTarget =
  | { kind: "external"; url: string }
  | { kind: "path"; path: string };

const resolveLinkTargetValue = (value: string): LinkTarget | null => {
  const url = safeExternalUrl(value);
  if (url) return { kind: "external", url };
  const path = localPathTarget(value);
  if (path) return { kind: "path", path };
  const stripped = stripAngleBrackets(value);
  if (isInlineCodeLocalPath(stripped)) {
    return { kind: "path", path: stripped };
  }
  return null;
};

type ImageTarget =
  | { kind: "remote"; src: string }
  | { kind: "path"; path: string };

const resolveImageTargetValue = (value: string): ImageTarget | null => {
  const src = safeRemoteImageUrl(value);
  if (src) return { kind: "remote", src };
  const path = localPathTarget(value);
  return path ? { kind: "path", path } : null;
};

const renderText = (text: string, searchQuery?: string): ReactNode =>
  searchQuery ? highlightText(text, searchQuery) : text;

// ---------------------------------------------------------------------------
// 纯文本 + 围栏代码（决策 D7：用户提问与超长 Markdown 共用，严禁解析行内记号）
// ---------------------------------------------------------------------------

export type PlainTextBlock =
  | { type: "text"; text: string }
  | { type: "code"; language: string; code: string };

/** 把内容切成纯文本段与围栏代码段；未闭合的围栏一直延续到末尾 */
export const splitFencedCode = (content: string): PlainTextBlock[] => {
  const blocks: PlainTextBlock[] = [];
  const lines = content.replace(/\r\n/g, "\n").split("\n");
  let text: string[] = [];
  let code: string[] | null = null;
  let language = "";
  let fence = "";

  const pushText = () => {
    const value = text.join("\n").replace(/^\n+|\n+$/g, "");
    if (value.trim()) blocks.push({ type: "text", text: value });
    text = [];
  };

  for (const line of lines) {
    if (code === null) {
      const open = /^ {0,3}(`{3,}|~{3,})\s*([^\s`]*)/.exec(line);
      if (open) {
        pushText();
        code = [];
        fence = open[1];
        language = open[2] ?? "";
      } else {
        text.push(line);
      }
      continue;
    }

    const trimmed = line.trim();
    if (
      trimmed.length >= fence.length &&
      trimmed === fence[0].repeat(trimmed.length)
    ) {
      blocks.push({ type: "code", language, code: code.join("\n") });
      code = null;
      continue;
    }
    code.push(line);
  }

  if (code !== null) {
    blocks.push({ type: "code", language, code: code.join("\n") });
  }
  pushText();
  return blocks;
};

/** 折叠预览：截到 maxLength（保留代理对），若截断处在未闭合的围栏内则补上闭合围栏 */
export const createCollapsedMarkdownPreview = (
  content: string,
  maxLength: number,
): string => {
  let limit = Math.min(maxLength, content.length);
  if (
    limit > 0 &&
    limit < content.length &&
    content.codePointAt(limit - 1)! > 0xffff
  ) {
    limit -= 1;
  }
  const preview = content.slice(0, limit);
  const lines = preview.replace(/\r\n/g, "\n").split("\n");
  let openFence: { fence: string; prefix: string } | null = null;

  for (const line of lines) {
    const strippedQuote = line.replace(/^(?: {0,3}>\s?)+/, "");
    const quotePrefixLen = line.length - strippedQuote.length;
    const quotePrefix = line.slice(0, quotePrefixLen).replace(/[^>\s]/g, " ");

    if (!openFence) {
      const open = /^( {0,3})(`{3,}|~{3,})/.exec(strippedQuote);
      if (open) {
        openFence = {
          fence: open[2],
          prefix: `${quotePrefix}${open[1]}`,
        };
      }
    } else {
      const trimmed = strippedQuote.trim();
      if (
        trimmed.length >= openFence.fence.length &&
        trimmed === openFence.fence[0].repeat(trimmed.length)
      ) {
        openFence = null;
      }
    }
  }

  return openFence
    ? `${preview}\n${openFence.prefix}${openFence.fence}\n\n…`
    : `${preview}…`;
};

export interface SessionPlainTextProps {
  content: string;
  searchQuery?: string;
  className?: string;
}

/**
 * 纯文本 + 围栏代码：用户提问（决策 D7）与超长 Markdown 的退化都用它，
 * 不解析任何 `#`、`*`、`_`、`<tag>` 或链接语法。
 */
export const SessionPlainText = memo(function SessionPlainText({
  content,
  searchQuery,
  className,
}: SessionPlainTextProps) {
  const blocks = useMemo(() => splitFencedCode(content), [content]);

  return (
    <div
      className={cn(
        "min-w-0 break-words text-body text-fg-1 [overflow-wrap:anywhere]",
        className,
      )}
    >
      {blocks.map((block, index) =>
        block.type === "code" ? (
          <SessionCodeBlock
            key={index}
            code={block.code}
            language={block.language}
            searchQuery={searchQuery}
          />
        ) : (
          <p
            key={index}
            className="my-1.5 whitespace-pre-wrap first:mt-0 last:mb-0"
          >
            {renderText(block.text, searchQuery)}
          </p>
        ),
      )}
    </div>
  );
});

// ---------------------------------------------------------------------------
// 外部链接打开与交互 Context
// ---------------------------------------------------------------------------

/** 外链安全打开：仅允许 http: / https: / mailto: 协议，交由后端 `open_external_url` 打开 */
export const openSessionLink = async (url: string): Promise<void> => {
  const safe = safeExternalUrl(url);
  if (!safe) {
    throw new Error("仅允许打开 HTTP / HTTPS / mailto 链接");
  }
  await sessionsApi.openExternalUrl(safe);
};

interface MarkdownActions {
  openLink: (url: string) => void;
  revealPath?: (path: string) => void;
  autoLoadRemoteImages: boolean;
  renderLocalImage?: (path: string, alt: string) => ReactNode;
}

const defaultOpenLink = (url: string) => {
  openSessionLink(url).catch((error: unknown) => {
    toast.error(extractErrorMessage(error));
  });
};

const MarkdownActionsContext = createContext<MarkdownActions>({
  openLink: defaultOpenLink,
  autoLoadRemoteImages: false,
});

const LINK_CLASS =
  "rounded-[2px] text-action-text underline-offset-[3px] hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring";

const ExternalLink = ({
  url,
  children,
}: {
  url: string;
  children: ReactNode;
}) => {
  const { openLink } = useContext(MarkdownActionsContext);
  const isWeb = /^https?:/i.test(url);

  const handleClick = (event: MouseEvent<HTMLAnchorElement>) => {
    event.preventDefault();
    openLink(url);
  };

  return (
    <a
      href={url}
      title={url}
      rel="noopener noreferrer"
      onClick={handleClick}
      onAuxClick={(event) => event.preventDefault()}
      className={LINK_CLASS}
    >
      {children}
      {isWeb && (
        <ExternalLinkIcon
          aria-hidden
          className="ms-0.5 inline size-3 align-[-1px] opacity-75"
        />
      )}
    </a>
  );
};

const MarkdownPathChip = (props: {
  path: string;
  projectDir?: string;
  label?: ReactNode;
  searchQuery?: string;
}) => {
  const { revealPath } = useContext(MarkdownActionsContext);
  return <PathChip {...props} onReveal={revealPath} />;
};

const MarkdownLocalImage = (props: {
  path: string;
  alt: string;
  projectDir?: string;
  searchQuery?: string;
}) => {
  const { renderLocalImage } = useContext(MarkdownActionsContext);
  if (renderLocalImage) return <>{renderLocalImage(props.path, props.alt)}</>;
  return (
    <MarkdownPathChip
      path={props.path}
      projectDir={props.projectDir}
      searchQuery={props.searchQuery}
    />
  );
};

const RemoteImage = ({
  src,
  alt,
  searchQuery,
}: {
  src: string;
  alt: string;
  searchQuery?: string;
}) => {
  const { t } = useTranslation();
  const { openLink, autoLoadRemoteImages } = useContext(MarkdownActionsContext);
  const [failedSrc, setFailedSrc] = useState<string | null>(null);

  const label =
    alt ||
    t("sessionManager.remoteImage", {
      defaultValue: "远程图片",
    });

  if (autoLoadRemoteImages && failedSrc !== src) {
    return (
      <img
        src={src}
        alt={alt}
        title={src}
        loading="lazy"
        decoding="async"
        referrerPolicy="no-referrer"
        onError={() => setFailedSrc(src)}
        className="my-2 block max-h-80 max-w-full rounded-[8px] border border-border object-contain"
      />
    );
  }

  const openLabel = t("sessionManager.reader.image.openRemote", {
    defaultValue: "在浏览器中打开远程图片",
  });
  return (
    <button
      type="button"
      title={src}
      aria-label={`${openLabel}: ${label}`}
      onClick={() => openLink(src)}
      className="my-2 inline-flex max-w-full items-center gap-2 rounded-[8px] border border-dashed border-border-strong bg-subtle px-3 py-2 text-left text-caption text-fg-2 transition-colors hover:border-action-text/40 hover:text-fg-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      <span className="min-w-0 truncate font-medium text-action-text">
        {renderText(label, searchQuery)}
      </span>
      <span className="shrink-0 text-fg-3">{openLabel}</span>
      <ExternalLinkIcon aria-hidden className="size-3 shrink-0 text-fg-3" />
    </button>
  );
};

// ---------------------------------------------------------------------------
// 零依赖 GFM 块级与行内解析器
// ---------------------------------------------------------------------------

type TableAlign = "left" | "center" | "right" | null;

interface ListItemNode {
  text: string;
  checked?: boolean;
}

type GfmBlock =
  | { kind: "code"; language: string; code: string }
  | { kind: "heading"; level: number; text: string }
  | { kind: "hr" }
  | { kind: "quote"; blocks: GfmBlock[] }
  | { kind: "list"; ordered: boolean; start: number; items: ListItemNode[] }
  | {
      kind: "table";
      headers: string[];
      aligns: TableAlign[];
      rows: string[][];
    }
  | { kind: "paragraph"; text: string };

type LinkReferences = ReadonlyMap<string, string>;

const FENCE_OPEN_RE = /^ {0,3}(`{3,}|~{3,})(.*)$/;
const HEADING_RE = /^ {0,3}(#{1,6})\s+(.+?)\s*#*$/;
const HR_RE = /^ {0,3}(?:-\s*-\s*-[-\s]*|\*\s*\*\s*\*[\*\s]*|_\s*_\s*_[_\s]*)$/;
const QUOTE_RE = /^ {0,3}>\s?(.*)$/;
const UNORDERED_ITEM_RE = /^ {0,3}[-*+]\s+(.*)$/;
const ORDERED_ITEM_RE = /^ {0,3}(\d+)[.)]\s+(.*)$/;
const TASK_MARKER_RE = /^\[([ xX])\]\s+([\s\S]*)$/;
const LINK_REF_DEF_RE =
  /^ {0,3}\[([^\]]+)\]:\s*(<[^>\n]+>|\S+)(?:\s+(?:"[^"\n]*"|'[^'\n]*'|\([^)\n]*\)))?\s*$/;
const TABLE_SEP_CELL_RE = /^:?-{3,}:?$/;

const normalizeRefKey = (label: string) =>
  label.trim().replace(/\s+/g, " ").toLowerCase();

const splitTableRow = (line: string): string[] | null => {
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
};

const parseTableSeparator = (
  line: string,
  expectedCols: number,
): TableAlign[] | null => {
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
};

const parseGfmDocument = (
  content: string,
): { blocks: GfmBlock[]; references: LinkReferences } => {
  const rawLines = content.replace(/\r\n?/g, "\n").split("\n");
  const references = new Map<string, string>();
  const filteredLines: string[] = [];

  let fenceState: string | null = null;
  for (const line of rawLines) {
    if (fenceState === null) {
      const open = FENCE_OPEN_RE.exec(line);
      if (open) {
        fenceState = open[1];
        filteredLines.push(line);
        continue;
      }
      const refMatch = LINK_REF_DEF_RE.exec(line);
      if (refMatch) {
        const key = normalizeRefKey(refMatch[1]);
        const target = refMatch[2];
        if (
          key &&
          !references.has(key) &&
          (resolveLinkTargetValue(target) || resolveImageTargetValue(target))
        ) {
          references.set(key, target);
          continue;
        }
      }
      filteredLines.push(line);
    } else {
      const trimmed = line.trim();
      if (
        trimmed.length >= fenceState.length &&
        trimmed === fenceState[0].repeat(trimmed.length)
      ) {
        fenceState = null;
      }
      filteredLines.push(line);
    }
  }

  const parseBlockLines = (lines: string[]): GfmBlock[] => {
    const blocks: GfmBlock[] = [];
    let index = 0;

    const startsSpecialBlock = (at: number): boolean => {
      const current = lines[at];
      if (!current.trim()) return true;
      if (
        FENCE_OPEN_RE.test(current) ||
        HR_RE.test(current) ||
        HEADING_RE.test(current) ||
        QUOTE_RE.test(current) ||
        UNORDERED_ITEM_RE.test(current) ||
        ORDERED_ITEM_RE.test(current)
      ) {
        return true;
      }
      if (at + 1 < lines.length) {
        const headerCells = splitTableRow(current);
        if (
          headerCells &&
          parseTableSeparator(lines[at + 1], headerCells.length)
        ) {
          return true;
        }
      }
      return false;
    };

    while (index < lines.length) {
      const line = lines[index];
      if (!line.trim()) {
        index += 1;
        continue;
      }

      const fenceMatch = FENCE_OPEN_RE.exec(line);
      if (fenceMatch) {
        const marker = fenceMatch[1];
        const language = fenceMatch[2].trim().split(/\s+/)[0] ?? "";
        const codeLines: string[] = [];
        index += 1;
        while (index < lines.length) {
          const trimmed = lines[index].trim();
          if (
            trimmed.length >= marker.length &&
            trimmed === marker[0].repeat(trimmed.length)
          ) {
            index += 1;
            break;
          }
          codeLines.push(lines[index]);
          index += 1;
        }
        blocks.push({ kind: "code", language, code: codeLines.join("\n") });
        continue;
      }

      if (HR_RE.test(line)) {
        blocks.push({ kind: "hr" });
        index += 1;
        continue;
      }

      const headingMatch = HEADING_RE.exec(line);
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
          const match = QUOTE_RE.exec(lines[index]);
          if (!match) break;
          quoteLines.push(match[1]);
          index += 1;
        }
        blocks.push({ kind: "quote", blocks: parseBlockLines(quoteLines) });
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
            rows.push(
              Array.from(
                { length: headerCells.length },
                (_, col) => rowCells[col] ?? "",
              ),
            );
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

      const unorderedFirst = UNORDERED_ITEM_RE.exec(line);
      if (unorderedFirst) {
        const items: ListItemNode[] = [];
        while (index < lines.length) {
          const match = UNORDERED_ITEM_RE.exec(lines[index]);
          if (!match) break;
          const itemLines = [match[1]];
          index += 1;
          while (
            index < lines.length &&
            lines[index].trim() !== "" &&
            /^\s{2,}\S/.test(lines[index]) &&
            !UNORDERED_ITEM_RE.test(lines[index]) &&
            !ORDERED_ITEM_RE.test(lines[index]) &&
            !FENCE_OPEN_RE.test(lines[index])
          ) {
            itemLines.push(lines[index].trim());
            index += 1;
          }
          const combined = itemLines.join("\n");
          const task = TASK_MARKER_RE.exec(combined);
          if (task) {
            items.push({
              text: task[2],
              checked: task[1].toLowerCase() === "x",
            });
          } else {
            items.push({ text: combined });
          }
        }
        blocks.push({ kind: "list", ordered: false, start: 1, items });
        continue;
      }

      const orderedFirst = ORDERED_ITEM_RE.exec(line);
      if (orderedFirst) {
        const start = Number.parseInt(orderedFirst[1], 10);
        const items: ListItemNode[] = [];
        while (index < lines.length) {
          const match = ORDERED_ITEM_RE.exec(lines[index]);
          if (!match) break;
          const itemLines = [match[2]];
          index += 1;
          while (
            index < lines.length &&
            lines[index].trim() !== "" &&
            /^\s{2,}\S/.test(lines[index]) &&
            !UNORDERED_ITEM_RE.test(lines[index]) &&
            !ORDERED_ITEM_RE.test(lines[index]) &&
            !FENCE_OPEN_RE.test(lines[index])
          ) {
            itemLines.push(lines[index].trim());
            index += 1;
          }
          items.push({ text: itemLines.join("\n") });
        }
        blocks.push({
          kind: "list",
          ordered: true,
          start: Number.isNaN(start) ? 1 : start,
          items,
        });
        continue;
      }

      const paragraphLines: string[] = [line];
      index += 1;
      while (index < lines.length && !startsSpecialBlock(index)) {
        paragraphLines.push(lines[index]);
        index += 1;
      }
      blocks.push({ kind: "paragraph", text: paragraphLines.join("\n") });
    }

    return blocks;
  };

  return {
    blocks: parseBlockLines(filteredLines),
    references,
  };
};

// ---------------------------------------------------------------------------
// 行内语法解析
// ---------------------------------------------------------------------------

interface InlineContext {
  searchQuery?: string;
  projectDir?: string;
  references: LinkReferences;
}

const matchInlineBackticks = (
  text: string,
  pos: number,
): { code: string; nextPos: number } | { unmatchedEnd: number } => {
  let openEnd = pos;
  while (openEnd < text.length && text[openEnd] === "`") openEnd += 1;
  const tickLen = openEnd - pos;
  let cursor = openEnd;
  while (cursor < text.length) {
    if (text[cursor] === "`") {
      let closeEnd = cursor;
      while (closeEnd < text.length && text[closeEnd] === "`") closeEnd += 1;
      if (closeEnd - cursor === tickLen) {
        const raw = text.slice(openEnd, cursor).replace(/\n/g, " ");
        const normalized =
          raw.length >= 2 &&
          raw.startsWith(" ") &&
          raw.endsWith(" ") &&
          raw.trim().length > 0
            ? raw.slice(1, -1)
            : raw;
        return { code: normalized, nextPos: closeEnd };
      }
      cursor = closeEnd;
      continue;
    }
    cursor += 1;
  }
  return { unmatchedEnd: openEnd };
};

const matchBracketConstruct = (
  text: string,
  pos: number,
  references: LinkReferences,
): { label: string; rawTarget: string; nextPos: number } | null => {
  if (text[pos] !== "[") return null;
  let depth = 1;
  let closeBracket = pos + 1;
  while (closeBracket < text.length && depth > 0) {
    const ch = text[closeBracket];
    if (ch === "\\") {
      closeBracket += 2;
      continue;
    }
    if (ch === "`") {
      const codeMatch = matchInlineBackticks(text, closeBracket);
      closeBracket =
        "nextPos" in codeMatch ? codeMatch.nextPos : codeMatch.unmatchedEnd;
      continue;
    }
    if (ch === "\n") return null;
    if (ch === "[") depth += 1;
    else if (ch === "]") depth -= 1;
    if (depth === 0) break;
    closeBracket += 1;
  }
  if (depth !== 0 || closeBracket >= text.length) return null;

  const label = text.slice(pos + 1, closeBracket);
  const after = closeBracket + 1;

  if (text[after] === "(") {
    let cursor = after + 1;
    while (
      cursor < text.length &&
      /\s/.test(text[cursor]) &&
      text[cursor] !== "\n"
    ) {
      cursor += 1;
    }
    let rawTarget = "";
    if (text[cursor] === "<") {
      const endAngle = text.indexOf(">", cursor + 1);
      if (endAngle < 0 || text.slice(cursor + 1, endAngle).includes("\n")) {
        return null;
      }
      rawTarget = text.slice(cursor, endAngle + 1);
      cursor = endAngle + 1;
    } else {
      const startUrl = cursor;
      let parenDepth = 0;
      while (cursor < text.length) {
        const ch = text[cursor];
        if (ch === "\n" || /\s/.test(ch)) break;
        if (ch === "(") parenDepth += 1;
        else if (ch === ")") {
          if (parenDepth === 0) break;
          parenDepth -= 1;
        }
        cursor += 1;
      }
      rawTarget = text.slice(startUrl, cursor);
    }
    while (
      cursor < text.length &&
      (text[cursor] === " " || text[cursor] === "\t")
    ) {
      cursor += 1;
    }
    if (text[cursor] === '"' || text[cursor] === "'") {
      const quote = text[cursor];
      const endQuote = text.indexOf(quote, cursor + 1);
      if (endQuote < 0 || text.slice(cursor + 1, endQuote).includes("\n")) {
        return null;
      }
      cursor = endQuote + 1;
      while (
        cursor < text.length &&
        (text[cursor] === " " || text[cursor] === "\t")
      ) {
        cursor += 1;
      }
    }
    if (text[cursor] !== ")" || !rawTarget) return null;
    return { label, rawTarget, nextPos: cursor + 1 };
  }

  if (text[after] === "[") {
    const closeRef = text.indexOf("]", after + 1);
    if (closeRef > 0 && !text.slice(after + 1, closeRef).includes("\n")) {
      const explicitRef = text.slice(after + 1, closeRef);
      const key = normalizeRefKey(explicitRef || label);
      const target = references.get(key);
      if (target) {
        return { label, rawTarget: target, nextPos: closeRef + 1 };
      }
    }
  }

  const shortcutTarget = references.get(normalizeRefKey(label));
  if (shortcutTarget) {
    return { label, rawTarget: shortcutTarget, nextPos: after };
  }

  return null;
};

const isWordChar = (ch: string | undefined) =>
  ch !== undefined && /[0-9A-Za-z_\u00c0-\u024f\u4e00-\u9fff]/.test(ch);

const BARE_URL_RE = /^https?:\/\/[^\s<>\])"']+/i;

const trimTrailingUrlPunct = (url: string): string => {
  let end = url.length;
  while (end > 0) {
    const ch = url[end - 1];
    if (/[.,;:!?]/.test(ch)) {
      end -= 1;
      continue;
    }
    if (ch === ")") {
      const candidate = url.slice(0, end);
      const opens = (candidate.match(/\(/g) ?? []).length;
      const closes = (candidate.match(/\)/g) ?? []).length;
      if (closes > opens) {
        end -= 1;
        continue;
      }
    }
    break;
  }
  return url.slice(0, end);
};

const renderInline = (
  text: string,
  ctx: InlineContext,
  allowLinks = true,
): ReactNode => {
  const { searchQuery, projectDir, references } = ctx;
  const nodes: ReactNode[] = [];
  let plain = "";
  let pos = 0;
  let key = 0;

  const flushPlain = () => {
    if (!plain) return;
    nodes.push(
      <Fragment key={`t-${key++}`}>{renderText(plain, searchQuery)}</Fragment>,
    );
    plain = "";
  };

  while (pos < text.length) {
    const ch = text[pos];

    if (
      (ch === "\\" && text[pos + 1] === "\n") ||
      (ch === " " && text[pos + 1] === " " && text[pos + 2] === "\n")
    ) {
      const step = ch === "\\" ? 2 : 3;
      flushPlain();
      nodes.push(<br key={`br-${key++}`} />);
      pos += step;
      continue;
    }
    if (ch === "\n") {
      if (plain.endsWith("  ")) {
        plain = plain.replace(/ +$/, "");
      }
      flushPlain();
      nodes.push(<br key={`nl-${key++}`} />);
      pos += 1;
      continue;
    }

    if (
      ch === "\\" &&
      pos + 1 < text.length &&
      /[\\`*_{}[\]()#+\-.!|~<>]/.test(text[pos + 1])
    ) {
      plain += text[pos + 1];
      pos += 2;
      continue;
    }

    if (ch === "`") {
      const matched = matchInlineBackticks(text, pos);
      if ("code" in matched) {
        flushPlain();
        if (allowLinks && isInlineCodeLocalPath(matched.code)) {
          nodes.push(
            <MarkdownPathChip
              key={`pc-${key++}`}
              path={matched.code.trim()}
              projectDir={projectDir}
              searchQuery={searchQuery}
            />,
          );
        } else {
          nodes.push(
            <code key={`c-${key++}`} className={SESSION_INLINE_CODE_CLASS}>
              {renderText(matched.code, searchQuery)}
            </code>,
          );
        }
        pos = matched.nextPos;
        continue;
      }
      plain += text.slice(pos, matched.unmatchedEnd);
      pos = matched.unmatchedEnd;
      continue;
    }

    if (allowLinks && ch === "!" && text[pos + 1] === "[") {
      const imgMatch = matchBracketConstruct(text, pos + 1, references);
      if (imgMatch) {
        const target = resolveImageTargetValue(imgMatch.rawTarget);
        if (target) {
          flushPlain();
          if (target.kind === "path") {
            nodes.push(
              <MarkdownLocalImage
                key={`li-${key++}`}
                path={target.path}
                alt={imgMatch.label}
                projectDir={projectDir}
                searchQuery={searchQuery}
              />,
            );
          } else {
            nodes.push(
              <RemoteImage
                key={`ri-${key++}`}
                src={target.src}
                alt={imgMatch.label}
                searchQuery={searchQuery}
              />,
            );
          }
          pos = imgMatch.nextPos;
          continue;
        }
      }
    }

    if (allowLinks && ch === "[") {
      const linkMatch = matchBracketConstruct(text, pos, references);
      if (linkMatch && linkMatch.label.trim().length > 0) {
        const target = resolveLinkTargetValue(linkMatch.rawTarget);
        if (target) {
          flushPlain();
          const labelContent = renderInline(linkMatch.label, ctx, false);
          if (target.kind === "path") {
            nodes.push(
              <MarkdownPathChip
                key={`lp-${key++}`}
                path={target.path}
                projectDir={projectDir}
                searchQuery={searchQuery}
                label={labelContent}
              />,
            );
          } else {
            nodes.push(
              <ExternalLink key={`el-${key++}`} url={target.url}>
                {labelContent}
              </ExternalLink>,
            );
          }
          pos = linkMatch.nextPos;
          continue;
        }
      }
    }

    if (allowLinks && ch === "<") {
      const closeAngle = text.indexOf(">", pos + 1);
      if (closeAngle > pos + 1) {
        const inner = text.slice(pos + 1, closeAngle);
        if (!/\s/.test(inner)) {
          const target = resolveLinkTargetValue(inner);
          if (target) {
            flushPlain();
            if (target.kind === "path") {
              nodes.push(
                <MarkdownPathChip
                  key={`ap-${key++}`}
                  path={target.path}
                  projectDir={projectDir}
                  searchQuery={searchQuery}
                />,
              );
            } else {
              nodes.push(
                <ExternalLink key={`al-${key++}`} url={target.url}>
                  {renderText(inner, searchQuery)}
                </ExternalLink>,
              );
            }
            pos = closeAngle + 1;
            continue;
          }
        }
      }
    }

    if (
      allowLinks &&
      (text.startsWith("https://", pos) || text.startsWith("http://", pos)) &&
      (pos === 0 || /[\s([<{"'：，。]/.test(text[pos - 1]))
    ) {
      const rawMatch = BARE_URL_RE.exec(text.slice(pos));
      if (rawMatch) {
        const candidate = trimTrailingUrlPunct(rawMatch[0]);
        const safe = safeExternalUrl(candidate);
        if (safe) {
          flushPlain();
          nodes.push(
            <ExternalLink key={`bu-${key++}`} url={safe}>
              {renderText(candidate, searchQuery)}
            </ExternalLink>,
          );
          pos += candidate.length;
          continue;
        }
      }
    }

    if (
      text.startsWith("**", pos) ||
      (text.startsWith("__", pos) && !isWordChar(text[pos - 1]))
    ) {
      const marker = text.slice(pos, pos + 2);
      const close = text.indexOf(marker, pos + 2);
      if (
        close > pos + 2 &&
        !text.slice(pos + 2, close).includes("\n") &&
        (marker === "**" || !isWordChar(text[close + 2]))
      ) {
        flushPlain();
        nodes.push(
          <strong key={`b-${key++}`} className="font-semibold">
            {renderInline(text.slice(pos + 2, close), ctx, allowLinks)}
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
          <del key={`d-${key++}`} className="text-fg-2">
            {renderInline(text.slice(pos + 2, close), ctx, allowLinks)}
          </del>,
        );
        pos = close + 2;
        continue;
      }
    }

    if (
      (ch === "*" &&
        text[pos + 1] !== "*" &&
        text[pos + 1] !== undefined &&
        !/\s/.test(text[pos + 1])) ||
      (ch === "_" &&
        text[pos + 1] !== "_" &&
        text[pos + 1] !== undefined &&
        !/\s/.test(text[pos + 1]) &&
        !isWordChar(text[pos - 1]))
    ) {
      let close = pos + 2;
      let found = -1;
      while (close < text.length && text[close] !== "\n") {
        if (
          text[close] === ch &&
          text[close - 1] !== ch &&
          text[close + 1] !== ch &&
          !/\s/.test(text[close - 1]) &&
          (ch === "*" || !isWordChar(text[close + 1]))
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
            {renderInline(text.slice(pos + 1, found), ctx, allowLinks)}
          </em>,
        );
        pos = found + 1;
        continue;
      }
    }

    plain += ch;
    pos += 1;
  }

  flushPlain();
  return nodes.length === 1 ? nodes[0] : nodes;
};

const collectInlinePieces = (
  text: string,
  ctx: InlineContext,
  pieces: string[],
  allowLinks = true,
) => {
  const { projectDir, references } = ctx;
  let plain = "";
  let pos = 0;
  const flush = () => {
    if (plain) {
      pieces.push(plain);
      plain = "";
    }
  };

  while (pos < text.length) {
    const ch = text[pos];
    if (
      ch === "\\" &&
      pos + 1 < text.length &&
      /[\\`*_{}[\]()#+\-.!|~<>]/.test(text[pos + 1])
    ) {
      plain += text[pos + 1];
      pos += 2;
      continue;
    }
    if (ch === "`") {
      const matched = matchInlineBackticks(text, pos);
      if ("code" in matched) {
        flush();
        pieces.push(
          allowLinks && isInlineCodeLocalPath(matched.code)
            ? shortenPath(matched.code.trim(), projectDir)
            : matched.code,
        );
        pos = matched.nextPos;
        continue;
      }
      plain += text.slice(pos, matched.unmatchedEnd);
      pos = matched.unmatchedEnd;
      continue;
    }
    if (allowLinks && ch === "!" && text[pos + 1] === "[") {
      const imgMatch = matchBracketConstruct(text, pos + 1, references);
      if (imgMatch) {
        const target = resolveImageTargetValue(imgMatch.rawTarget);
        if (target) {
          flush();
          pieces.push(
            target.kind === "path"
              ? shortenPath(target.path, projectDir)
              : imgMatch.label,
          );
          pos = imgMatch.nextPos;
          continue;
        }
      }
    }
    if (allowLinks && ch === "[") {
      const linkMatch = matchBracketConstruct(text, pos, references);
      if (linkMatch && linkMatch.label.trim().length > 0) {
        const target = resolveLinkTargetValue(linkMatch.rawTarget);
        if (target) {
          flush();
          collectInlinePieces(linkMatch.label, ctx, pieces, false);
          pos = linkMatch.nextPos;
          continue;
        }
      }
    }
    if (
      text.startsWith("**", pos) ||
      (text.startsWith("__", pos) && !isWordChar(text[pos - 1]))
    ) {
      const marker = text.slice(pos, pos + 2);
      const close = text.indexOf(marker, pos + 2);
      if (close > pos + 2 && !text.slice(pos + 2, close).includes("\n")) {
        flush();
        collectInlinePieces(
          text.slice(pos + 2, close),
          ctx,
          pieces,
          allowLinks,
        );
        pos = close + 2;
        continue;
      }
    }
    if (text.startsWith("~~", pos)) {
      const close = text.indexOf("~~", pos + 2);
      if (close > pos + 2 && !text.slice(pos + 2, close).includes("\n")) {
        flush();
        collectInlinePieces(
          text.slice(pos + 2, close),
          ctx,
          pieces,
          allowLinks,
        );
        pos = close + 2;
        continue;
      }
    }
    if (
      (ch === "*" &&
        text[pos + 1] !== "*" &&
        text[pos + 1] !== undefined &&
        !/\s/.test(text[pos + 1])) ||
      (ch === "_" &&
        text[pos + 1] !== "_" &&
        text[pos + 1] !== undefined &&
        !/\s/.test(text[pos + 1]) &&
        !isWordChar(text[pos - 1]))
    ) {
      let close = pos + 2;
      let found = -1;
      while (close < text.length && text[close] !== "\n") {
        if (
          text[close] === ch &&
          text[close - 1] !== ch &&
          text[close + 1] !== ch &&
          !/\s/.test(text[close - 1]) &&
          (ch === "*" || !isWordChar(text[close + 1]))
        ) {
          found = close;
          break;
        }
        close += 1;
      }
      if (found > pos + 1) {
        flush();
        collectInlinePieces(
          text.slice(pos + 1, found),
          ctx,
          pieces,
          allowLinks,
        );
        pos = found + 1;
        continue;
      }
    }
    plain += ch;
    pos += 1;
  }
  flush();
};

const collectBlockPieces = (
  blocks: GfmBlock[],
  ctx: InlineContext,
  pieces: string[],
) => {
  for (const block of blocks) {
    switch (block.kind) {
      case "code":
        pieces.push(block.code);
        break;
      case "heading":
      case "paragraph":
        collectInlinePieces(block.text, ctx, pieces);
        break;
      case "quote":
        collectBlockPieces(block.blocks, ctx, pieces);
        break;
      case "list":
        block.items.forEach((item) =>
          collectInlinePieces(item.text, ctx, pieces),
        );
        break;
      case "table":
        block.headers.forEach((header) =>
          collectInlinePieces(header, ctx, pieces),
        );
        block.rows.forEach((row) =>
          row.forEach((cell) => collectInlinePieces(cell, ctx, pieces)),
        );
        break;
      case "hr":
        break;
    }
  }
};

/**
 * 判断搜索词是否会出现在某个连续渲染文本片段中。
 * 若命中仅藏在链接 URL 中或跨越行内节点边界，调用方可另行展示原文片段。
 */
export const hasHighlightableMarkdownMatch = (
  content: string,
  query: string,
  projectDir?: string,
): boolean => {
  if (!query) return false;
  const normalized = query.toLowerCase();

  if (content.length > MARKDOWN_PARSE_LIMIT) {
    return splitFencedCode(content).some((block) =>
      (block.type === "code" ? block.code : block.text)
        .toLowerCase()
        .includes(normalized),
    );
  }

  const { blocks, references } = parseGfmDocument(content);
  const pieces: string[] = [];
  collectBlockPieces(blocks, { projectDir, references }, pieces);
  return pieces.some((piece) => piece.toLowerCase().includes(normalized));
};

// ---------------------------------------------------------------------------
// 块级渲染
// ---------------------------------------------------------------------------

const HEADING_CLASS: Record<number, string> = {
  1: "mt-4 text-title",
  2: "mt-3 text-section",
  3: "mt-3 text-body font-semibold",
};

const renderBlocks = (blocks: GfmBlock[], ctx: InlineContext): ReactNode =>
  blocks.map((block, index) => {
    switch (block.kind) {
      case "code":
        return (
          <SessionCodeBlock
            key={index}
            code={block.code}
            language={block.language}
            searchQuery={ctx.searchQuery}
          />
        );
      case "heading": {
        const className = cn(
          "mb-1.5 tracking-normal text-fg-1 first:mt-0",
          HEADING_CLASS[block.level] ??
            "mt-2 text-body font-semibold text-fg-2",
        );
        const inner = renderInline(block.text, ctx);
        if (block.level === 1)
          return (
            <h1 key={index} className={className}>
              {inner}
            </h1>
          );
        if (block.level === 2)
          return (
            <h2 key={index} className={className}>
              {inner}
            </h2>
          );
        if (block.level === 3)
          return (
            <h3 key={index} className={className}>
              {inner}
            </h3>
          );
        if (block.level === 4)
          return (
            <h4 key={index} className={className}>
              {inner}
            </h4>
          );
        if (block.level === 5)
          return (
            <h5 key={index} className={className}>
              {inner}
            </h5>
          );
        return (
          <h6 key={index} className={className}>
            {inner}
          </h6>
        );
      }
      case "hr":
        return <hr key={index} className="my-3 border-border" />;
      case "quote":
        return (
          <blockquote
            key={index}
            className="my-2 border-l-2 border-border-strong pl-3 text-fg-2"
          >
            {renderBlocks(block.blocks, ctx)}
          </blockquote>
        );
      case "list": {
        const hasTasks = block.items.some((item) => item.checked !== undefined);
        if (block.ordered) {
          return (
            <ol
              key={index}
              start={block.start}
              className="my-1.5 list-decimal space-y-1 pl-5 marker:text-fg-3"
            >
              {block.items.map((item, itemIdx) => (
                <li key={itemIdx} className="pl-0.5">
                  {renderInline(item.text, ctx)}
                </li>
              ))}
            </ol>
          );
        }
        return (
          <ul
            key={index}
            className={cn(
              "my-1.5 space-y-1 marker:text-fg-3",
              hasTasks ? "list-none pl-1" : "list-disc pl-5",
            )}
          >
            {block.items.map((item, itemIdx) =>
              item.checked !== undefined ? (
                <li key={itemIdx} className="pl-0.5">
                  <span className="inline-flex items-start gap-1.5">
                    <input
                      type="checkbox"
                      checked={item.checked}
                      readOnly
                      disabled
                      className="mt-[3px] size-3.5 accent-action"
                    />
                    <span
                      className={cn(item.checked && "text-fg-2 line-through")}
                    >
                      {renderInline(item.text, ctx)}
                    </span>
                  </span>
                </li>
              ) : (
                <li key={itemIdx} className="pl-0.5">
                  {renderInline(item.text, ctx)}
                </li>
              ),
            )}
          </ul>
        );
      }
      case "table":
        return (
          <div key={index} className="my-2 max-w-full overflow-x-auto">
            <table className="w-full border-collapse text-left text-caption">
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
                      className="border border-border bg-subtle px-2 py-1.5 align-top font-semibold"
                    >
                      {renderInline(header, ctx)}
                    </th>
                  ))}
                </tr>
              </thead>
              {block.rows.length > 0 && (
                <tbody>
                  {block.rows.map((row, rowIdx) => (
                    <tr key={rowIdx}>
                      {row.map((cell, colIdx) => (
                        <td
                          key={colIdx}
                          style={
                            block.aligns[colIdx]
                              ? { textAlign: block.aligns[colIdx] ?? undefined }
                              : undefined
                          }
                          className="border border-border px-2 py-1.5 align-top"
                        >
                          {renderInline(cell, ctx)}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              )}
            </table>
          </div>
        );
      case "paragraph":
        return (
          <p key={index} className="my-1.5 first:mt-0 last:mb-0">
            {renderInline(block.text, ctx)}
          </p>
        );
    }
  });

const VARIANT_CLASS = {
  body: "text-body text-fg-1",
  note: "text-caption text-fg-2",
} as const;

const MarkdownTree = memo(function MarkdownTree({
  content,
  searchQuery,
  projectDir,
}: Pick<SessionMarkdownProps, "content" | "searchQuery" | "projectDir">) {
  const parsed = useMemo(() => parseGfmDocument(content), [content]);
  const rendered = useMemo(
    () =>
      renderBlocks(parsed.blocks, {
        searchQuery,
        projectDir,
        references: parsed.references,
      }),
    [parsed, projectDir, searchQuery],
  );
  return <>{rendered}</>;
});

export const SessionMarkdown = memo(function SessionMarkdown({
  content,
  searchQuery,
  projectDir,
  variant = "body",
  onOpenLink,
  onRevealPath,
  autoLoadRemoteImages = false,
  renderLocalImage,
  className,
}: SessionMarkdownProps) {
  const readerCtx = useReaderContext();
  const effectiveProjectDir = projectDir ?? readerCtx.projectDir;
  const effectiveSearchQuery = searchQuery ?? readerCtx.searchQuery;
  const actions = useMemo<MarkdownActions>(
    () => ({
      openLink: onOpenLink ?? readerCtx.onOpenUrl ?? defaultOpenLink,
      revealPath: onRevealPath ?? readerCtx.onRevealPath,
      autoLoadRemoteImages,
      renderLocalImage,
    }),
    [
      autoLoadRemoteImages,
      onOpenLink,
      onRevealPath,
      readerCtx.onOpenUrl,
      readerCtx.onRevealPath,
      renderLocalImage,
    ],
  );

  if (content.length > MARKDOWN_PARSE_LIMIT) {
    return (
      <SessionPlainText
        content={content}
        searchQuery={effectiveSearchQuery}
        className={cn(VARIANT_CLASS[variant], className)}
      />
    );
  }

  return (
    <MarkdownActionsContext.Provider value={actions}>
      <div
        data-session-markdown=""
        className={cn(
          "min-w-0 break-words [overflow-wrap:anywhere]",
          VARIANT_CLASS[variant],
          className,
        )}
      >
        <MarkdownTree
          content={content}
          searchQuery={effectiveSearchQuery}
          projectDir={effectiveProjectDir}
        />
      </div>
    </MarkdownActionsContext.Provider>
  );
});
