import { sessionsApi } from "@/lib/api/sessions";
import { formatMessageTime } from "../utils";
import type { AgentReaderStyle } from "./agentStyles";
import { describeEvent } from "./SessionEventRow";
import {
  formatDuration,
  formatMergedTitle,
  formatStepMeta,
  formatStepTitle,
  formatTokens,
  isFailureStatus,
  type StepMeta,
  type Translate,
} from "./toolSummary";
import {
  isChangeStep,
  type ReaderFilter,
  type SessionTurn,
  type TimelineStep,
  type TurnChapter,
  type TurnEvent,
} from "./turns";

/**
 * 「复制这一轮 / 复制整段为 Markdown / 导出为 Markdown 文件」：
 * 提问原文、步骤写成 `- ⏺ Bash(cargo build) — 失败 · exit 101 · 12s` 列表、
 * 最终回复原文；思考默认不含（决策 D14），并同步遵循当前「全部 / 对话 / 改动」视图筛选。
 */

export interface MarkdownExportOptions {
  style: AgentReaderStyle;
  appName: string;
  youLabel: string;
  projectDir?: string;
  t: Translate;
  /** 费用格式化函数，和界面上看到的一致 */
  formatCost: (usd: number) => string;
  includeThinking?: boolean;
  filter?: ReaderFilter;
}

const metaText = (meta: StepMeta, { t, formatCost }: MarkdownExportOptions) => {
  const parts: string[] = [];
  if (isFailureStatus(meta.status)) parts.push(t(`status.${meta.status}`));
  if (meta.exitCode !== undefined) {
    parts.push(t("exitCode", { code: meta.exitCode }));
  }
  if (meta.diff) parts.push(`+${meta.diff.added} −${meta.diff.removed}`);
  if (meta.durationMs) parts.push(formatDuration(meta.durationMs));
  if (meta.tokens !== undefined) {
    parts.push(t("summaryTokens", { tokens: formatTokens(meta.tokens) }));
  }
  if (meta.costUsd !== undefined) parts.push(formatCost(meta.costUsd));
  return parts.length ? ` — ${parts.join(" · ")}` : "";
};

/** 列表项里的多行文本：续行缩进两格 */
const indentBody = (text: string, prefix = "  ") =>
  text
    .trim()
    .split("\n")
    .map((line, index) => (index === 0 ? line : `${prefix}${line}`))
    .join("\n");

const stepLines = (
  step: TimelineStep,
  options: MarkdownExportOptions,
  depth = 0,
): string[] => {
  const { style, t, projectDir } = options;
  const pad = "  ".repeat(depth);
  switch (step.kind) {
    case "tool": {
      const title = formatStepTitle(step, style, { projectDir, t });
      const meta = formatStepMeta(step, style);
      return [`${pad}- ${meta.glyph} ${title.text}${metaText(meta, options)}`];
    }
    case "merged": {
      const title = formatMergedTitle(step);
      const meta = formatStepMeta(step, style);
      return [
        `${pad}- ${meta.glyph} ${title.text}${metaText(meta, options)}`,
        ...step.children.flatMap((child) =>
          stepLines(child, options, depth + 1),
        ),
      ];
    }
    case "thinking": {
      if (!options.includeThinking) return [];
      const label = t(style.thinkingLabelKey);
      const body = step.block.text || step.block.summary || "";
      const glyph = style.thinkingGlyph ? `${style.thinkingGlyph} ` : "";
      return [
        `${pad}- ${glyph}${label}${body ? `: ${indentBody(body, `${pad}  `)}` : ""}`,
      ];
    }
    case "note":
      return [`${pad}- ${indentBody(step.text, `${pad}  `)}`];
    case "image":
      return [`${pad}- [${step.image.alt || t("image.alt", { index: 1 })}]`];
    case "event": {
      const { label, detail } = describeEvent(step.block, t, step.body);
      return [`${pad}- ${label}${detail ? `: ${indentBody(detail)}` : ""}`];
    }
  }
};

const eventLine = (event: TurnEvent, t: Translate) => {
  const { label, detail } = describeEvent(event.block, t, event.body);
  const url = event.block.kind === "pr_link" && event.block.url;
  return `> ${label}${url ? ` (${url})` : ""}${detail ? `\n>\n> ${detail.replace(/\n/g, "\n> ")}` : ""}`;
};

/** 章节写成小标题，摘要作为引用跟在后面 */
const chapterSection = (chapter: TurnChapter) =>
  `### ${chapter.title}${chapter.summary ? `\n\n> ${chapter.summary}` : ""}`;

const heading = (label: string, ts?: number) =>
  `**${label}**${ts ? ` ${formatMessageTime(ts)}` : ""}`;

