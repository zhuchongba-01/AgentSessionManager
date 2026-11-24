import type { SessionMessage, SessionMeta } from "@/types";
import {
  formatSessionTitle,
  formatTimestamp,
  splitCodexIdeContext,
} from "./utils";
import { extractToolName, type SessionTurnGroup } from "./turnGrouping";

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

function summarizeToolLine(content: string): string {
  const lines = content
    .trim()
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  const toolName = extractToolName(content);
  if (toolName) {
    const detail = lines[1] ? ` — \`${lines[1].slice(0, 100)}\`` : "";
    return `- ⏺ **${toolName}**${detail}`;
  }
  const preview = (lines[0] ?? "工具输出").slice(0, 120);
  return `- ⏺ 工具输出：\`${preview.replace(/`/g, "'")}\``;
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
  for (const idx of turn.stepIndexes) {
    const msg = messages[idx];
    if (!msg) continue;
    const role = msg.role.toLowerCase();
    const text = resolveMessageText(
      msg,
      isCodexSession,
      expandedContents,
    ).trim();
    if (!text) continue;

    if (role === "thinking") {
      if (includeThinking) {
        const quoted = text
          .split(/\r?\n/)
          .map((line) => `> ${line}`)
          .join("\n");
        stepLines.push(`> **思考过程**\n${quoted}`);
      }
    } else if (role === "assistant") {
      stepLines.push(text);
    } else if (role !== "user") {
      stepLines.push(summarizeToolLine(text));
    }
  }

  if (stepLines.length > 0) {
    sections.push(stepLines.join("\n"));
  }

  if (turn.finalAssistantIndex !== null && messages[turn.finalAssistantIndex]) {
    const aiMsg = messages[turn.finalAssistantIndex];
    const ts = aiMsg.ts ? ` · ${formatTimestamp(aiMsg.ts)}` : "";
    const body = resolveMessageText(
      aiMsg,
      isCodexSession,
      expandedContents,
    ).trim();
    if (body) {
      sections.push(`### ${providerLabel}${ts}\n\n${body}`);
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
