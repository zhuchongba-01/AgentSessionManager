import type { SessionMessage, SessionMeta } from "@/types";
import {
  formatSessionTitle,
  formatTimestamp,
  splitCodexIdeContext,
} from "./utils";
import type { SessionTurnGroup, TimelineStep, ToolStep } from "./turnGrouping";

export interface ExportMarkdownOptions {
  session: SessionMeta;
  providerLabel: string;
  isCodexSession?: boolean;
  includeThinking?: boolean;
  expandedContents?: Map<number, string>;
}

function resolveMessageText(
  message: SessionMessage,
  isCodexSession: boolean,
  expandedContents?: Map<number, string>,
): string {
  const tIdx = message.transcriptIndex ?? message.index;
  const raw =
    (tIdx !== undefined ? expandedContents?.get(tIdx) : undefined) ??
    message.content;
  if (isCodexSession && message.role.toLowerCase() === "user") {
    return splitCodexIdeContext(raw)?.prompt ?? raw;
  }
  return raw;
}

function formatToolStepMarkdown(step: ToolStep): string {
  const name = step.call?.rawName ?? "工具输出";
  const target = step.call?.title
    ? ` \`${step.call.title.slice(0, 120)}\``
    : "";
  const statusSuffix =
    step.status === "error"
      ? " *(失败)*"
      : step.status === "interrupted"
        ? " *(已中断)*"
        : "";
  return `- ⏺ **${name}**${target}${statusSuffix}`;
}

function formatTimelineStepMarkdown(
  step: TimelineStep,
  includeThinking: boolean,
  isCodexSession: boolean,
  expandedContents?: Map<number, string>,
): string | null {
  switch (step.kind) {
    case "thinking": {
      if (!includeThinking) return null;
      if (step.redacted) {
        return `> *思考内容不可见*`;
      }
      const full = resolveMessageText(
        step.message,
        isCodexSession,
        expandedContents,
      ).trim();
      const text = full || step.summary || step.text;
      if (!text) return null;
      const quoted = text
        .split(/\r?\n/)
        .map((line) => `> ${line}`)
        .join("\n");
      return `> **思考过程**\n${quoted}`;
    }
    case "note":
      return step.text.trim() || null;
    case "tool":
      return formatToolStepMarkdown(step);
    case "merged":
      return step.children.map(formatToolStepMarkdown).join("\n");
    case "event":
      return step.event.detail
        ? `> *${step.event.label}*：${step.event.detail}`
        : `> *${step.event.label}*`;
  }
}

export function turnToMarkdown(
  turn: SessionTurnGroup,
  messages: SessionMessage[],
  options: ExportMarkdownOptions,
): string {
  const {
    providerLabel,
    isCodexSession = false,
    includeThinking = false,
    expandedContents,
  } = options;
  const sections: string[] = [];

  if (turn.userIndex !== null && messages[turn.userIndex]) {
    const userMsg = messages[turn.userIndex];
    const ts = userMsg.ts ? ` · ${formatTimestamp(userMsg.ts)}` : "";
    const body = resolveMessageText(
      userMsg,
      isCodexSession,
      expandedContents,
    ).trim();
    if (body) {
      sections.push(`### 用户${ts}\n\n${body}`);
    }
  }

  const stepLines: string[] = [];
  for (const step of turn.steps) {
    const formatted = formatTimelineStepMarkdown(
      step,
      includeThinking,
      isCodexSession,
      expandedContents,
    );
    if (formatted) stepLines.push(formatted);
  }

  if (stepLines.length > 0) {
    sections.push(stepLines.join("\n"));
  }

  if (turn.finalAssistantIndex !== null && messages[turn.finalAssistantIndex]) {
    const aiMsg = messages[turn.finalAssistantIndex];
    const ts = aiMsg.ts ? ` · ${formatTimestamp(aiMsg.ts)}` : "";
    const modelBadge = turn.model ? ` (${turn.model})` : "";
    const body = (
      turn.finalReplyText ??
      resolveMessageText(aiMsg, isCodexSession, expandedContents)
    ).trim();
    if (body) {
      sections.push(`### ${providerLabel}${modelBadge}${ts}\n\n${body}`);
    }
  }

  return sections.join("\n\n");
}

export function sessionToMarkdown(
  turns: SessionTurnGroup[],
  messages: SessionMessage[],
  options: ExportMarkdownOptions,
): string {
  const { session, providerLabel } = options;
  const title = formatSessionTitle(session);
  const headerLines = [
    `# ${title}`,
    "",
    `- **Agent**: ${providerLabel}`,
    `- **Session ID**: \`${session.sessionId}\``,
  ];
  if (session.projectDir) {
    headerLines.push(`- **项目目录**: \`${session.projectDir}\``);
  }
  const activeTs = session.lastActiveAt ?? session.createdAt;
  if (activeTs) {
    headerLines.push(`- **时间**: ${formatTimestamp(activeTs)}`);
  }

  const body = turns
    .map((turn) => turnToMarkdown(turn, messages, options))
    .filter(Boolean)
    .join("\n\n---\n\n");

  return `${headerLines.join("\n")}\n\n---\n\n${body}\n`;
}

export function sanitizeMarkdownFilename(
  title: string,
  sessionId: string,
): string {
  const cleaned = title
    .replace(/[<>:"/\\|?*\u0000-\u001f]+/g, "-")
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 60);
  const stem = cleaned || `session-${sessionId.slice(0, 8)}`;
  return `${stem}.md`;
}

export function downloadMarkdownFile(filename: string, content: string): void {
  const blob = new Blob([content], { type: "text/markdown;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.style.display = "none";
  document.body.appendChild(anchor);
  anchor.click();
  document.body.removeChild(anchor);
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