export const turnToMarkdown = (
  turn: SessionTurn,
  options: MarkdownExportOptions,
): string => {
  const { t, filter = "all" } = options;
  const sections: string[] = [];

  if (filter === "changes") {
    const changeSteps = turn.steps.filter(
      (step) =>
        isChangeStep(step) ||
        (step.kind === "merged" && step.children.some(isChangeStep)),
    );
    if (changeSteps.length === 0) return "";
    if (turn.question) {
      const images = turn.question.images.map(
        (image, index) =>
          `[${image.alt || t("image.alt", { index: index + 1 })}]`,
      );
      sections.push(
        [
          heading(options.youLabel, turn.question.ts),
          turn.question.text,
          images.join(" "),
        ]
          .filter(Boolean)
          .join("\n\n"),
      );
    }
    const changeLines = changeSteps.flatMap((step) =>
      step.kind === "merged"
        ? step.children
            .filter(isChangeStep)
            .flatMap((child) => stepLines(child, options))
        : stepLines(step, options),
    );
    if (changeLines.length > 0) {
      sections.push(changeLines.join("\n"));
    }
    return sections.join("\n\n");
  }

  if (filter === "all") {
    turn.leadingEvents.forEach((event) => sections.push(eventLine(event, t)));
  }
  if (turn.question) {
    const images = turn.question.images.map(
      (image, index) =>
        `[${image.alt || t("image.alt", { index: index + 1 })}]`,
    );
    sections.push(
      [
        heading(options.youLabel, turn.question.ts),
        turn.question.text,
        images.join(" "),
      ]
        .filter(Boolean)
        .join("\n\n"),
    );
  }
  if (filter === "conversation") {
    turn.chapters.forEach((chapter) => sections.push(chapterSection(chapter)));
    if (turn.final) {
      sections.push(
        `${heading(options.appName, turn.final.ts)}\n\n${turn.final.text}`,
      );
    }
    return sections.join("\n\n");
  }

  let lines: string[] = [];
  const flush = () => {
    if (lines.length) sections.push(lines.join("\n"));
    lines = [];
  };
  const pushChapters = (stepId?: string) =>
    turn.chapters
      .filter((chapter) => chapter.beforeStepId === stepId)
      .forEach((chapter) => {
        flush();
        sections.push(chapterSection(chapter));
      });
  turn.steps.forEach((step) => {
    pushChapters(step.id);
    lines.push(...stepLines(step, options));
  });
  pushChapters();
  flush();
  if (turn.final) {
    sections.push(
      `${heading(options.appName, turn.final.ts)}\n\n${turn.final.text}`,
    );
  }
  turn.trailingEvents.forEach((event) => sections.push(eventLine(event, t)));
  return sections.join("\n\n");
};

export const transcriptToMarkdown = (
  title: string,
  turns: SessionTurn[],
  options: MarkdownExportOptions,
): string => {
  const body = turns
    .map((turn) => turnToMarkdown(turn, options))
    .filter(Boolean)
    .join("\n\n---\n\n");
  return `# ${title}\n\n${body}\n`;
};

/** 触发本地浏览器/WebView 下载 Markdown 文件（纯浏览器无选择器环境兜底） */
export const downloadMarkdownFile = (
  filename: string,
  content: string,
): string => {
  const trimmed = filename.trim() || "session.md";
  const safeName = trimmed.toLowerCase().endsWith(".md")
    ? trimmed
    : `${trimmed}.md`;
  const blob = new Blob([content], { type: "text/markdown;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  try {
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = safeName;
    anchor.style.display = "none";
    document.body.appendChild(anchor);
    anchor.click();
    document.body.removeChild(anchor);
  } finally {
    window.setTimeout(() => {
      URL.revokeObjectURL(url);
    }, 1000);
  }
  return safeName;
};

/**
 * 弹出系统「另存为」窗口让用户选择保存路径并写入 Markdown 文件。
 * 用户取消窗口时返回 `null`（不触发任何自动下载或提示）。
 */
export const saveMarkdownFile = async (
  filename: string,
  content: string,
  defaultDir?: string,
): Promise<string | null> => {
  const trimmed = filename.trim() || "session.md";
  const safeName = trimmed.toLowerCase().endsWith(".md")
    ? trimmed
    : `${trimmed}.md`;

  try {
    return await sessionsApi.saveMarkdownExport(safeName, content, defaultDir);
  } catch (error) {
    // 仅当运行在无 Tauri IPC 的纯浏览器环境时回退到 File System Access API
    const isMissingTauriIpc =
      typeof window !== "undefined" &&
      !("__TAURI_INTERNALS__" in window) &&
      error instanceof TypeError;
    if (!isMissingTauriIpc) {
      throw error;
    }
  }

  const win = window as Window & {
    showSaveFilePicker?: (options?: {
      suggestedName?: string;
      types?: Array<{
        description: string;
        accept: Record<string, string[]>;
      }>;
    }) => Promise<{
      name?: string;
      createWritable: () => Promise<{
        write: (data: string) => Promise<void>;
        close: () => Promise<void>;
      }>;
    }>;
  };
  if (typeof win.showSaveFilePicker === "function") {
    try {
      const handle = await win.showSaveFilePicker({
        suggestedName: safeName,
        types: [
          {
            description: "Markdown 文档",
            accept: { "text/markdown": [".md"] },
          },
        ],
      });
      const writable = await handle.createWritable();
      await writable.write(content);
      await writable.close();
      return handle.name || safeName;
    } catch (pickerError) {
      if (
        pickerError instanceof DOMException &&
        pickerError.name === "AbortError"
      ) {
        return null;
      }
      throw pickerError;
    }
  }

  return downloadMarkdownFile(safeName, content);
};
