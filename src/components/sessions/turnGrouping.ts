import type { ReaderBlock, SessionMessage } from "@/types";
import {
  classifyToolKind,
  getAgentReaderStyle,
  type AgentReaderStyle,
  type ToolActionKind,
  type ToolRunStatus,
} from "./agentReaderStyles";
import { extractDiffPayload } from "./SessionDiffView";
import type { SessionReaderViewMode } from "./types";

const TOOL_HEADER_RE = /^\[Tool:\s*([^\]]+)\](?:\s+(.*))?$/i;
const RESULT_HEADER_RE = /^\[(Result|Error):\s*([^\]]+)\]/i;
const GENERIC_RESULT_HEADER_RE = /^\[(Result|Error)\]\s*$/i;
const COMPACTED_PREFIX = "[Compacted Summary]";
const INTERRUPTED_PREFIX = "[Request interrupted by user";

const WRITE_TOOL_NAMES = new Set([
  "edit",
  "multiedit",
  "write",
  "write_file",
  "edit_file",
  "replace",
  "notebookedit",
  "apply_patch",
  "filechange",
  "patch",
  "create_file",
  "str_replace_editor",
  "str_replace_based_edit_tool",
]);

const DIFF_CONTENT_RE =
  /(?:^|\n)(?:```(?:diff|patch)\b|\*\*\* (?:Update|Add|Delete) File:|@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@)/m;

const ERROR_CONTENT_RE =
  /(?:^|\n)(?:Exit code[:\s]+[1-9]\d*|Process exited with code [1-9]\d*|\[(?:error|failed|Web search error)[^\]]*\]|Error:|Traceback \(most recent call last\):|thread '.*' panicked at)/i;

/** 折叠态下最多常显几条失败步骤（cc-switch 规则 2） */
export const MAX_PINNED_FAILURES = 5;
/** 失败步骤自动展开时的预览行数 */
export const FAILURE_PREVIEW_LINES = 6;
/** 普通步骤展开时的默认预览行数 */
export const STEP_PREVIEW_LINES = 12;

export interface ToolCountChip {
  name: string;
  count: number;
}

export interface ParsedToolCall {
  id: string;
  rawName: string;
  kind: ToolActionKind;
  server?: string;
  title: string;
  detail?: string;
  inputPreview: string;
  inputTruncated: boolean;
  diffAdded?: number;
  diffRemoved?: number;
  diffText?: string;
}

export interface ParsedToolResult {
  callId: string;
  status: ToolRunStatus;
  output: string;
  lineCount: number;
  exitCode?: number;
  durationMs?: number;
  message: SessionMessage;
}

export interface ThinkingStep {
  kind: "thinking";
  id: string;
  messageIndex: number;
  text: string;
  summary?: string;
  redacted: boolean;
  durationMs?: number;
  message: SessionMessage;
}

export interface NoteStep {
  kind: "note";
  id: string;
  messageIndex: number;
  text: string;
  message: SessionMessage;
}

export interface ToolStep {
  kind: "tool";
  id: string;
  messageIndex: number;
  resultMessageIndex?: number;
  call?: ParsedToolCall;
  result?: ParsedToolResult;
  status: ToolRunStatus;
  sourceMessage: SessionMessage;
}

export interface MergedStep {
  kind: "merged";
  id: string;
  mergeKind: "explored" | "mcp";
  server?: string;
  children: ToolStep[];
  status: ToolRunStatus;
  messageIndex: number;
}

export interface EventItem {
  id: string;
  messageIndex: number;
  eventKind:
    | "compaction"
    | "aborted"
    | "model_change"
    | "slash_command"
    | "pr_link"
    | "hook"
    | "sub_agent"
    | "error"
    | "info";
  label: string;
  detail?: string;
  url?: string;
  tone: "muted" | "danger";
}

export interface EventStep {
  kind: "event";
  id: string;
  messageIndex: number;
  event: EventItem;
}

export type TimelineStep =
  | ThinkingStep
  | NoteStep
  | ToolStep
  | MergedStep
  | EventStep;

export interface TurnChapter {
  id: string;
  messageIndex: number;
  title: string;
  summary?: string;
  beforeStepId?: string;
}

export interface SessionTurnGroup {
  turnKey: string;
  turnIndex: number;
  /** 第几个含用户提问的轮次（1 起，用于无障碍跳转标签） */
  userTurnOrdinal: number | null;
  messageIndexes: number[];
  userIndex: number | null;
  /** 中间过程关联的原始消息下标集合 */
  stepIndexes: number[];
  /** 结构化步骤序列（含合并后的 Explored / MCP 组） */
  steps: TimelineStep[];
  chapters: TurnChapter[];
  leadingEvents: EventItem[];
  trailingEvents: EventItem[];
  finalAssistantIndex: number | null;
  /** 最终回复的纯文本（当同一条 assistant 混有前置工具调用时，仅取最后工具调用之后的最终正文） */
  finalReplyText?: string;
  agentStartTs?: number;
  model?: string;
  tokens?: number;
  costUsd?: number;
  durationMs?: number;
  aborted: boolean;
  toolCount: number;
  commandCount: number;
  thinkingCount: number;
  editCount: number;
  filesChanged: number;
  errorCount: number;
  hasChanges: boolean;
  toolChips: ToolCountChip[];
}

export type ReaderItem =
  | {
      kind: "question";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      message: SessionMessage;
    }
  | {
      kind: "timeline";
      key: string;
      turnIndex: number;
      turnKey: string;
      turn: SessionTurnGroup;
      expanded: boolean;
      hiddenFailures: number;
      isAgentStart: boolean;
    }
  | {
      kind: "step";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      step: TimelineStep;
      depth: 0 | 1;
      expanded: boolean;
      previewLines: number;
      pinned: boolean;
      inTimeline: boolean;
      isAgentStart: boolean;
    }
  | {
      kind: "chapter";
      key: string;
      turnIndex: number;
      turnKey: string;
      chapter: TurnChapter;
      isAgentStart: boolean;
    }
  | {
      kind: "event";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      event: EventItem;
    }
  | {
      kind: "injected";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      message: SessionMessage;
    }
  | {
      kind: "final";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      message: SessionMessage;
      overrideText?: string;
      isAgentStart: boolean;
    }
  | {
      kind: "divider";
      key: string;
      turnIndex: number;
      turnKey: string;
    };

export function extractToolName(content: string): string | null {
  const firstLine = content.trimStart().split(/\r?\n/, 1)[0] ?? "";
  const matched = firstLine.match(TOOL_HEADER_RE);
  if (matched?.[1]) {
    return matched[1].trim();
  }
  return null;
}

export function extractExitCode(content: string): number | undefined {
  const match = content.match(
    /(?:^|\n)(?:Exit code[:\s]+|Process exited with code\s+)(\d+)\b/i,
  );
  if (!match) return undefined;
  const parsed = Number(match[1]);
  return Number.isFinite(parsed) ? parsed : undefined;
}

export function splitThinkingSummary(raw: string): {
  summary?: string;
  body: string;
} {
  const trimmed = raw.trim();
  if (!trimmed.startsWith("**")) {
    return { body: trimmed };
  }
  const nl = trimmed.indexOf("\n");
  const firstLine = (nl >= 0 ? trimmed.slice(0, nl) : trimmed).trim();
  if (
    firstLine.length > 4 &&
    firstLine.startsWith("**") &&
    firstLine.endsWith("**") &&
    !firstLine.slice(2, -2).includes("**")
  ) {
    const summary = firstLine.slice(2, -2).trim();
    const rest = nl >= 0 ? trimmed.slice(nl + 1).trim() : "";
    if (summary) {
      return { summary, body: rest };
    }
  }
  return { body: trimmed };
}

export function stripCodexEnvelope(raw: string): {
  body: string;
  exitCode?: number;
  durationMs?: number;
} {
  const lines = raw.replace(/\r\n/g, "\n").split("\n");
  let idx = 0;
  let sawHeader = false;
  let exitCode: number | undefined;
  let durationMs: number | undefined;

  while (idx < lines.length && idx < 8) {
    const t = lines[idx].trim();
    if (
      t.startsWith("Chunk ID:") ||
      t.startsWith("Original token count:") ||
      t.startsWith("Total output lines:")
    ) {
      sawHeader = true;
      idx += 1;
      continue;
    }
    if (t.startsWith("Wall time:")) {
      sawHeader = true;
      const num = parseFloat(
        t
          .slice("Wall time:".length)
          .replace(/seconds|s$/i, "")
          .trim(),
      );
      if (Number.isFinite(num) && num >= 0) {
        durationMs = Math.round(num * 1000);
      }
      idx += 1;
      continue;
    }
    if (t.startsWith("Process exited with code ")) {
      sawHeader = true;
      const code = Number(t.slice("Process exited with code ".length).trim());
      if (Number.isFinite(code)) exitCode = code;
      idx += 1;
      continue;
    }
    if (t === "Output:" && sawHeader) {
      idx += 1;
      break;
    }
    break;
  }

  const body = sawHeader ? lines.slice(idx).join("\n") : raw;
  if (exitCode === undefined) {
    exitCode = extractExitCode(body);
  }
  return { body: body.replace(/[\r\n]+$/, ""), exitCode, durationMs };
}

export function isModificationMessage(message: SessionMessage): boolean {
  const role = message.role.toLowerCase();
  if (role === "user" || role === "thinking") return false;
  if (
    message.meta?.blocks?.some(
      (b) =>
        b.type === "tool_call" &&
        (WRITE_TOOL_NAMES.has(
          b.rawName.toLowerCase().replace(/^mcp__[^_]+__/, ""),
        ) ||
          Boolean(b.diffText) ||
          (b.diffAdded ?? 0) + (b.diffRemoved ?? 0) > 0),
    )
  ) {
    return true;
  }
  const toolName = extractToolName(message.content);
  if (toolName) {
    const normalized = toolName.toLowerCase().replace(/^mcp__[^_]+__/, "");
    if (WRITE_TOOL_NAMES.has(normalized)) return true;
  }
  return (
    DIFF_CONTENT_RE.test(message.content) ||
    extractDiffPayload(message.content) !== null
  );
}

export function isErrorToolMessage(message: SessionMessage): boolean {
  const role = message.role.toLowerCase();
  if (role === "user" || role === "thinking") return false;
  if (
    message.meta?.blocks?.some(
      (b) =>
        b.type === "tool_result" &&
        (b.status === "error" || b.status === "interrupted"),
    )
  ) {
    return true;
  }
  if (role === "assistant") return false;
  return ERROR_CONTENT_RE.test(message.content);
}

export function isFailureStep(step: TimelineStep): boolean {
  return (
    (step.kind === "tool" || step.kind === "merged") &&
    (step.status === "error" || step.status === "interrupted")
  );
}

export function isChangeStep(step: TimelineStep): step is ToolStep {
  if (step.kind !== "tool") return false;
  if (step.call) {
    if (step.call.kind === "edit" || step.call.kind === "write") return true;
    if (
      step.call.diffText ||
      (step.call.diffAdded ?? 0) + (step.call.diffRemoved ?? 0) > 0
    ) {
      return true;
    }
  }
  return isModificationMessage(step.sourceMessage);
}

function isChapterToolCall(call: ParsedToolCall): boolean {
  if (call.kind !== "mcp" || !call.rawName.startsWith("mcp__")) return false;
  if (!call.rawName.endsWith("__mark_chapter")) return false;
  const t = call.title.trim();
  return t.length > 0 && !t.endsWith(".mark_chapter");
}

function splitMcpServer(rawName: string): string | undefined {
  if (!rawName.startsWith("mcp__")) return undefined;
  const rest = rawName.slice("mcp__".length);
  const idx = rest.indexOf("__");
  return idx > 0 ? rest.slice(0, idx) : undefined;
}

/**
 * 将单条 SessionMessage 归一化为结构化 ReaderBlock 列表：
 * - 若后端已附带 `meta.blocks`，优先采用；
 * - 否则从旧格式文本（含混合在 assistant 里的 `[Tool: X]`、`[Result: X]`、`$ cmd`、`[Web search]`、`[Compacted Summary]` 等）无损提取。
 */
function normalizeMessageBlocks(
  message: SessionMessage,
  msgIdx: number,
): ReaderBlock[] {
  if (message.meta?.blocks && message.meta.blocks.length > 0) {
    return message.meta.blocks;
  }

  const role = message.role.toLowerCase();
  const raw = message.content ?? "";
  const trimmed = raw.trim();

  if (role === "thinking") {
    const redacted =
      Boolean(message.meta?.redacted) || trimmed === "[Redacted Thinking]";
    if (redacted) {
      return [
        {
          type: "thinking",
          text: "",
          redacted: true,
          durationMs: message.meta?.durationMs,
        },
      ];
    }
    if (!trimmed) return [];
    const { summary, body } = splitThinkingSummary(raw);
    return [
      {
        type: "thinking",
        text: summary && !body ? "" : summary ? body : raw,
        summary: message.meta?.thinkingSummary ?? summary,
        redacted: false,
        durationMs: message.meta?.durationMs,
      },
    ];
  }

  if (trimmed.startsWith(COMPACTED_PREFIX)) {
    const detail = trimmed.slice(COMPACTED_PREFIX.length).trim();
    return [
      {
        type: "event",
        kind: "compaction",
        text: "上下文已压缩",
        detail: detail || undefined,
      },
    ];
  }

  if (role === "user") {
    if (trimmed.startsWith(INTERRUPTED_PREFIX)) {
      return [{ type: "event", kind: "aborted", text: "已中断" }];
    }
    return trimmed ? [{ type: "text", text: raw }] : [];
  }

  if (role === "system") {
    const isErr = /^\[error/i.test(trimmed);
    const clean = trimmed.replace(/^\[Error\]\s*/i, "");
    return [
      {
        type: "event",
        kind: isErr ? "error" : "info",
        text: clean.split(/\r?\n/, 1)[0] ?? clean,
        detail: clean.includes("\n")
          ? clean.slice(clean.indexOf("\n") + 1).trim()
          : undefined,
      },
    ];
  }

  if (role !== "assistant") {
    // role === "tool" 等
    if (!trimmed) return [];

    // 1. `[Result: Name]\n...` 或 `[Error: Name]\n...`
    const resMatch = trimmed.match(RESULT_HEADER_RE);
    if (resMatch) {
      const isErr = resMatch[1].toLowerCase() === "error";
      const toolName = resMatch[2].trim();
      const nl = trimmed.indexOf("\n");
      const body = nl >= 0 ? trimmed.slice(nl + 1) : "";
      const env = stripCodexEnvelope(body);
      return [
        {
          type: "tool_result",
          callId: `legacy_name:${toolName.toLowerCase()}`,
          status:
            isErr || (env.exitCode !== undefined && env.exitCode !== 0)
              ? "error"
              : "success",
          output: env.body,
          lineCount: env.body ? env.body.split("\n").length : 0,
          exitCode: env.exitCode,
          durationMs: env.durationMs,
        },
      ];
    }

    if (GENERIC_RESULT_HEADER_RE.test(trimmed.split(/\r?\n/, 1)[0] ?? "")) {
      const isErr = trimmed.toLowerCase().startsWith("[error");
      const nl = trimmed.indexOf("\n");
      const body = nl >= 0 ? trimmed.slice(nl + 1) : "";
      const env = stripCodexEnvelope(body);
      return [
        {
          type: "tool_result",
          callId: "",
          status: isErr ? "error" : "success",
          output: env.body,
          lineCount: env.body ? env.body.split("\n").length : 0,
          exitCode: env.exitCode,
          durationMs: env.durationMs,
        },
      ];
    }

    // 2. `[Tool: Name]...`
    const firstLine = trimmed.split(/\r?\n/, 1)[0] ?? "";
    const toolMatch = firstLine.match(TOOL_HEADER_RE);
    if (toolMatch) {
      const rawName = toolMatch[1].trim();
      const inlineTarget = toolMatch[2]?.trim() ?? "";
      const restLines = raw.replace(/\r\n/g, "\n").split("\n").slice(1);
      const restText = restLines.join("\n").trim();
      const diffPayload = extractDiffPayload(raw);

      let title = inlineTarget;
      let remainingOutput = restText;
      if (!title && restLines.length > 0) {
        for (let i = 0; i < restLines.length; i += 1) {
          const l = restLines[i].trim();
          if (!l) continue;
          if (
            l.startsWith("```") ||
            l.startsWith("@@ ") ||
            l.startsWith("--- ") ||
            l.startsWith("*** Begin Patch") ||
            /^\*\*\* (?:Add|Update|Delete) File:/.test(l) ||
            l.startsWith("diff --git ")
          ) {
            const patchFile = restLines.find((pl) =>
              /^\*\*\* (?:Add|Update|Delete) File:\s*(.+)$/.test(pl),
            );
            if (patchFile) {
              title = patchFile
                .replace(/^\*\*\* (?:Add|Update|Delete) File:\s*/, "")
                .trim();
            }
            remainingOutput = restLines.slice(i).join("\n").trim();
            break;
          }
          title = l.replace(/^\$\s+/, "");
          remainingOutput = restLines
            .slice(i + 1)
            .join("\n")
            .trim();
          break;
        }
      }

      const blocks: ReaderBlock[] = [
        {
          type: "tool_call",
          id: `legacy_call_${msgIdx}`,
          rawName,
          server: splitMcpServer(rawName),
          title,
          inputPreview: "",
          inputTruncated: false,
          diffAdded: diffPayload?.added,
          diffRemoved: diffPayload?.removed,
          diffText: diffPayload?.diffText,
        },
      ];

      if (remainingOutput && !diffPayload) {
        const env = stripCodexEnvelope(remainingOutput);
        const isErr =
          isErrorToolMessage(message) ||
          (env.exitCode !== undefined && env.exitCode !== 0);
        blocks.push({
          type: "tool_result",
          callId: `legacy_call_${msgIdx}`,
          status: isErr ? "error" : "success",
          output: env.body,
          lineCount: env.body ? env.body.split("\n").length : 0,
          exitCode: env.exitCode,
          durationMs: env.durationMs,
        });
      }
      return blocks;
    }

    // 3. `$ cmd` 或 `[Web search] query`
    if (trimmed.startsWith("$ ")) {
      const nl = trimmed.indexOf("\n");
      const cmd = (nl >= 0 ? trimmed.slice(2, nl) : trimmed.slice(2)).trim();
      const out = nl >= 0 ? trimmed.slice(nl + 1).trim() : "";
      const blocks: ReaderBlock[] = [
        {
          type: "tool_call",
          id: `legacy_sh_${msgIdx}`,
          rawName: "shell",
          title: cmd,
          inputPreview: "",
          inputTruncated: false,
        },
      ];
      if (out) {
        const env = stripCodexEnvelope(out);
        blocks.push({
          type: "tool_result",
          callId: `legacy_sh_${msgIdx}`,
          status: env.exitCode && env.exitCode !== 0 ? "error" : "success",
          output: env.body,
          lineCount: env.body.split("\n").length,
          exitCode: env.exitCode,
          durationMs: env.durationMs,
        });
      }
      return blocks;
    }

    if (trimmed.startsWith("[Web search]")) {
      const q = trimmed.slice("[Web search]".length).trim();
      return [
        {
          type: "tool_call",
          id: `legacy_ws_${msgIdx}`,
          rawName: "web_search",
          title: q,
          inputPreview: "",
          inputTruncated: false,
        },
      ];
    }

    // 4. 裸工具输出（可能配对上一条 `[Tool: X]`，也可能是独立工具输出）
    const env = stripCodexEnvelope(raw);
    const isErr =
      isErrorToolMessage(message) ||
      (env.exitCode !== undefined && env.exitCode !== 0);
    return [
      {
        type: "tool_result",
        callId: "",
        status: isErr ? "error" : "success",
        output: env.body,
        lineCount: env.body ? env.body.split("\n").length : 0,
        exitCode: env.exitCode,
        durationMs: env.durationMs,
      },
    ];
  }

  // role === "assistant"：拆分其中混排的 `[Tool: X]` 行与普通说明/回复文本
  if (!trimmed) return [];
  const blocks: ReaderBlock[] = [];
  let pendingLines: string[] = [];
  const flushText = () => {
    const joined = pendingLines.join("\n").trim();
    if (joined) {
      blocks.push({ type: "text", text: joined });
    }
    pendingLines = [];
  };

  const lines = raw.replace(/\r\n/g, "\n").split("\n");
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const m = line.trim().match(TOOL_HEADER_RE);
    if (!m) {
      pendingLines.push(line);
      continue;
    }
    flushText();
    const rawName = m[1].trim();
    const inlineTarget = m[2]?.trim() ?? "";
    // 若后面紧跟 apply_patch 或 fenced diff，将其吸收进该工具调用
    const patchLines: string[] = [];
    if (
      lines[i + 1]?.trim().startsWith("*** Begin Patch") ||
      lines[i + 1]?.trim().startsWith("```diff")
    ) {
      const isFence = lines[i + 1].trim().startsWith("```");
      i += 1;
      patchLines.push(lines[i]);
      while (i + 1 < lines.length) {
        i += 1;
        patchLines.push(lines[i]);
        if (isFence && lines[i].trim().startsWith("```")) break;
        if (!isFence && lines[i].trim().startsWith("*** End Patch")) break;
      }
    }
    const patchBody = patchLines.join("\n");
    const diffInfo = patchBody ? extractDiffPayload(patchBody) : null;
    let title = inlineTarget;
    if (!title && patchBody) {
      const fileMatch = patchBody.match(
        /^\*\*\* (?:Add|Update|Delete) File:\s*(.+)$/m,
      );
      if (fileMatch?.[1]) title = fileMatch[1].trim();
    }
    blocks.push({
      type: "tool_call",
      id: `legacy_asst_call_${msgIdx}_${blocks.length}`,
      rawName,
      server: splitMcpServer(rawName),
      title,
      inputPreview: patchBody,
      inputTruncated: false,
      diffAdded: diffInfo?.added,
      diffRemoved: diffInfo?.removed,
      diffText: diffInfo?.diffText,
    });
  }
  flushText();
  return blocks;
}

function mergeConsecutiveSteps(
  steps: TimelineStep[],
  style: AgentReaderStyle,
): TimelineStep[] {
  const mergeKeyOf = (step: TimelineStep): string => {
    if (step.kind !== "tool" || !step.call) return "";
    if (step.status === "error" || step.status === "interrupted") return "";
    const { kind, server } = step.call;
    if (style.merge.readSearchRuns && (kind === "read" || kind === "search")) {
      return "explored";
    }
    if (style.merge.mcpSameServerRuns && kind === "mcp" && server) {
      return `mcp:${server}`;
    }
    return "";
  };

  const merged: TimelineStep[] = [];
  let run: ToolStep[] = [];
  let runKey = "";

  const flush = () => {
    if (run.length >= 2) {
      merged.push({
        kind: "merged",
        id: `${run[0].id}+merged`,
        mergeKind: runKey === "explored" ? "explored" : "mcp",
        server: runKey === "explored" ? undefined : run[0].call?.server,
        children: run,
        status: "success",
        messageIndex: run[0].messageIndex,
      });
    } else {
      merged.push(...run);
    }
    run = [];
    runKey = "";
  };

  for (const step of steps) {
    const key = mergeKeyOf(step);
    if (key && key === runKey) {
      run.push(step as ToolStep);
      continue;
    }
    flush();
    if (key) {
      run = [step as ToolStep];
      runKey = key;
    } else {
      merged.push(step);
    }
  }
  flush();
  return merged;
}

export function groupMessagesIntoTurns(
  messages: SessionMessage[],
  providerId?: string | null,
): SessionTurnGroup[] {
  if (messages.length === 0) return [];
  const style = getAgentReaderStyle(providerId);

  const rawSlices: Array<{ start: number; end: number }> = [];
  let currentStart = 0;

  for (let i = 0; i < messages.length; i += 1) {
    const msg = messages[i];
    const isRealUser =
      msg.role.toLowerCase() === "user" &&
      !msg.meta?.injected &&
      !msg.content.trim().startsWith(COMPACTED_PREFIX) &&
      !msg.content.trim().startsWith(INTERRUPTED_PREFIX);
    if (isRealUser && i > currentStart) {
      rawSlices.push({ start: currentStart, end: i });
      currentStart = i;
    }
  }
  rawSlices.push({ start: currentStart, end: messages.length });

  let userOrdinalCounter = 0;

  return rawSlices.map(({ start, end }, turnIndex) => {
    const indexes: number[] = [];
    for (let i = start; i < end; i += 1) {
      indexes.push(i);
    }

    const blockCache = new Map<number, ReaderBlock[]>();
    const blocksOf = (i: number) => {
      let b = blockCache.get(i);
      if (!b) {
        b = normalizeMessageBlocks(messages[i], i);
        blockCache.set(i, b);
      }
      return b;
    };

    let userIndex: number | null = null;
    for (let i = start; i < end; i += 1) {
      const msg = messages[i];
      if (
        msg.role.toLowerCase() === "user" &&
        !msg.meta?.injected &&
        blocksOf(i).some((b) => b.type === "text" && b.text.trim().length > 0)
      ) {
        userIndex = i;
        break;
      }
    }
    const userTurnOrdinal =
      userIndex !== null ? (userOrdinalCounter += 1) : null;

    // 定位本轮最终回复：最后一个非章节 tool_call 之后的最后一条含非空 Text 的 assistant 消息
    let lastToolCallPos: [number, number] = [-1, -1];
    for (let i = start; i < end; i += 1) {
      blocksOf(i).forEach((b, j) => {
        if (b.type === "tool_call") {
          const kind = classifyToolKind(b.rawName);
          const isChap =
            kind === "mcp" &&
            b.rawName.startsWith("mcp__") &&
            b.rawName.endsWith("__mark_chapter") &&
            Boolean(b.title.trim());
          if (!isChap) {
            lastToolCallPos = [i, j];
          }
        } else if (b.type === "tool_result" && i > lastToolCallPos[0]) {
          // 独立结果也在最终回复之前
          lastToolCallPos = [i, j];
        }
      });
    }

    let finalAt: [number, number] | null = null;
    for (let i = end - 1; i >= start; i -= 1) {
      const msg = messages[i];
      if (msg.role.toLowerCase() !== "assistant" || msg.meta?.injected) {
        continue;
      }
      if (i < lastToolCallPos[0]) break;
      const from = i === lastToolCallPos[0] ? lastToolCallPos[1] + 1 : 0;
      const slice = blocksOf(i).slice(from);
      if (slice.some((b) => b.type === "text" && b.text.trim().length > 0)) {
        finalAt = [i, from];
        break;
      }
    }

    const anchorMsg = messages[userIndex ?? start];
    const anchorIdentity =
      anchorMsg?.transcriptIndex ?? anchorMsg?.index ?? start;
    const turnKey = `turn-${turnIndex}-${anchorIdentity}`;

    const rawSteps: TimelineStep[] = [];
    const stepIndexSet = new Set<number>();
    const rawChapters: Array<{ chapter: TurnChapter; at: number }> = [];
    const chapterCallIds = new Set<string>();
    const callStepById = new Map<string, ToolStep>();
    const openUnpairedCalls: ToolStep[] = [];
    const leadingEvents: EventItem[] = [];
    let aborted = false;
    let agentStartTs: number | undefined;
    let firstTs: number | undefined;
    let lastTs: number | undefined;
    let turnModel: string | undefined;
    let totalTokens = 0;
    let hasTokens = false;
    let totalCostUsd = 0;
    let hasCost = false;
    const finalTextParts: string[] = [];

    for (let i = start; i < end; i += 1) {
      const msg = messages[i];
      if (msg.ts !== undefined) {
        firstTs ??= msg.ts;
        lastTs = msg.ts;
      }
      if (msg.meta?.model) {
        turnModel = msg.meta.model;
      }
      if (msg.meta?.tokens !== undefined) {
        totalTokens += msg.meta.tokens;
        hasTokens = true;
      }
      if (msg.meta?.costUsd !== undefined) {
        totalCostUsd += msg.meta.costUsd;
        hasCost = true;
      }
      if (msg.meta?.injected) continue;

      const recordAgentTs = () => {
        if (agentStartTs === undefined && msg.ts !== undefined) {
          agentStartTs = msg.ts;
        }
      };

      const blocks = blocksOf(i);
      blocks.forEach((block, j) => {
        const blockId = `${turnKey}:${i}:${j}`;
        const inFinal = finalAt !== null && i === finalAt[0] && j >= finalAt[1];

        if (block.type === "text") {
          if (i === userIndex) return;
          if (inFinal) {
            if (block.text.trim()) finalTextParts.push(block.text);
            recordAgentTs();
            return;
          }
          if (!block.text.trim()) return;
          recordAgentTs();
          stepIndexSet.add(i);
          rawSteps.push({
            kind: "note",
            id: blockId,
            messageIndex: i,
            text: block.text,
            message: msg,
          });
          return;
        }

        if (block.type === "thinking") {
          if (!block.text && !block.summary && !block.redacted) return;
          recordAgentTs();
          stepIndexSet.add(i);
          rawSteps.push({
            kind: "thinking",
            id: blockId,
            messageIndex: i,
            text: block.text,
            summary: block.summary,
            redacted: Boolean(block.redacted),
            durationMs: block.durationMs,
            message: msg,
          });
          return;
        }

        if (block.type === "tool_call") {
          recordAgentTs();
          const parsedCall: ParsedToolCall = {
            id: block.id,
            rawName: block.rawName,
            kind: classifyToolKind(block.rawName),
            server: block.server ?? splitMcpServer(block.rawName),
            title: block.title,
            detail: block.detail,
            inputPreview: block.inputPreview ?? "",
            inputTruncated: Boolean(block.inputTruncated),
            diffAdded: block.diffAdded,
            diffRemoved: block.diffRemoved,
            diffText: block.diffText,
          };
          if (isChapterToolCall(parsedCall)) {
            if (parsedCall.id) chapterCallIds.add(parsedCall.id);
            rawChapters.push({
              chapter: {
                id: blockId,
                messageIndex: i,
                title: parsedCall.title.trim(),
                summary: parsedCall.detail?.trim() || undefined,
              },
              at: rawSteps.length,
            });
            return;
          }
          stepIndexSet.add(i);
          const step: ToolStep = {
            kind: "tool",
            id: blockId,
            messageIndex: i,
            call: parsedCall,
            status: parsedCall.id.startsWith("legacy_") ? "unknown" : "pending",
            sourceMessage: msg,
          };
          if (parsedCall.id && !callStepById.has(parsedCall.id)) {
            callStepById.set(parsedCall.id, step);
          }
          openUnpairedCalls.push(step);
          rawSteps.push(step);
          return;
        }

        if (block.type === "tool_result") {
          if (block.callId && chapterCallIds.has(block.callId)) return;
          recordAgentTs();
          stepIndexSet.add(i);
          const status: ToolRunStatus =
            block.status === "error"
              ? "error"
              : block.status === "interrupted"
                ? "interrupted"
                : "success";
          const parsedResult: ParsedToolResult = {
            callId: block.callId,
            status,
            output: block.output,
            lineCount: block.lineCount,
            exitCode: block.exitCode,
            durationMs: block.durationMs,
            message: msg,
          };

          // 1) 按精确 callId 配对
          let owner = block.callId ? callStepById.get(block.callId) : undefined;
          // 2) 按 `[Result: Name]` 工具名就近配对
          if (!owner && block.callId.startsWith("legacy_name:")) {
            const targetName = block.callId.slice("legacy_name:".length);
            for (let k = openUnpairedCalls.length - 1; k >= 0; k -= 1) {
              const cand = openUnpairedCalls[k];
              if (
                !cand.result &&
                cand.call?.rawName.toLowerCase() === targetName
              ) {
                owner = cand;
                break;
              }
            }
          }
          // 3) 紧邻上一条尚未配对结果的 `[Tool: X]` 调用（如 Claude/Codex 旧格式相邻行）
          if (!owner && !block.callId) {
            const lastOpen = openUnpairedCalls[openUnpairedCalls.length - 1];
            if (lastOpen && !lastOpen.result) {
              owner = lastOpen;
            }
          }

          if (owner && !owner.result) {
            owner.result = parsedResult;
            owner.resultMessageIndex = i;
            owner.status = status;
            return;
          }

          rawSteps.push({
            kind: "tool",
            id: blockId,
            messageIndex: i,
            resultMessageIndex: i,
            result: parsedResult,
            status,
            sourceMessage: msg,
          });
          return;
        }

        if (block.type === "event") {
          if (block.kind === "aborted") aborted = true;
          const ev: EventItem = {
            id: blockId,
            messageIndex: i,
            eventKind: (block.kind as EventItem["eventKind"]) || "info",
            label: block.text || "事件",
            detail: block.detail,
            url: block.url,
            tone:
              block.kind === "error" || block.kind === "hook"
                ? "danger"
                : "muted",
          };
          const beforeQuestion =
            userIndex !== null
              ? i <= userIndex
              : !rawSteps.some((s) => s.kind !== "event");
          if (beforeQuestion) {
            leadingEvents.push(ev);
          } else {
            rawSteps.push({
              kind: "event",
              id: blockId,
              messageIndex: i,
              event: ev,
            });
          }
        }
      });
    }

    // 末尾纯事件移入 trailingEvents
    let lastWorkIdx = -1;
    rawSteps.forEach((s, idx) => {
      if (s.kind !== "event") lastWorkIdx = idx;
    });
    const trailingEventSteps = rawSteps.splice(lastWorkIdx + 1) as EventStep[];
    const trailingEvents = trailingEventSteps.map((s) => s.event);

    // 已经落定完成的历史轮次中，没有收到 tool_result 的调用视为已执行（unknown/success），不再挂 pending
    for (const s of rawSteps) {
      if (s.kind === "tool" && s.status === "pending" && finalAt !== null) {
        s.status = "success";
      }
    }

    const mergedSteps = mergeConsecutiveSteps(rawSteps, style);
    const chapters: TurnChapter[] = rawChapters.map(({ chapter, at }) => {
      const nextRaw = rawSteps[at];
      const top =
        nextRaw &&
        mergedSteps.find(
          (ms) =>
            ms === nextRaw ||
            (ms.kind === "merged" && ms.children.includes(nextRaw as ToolStep)),
        );
      return top ? { ...chapter, beforeStepId: top.id } : chapter;
    });

    // 统计指标
    const allToolSteps: ToolStep[] = mergedSteps.flatMap((s) =>
      s.kind === "merged" ? s.children : s.kind === "tool" ? [s] : [],
    );
    let commandCount = 0;
    let editCount = 0;
    let errorCount = 0;
    const changedFiles = new Set<string>();
    const toolOrder: string[] = [];
    const toolCounts = new Map<string, number>();

    for (const ts of allToolSteps) {
      const name = ts.call?.rawName ?? "Tool";
      if (!toolCounts.has(name)) toolOrder.push(name);
      toolCounts.set(name, (toolCounts.get(name) ?? 0) + 1);
      if (ts.call?.kind === "shell") commandCount += 1;
      if (ts.status === "error" || ts.status === "interrupted") {
        errorCount += 1;
      }
      if (isChangeStep(ts)) {
        editCount += 1;
        if (ts.call?.title) changedFiles.add(ts.call.title);
      }
    }

    const thinkingCount = mergedSteps.filter(
      (s) => s.kind === "thinking",
    ).length;

    const finalAssistantIndex = finalAt ? finalAt[0] : null;
    const assistantHasDiff =
      finalAssistantIndex !== null &&
      isModificationMessage(messages[finalAssistantIndex]);

    const startTs =
      (userIndex !== null ? messages[userIndex]?.ts : undefined) ?? firstTs;
    const spanMs =
      startTs !== undefined && lastTs !== undefined && lastTs > startTs
        ? lastTs - startTs
        : undefined;
    const stepDurSum = allToolSteps.reduce(
      (sum, s) => sum + (s.result?.durationMs ?? 0),
      0,
    );
    const durationMs =
      spanMs !== undefined && spanMs >= 1000
        ? spanMs
        : stepDurSum > 0
          ? stepDurSum
          : undefined;

    const finalMsg =
      finalAssistantIndex !== null ? messages[finalAssistantIndex] : undefined;
    const joinedFinal = finalTextParts.join("\n\n").trim();
    const finalReplyText =
      finalMsg && joinedFinal && joinedFinal !== finalMsg.content.trim()
        ? joinedFinal
        : undefined;

    return {
      turnKey,
      turnIndex,
      userTurnOrdinal,
      messageIndexes: indexes,
      userIndex,
      stepIndexes: Array.from(stepIndexSet).sort((a, b) => a - b),
      steps: mergedSteps,
      chapters,
      leadingEvents,
      trailingEvents,
      finalAssistantIndex,
      finalReplyText,
      agentStartTs,
      model: turnModel,
      tokens: hasTokens ? totalTokens : undefined,
      costUsd: hasCost ? totalCostUsd : undefined,
      durationMs,
      aborted,
      toolCount: allToolSteps.length,
      commandCount,
      thinkingCount,
      editCount,
      filesChanged: changedFiles.size,
      errorCount,
      hasChanges: editCount > 0 || assistantHasDiff,
      toolChips: toolOrder.slice(0, 4).map((name) => ({
        name,
        count: toolCounts.get(name) ?? 1,
      })),
    };
  });
}

export interface BuildReaderItemsOptions {
  messages: SessionMessage[];
  turns: SessionTurnGroup[];
  viewMode: SessionReaderViewMode;
  expandAllTimelines: boolean;
  expandedTurnKeys: Set<string>;
  collapsedTurnKeys: Set<string>;
  stepOverrides?: ReadonlyMap<string, boolean>;
  matchedMessageSet: Set<number>;
  showInjected?: boolean;
}

export interface ReaderLayout {
  items: ReaderItem[];
  messageIndexToRow: Map<number, number>;
  turnIndexToQuestionRow: Map<number, number>;
  turnIndexToReplyRow: Map<number, number>;
  collapsibleTurnCount: number;
}

export function buildReaderLayout({
  messages,
  turns,
  viewMode,
  expandAllTimelines,
  expandedTurnKeys,
  collapsedTurnKeys,
  stepOverrides,
  matchedMessageSet,
  showInjected = false,
}: BuildReaderItemsOptions): ReaderLayout {
  const items: ReaderItem[] = [];
  const messageIndexToRow = new Map<number, number>();
  const turnIndexToQuestionRow = new Map<number, number>();
  const turnIndexToReplyRow = new Map<number, number>();
  let collapsibleTurnCount = 0;
  let emittedTurns = 0;

  const isStepMatched = (step: TimelineStep): boolean => {
    if (step.kind === "merged") {
      return step.children.some((c) => isStepMatched(c));
    }
    if (matchedMessageSet.has(step.messageIndex)) return true;
    if (
      step.kind === "tool" &&
      step.resultMessageIndex !== undefined &&
      matchedMessageSet.has(step.resultMessageIndex)
    ) {
      return true;
    }
    return false;
  };

  const registerStepMessageIndexes = (step: TimelineStep, rowIndex: number) => {
    if (step.kind === "merged") {
      for (const child of step.children) {
        registerStepMessageIndexes(child, rowIndex);
      }
      return;
    }
    if (!messageIndexToRow.has(step.messageIndex)) {
      messageIndexToRow.set(step.messageIndex, rowIndex);
    }
    if (
      step.kind === "tool" &&
      step.resultMessageIndex !== undefined &&
      !messageIndexToRow.has(step.resultMessageIndex)
    ) {
      messageIndexToRow.set(step.resultMessageIndex, rowIndex);
    }
  };

  for (const turn of turns) {
    if (viewMode === "changes" && !turn.hasChanges && turn.errorCount === 0) {
      continue;
    }

    const turnStartLen = items.length;
    if (emittedTurns > 0) {
      items.push({
        kind: "divider",
        key: `divider-${turn.turnKey}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
      });
    }

    let agentStarted = false;
    const takeAgentStart = () => {
      if (agentStarted) return false;
      agentStarted = true;
      return true;
    };

    const pushQuestion = (idx: number) => {
      const message = messages[idx];
      if (!message) return;
      const rowIndex = items.length;
      items.push({
        kind: "question",
        key: `q-${message.transcriptIndex ?? message.index ?? idx}-${idx}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: idx,
        message,
      });
      messageIndexToRow.set(idx, rowIndex);
      turnIndexToQuestionRow.set(turn.turnIndex, rowIndex);
    };

    const pushChapter = (chapter: TurnChapter) => {
      const rowIndex = items.length;
      const isAgentStart = takeAgentStart();
      if (isAgentStart && !turnIndexToReplyRow.has(turn.turnIndex)) {
        turnIndexToReplyRow.set(turn.turnIndex, rowIndex);
      }
      items.push({
        kind: "chapter",
        key: `chap-${chapter.id}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        chapter,
        isAgentStart,
      });
      if (!messageIndexToRow.has(chapter.messageIndex)) {
        messageIndexToRow.set(chapter.messageIndex, rowIndex);
      }
    };

    const pushStepRows = (
      step: TimelineStep,
      depth: 0 | 1,
      inTimeline: boolean,
      pinned: boolean,
      forceOpen = false,
    ) => {
      const override = stepOverrides?.get(step.id);
      const failure = isFailureStep(step);
      const hit = isStepMatched(step);
      const soleRawTool =
        turn.steps.length === 1 &&
        turn.finalAssistantIndex === null &&
        turn.userIndex === null &&
        step.kind === "tool" &&
        !step.call;
      const expanded = override ?? (forceOpen || failure || hit || soleRawTool);
      const previewLines = hit
        ? Number.MAX_SAFE_INTEGER
        : failure && override === undefined
          ? FAILURE_PREVIEW_LINES
          : STEP_PREVIEW_LINES;

      const rowIndex = items.length;
      const isAgentStart = depth === 0 ? takeAgentStart() : false;
      if (isAgentStart && !turnIndexToReplyRow.has(turn.turnIndex)) {
        turnIndexToReplyRow.set(turn.turnIndex, rowIndex);
      }

      items.push({
        kind: "step",
        key: `s-${step.id}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: step.messageIndex,
        step,
        depth,
        expanded,
        previewLines,
        pinned,
        inTimeline,
        isAgentStart,
      });
      registerStepMessageIndexes(step, rowIndex);

      if (step.kind === "merged" && expanded) {
        for (const child of step.children) {
          pushStepRows(child, 1, inTimeline, false, false);
        }
      }
    };

    const pushFinal = (idx: number) => {
      const message = messages[idx];
      if (!message) return;
      const rowIndex = items.length;
      const isAgentStart = takeAgentStart();
      turnIndexToReplyRow.set(turn.turnIndex, rowIndex);
      items.push({
        kind: "final",
        key: `f-${message.transcriptIndex ?? message.index ?? idx}-${idx}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: idx,
        message,
        overrideText: turn.finalReplyText,
        isAgentStart,
      });
      messageIndexToRow.set(idx, rowIndex);
    };

    if (viewMode === "dialogue") {
      if (turn.userIndex !== null) pushQuestion(turn.userIndex);
      for (const chap of turn.chapters) pushChapter(chap);
      if (turn.finalAssistantIndex !== null) {
        pushFinal(turn.finalAssistantIndex);
      }
      if (items.length > turnStartLen + (emittedTurns > 0 ? 1 : 0)) {
        emittedTurns += 1;
      } else if (emittedTurns > 0) {
        items.pop();
      }
      continue;
    }

    if (viewMode === "changes") {
      if (turn.userIndex !== null) pushQuestion(turn.userIndex);
      const flatCandidates: TimelineStep[] = turn.steps.flatMap((s) =>
        s.kind === "merged" ? s.children : [s],
      );
      for (const s of flatCandidates) {
        if (isChangeStep(s) || isFailureStep(s)) {
          pushStepRows(s, 0, false, false, true);
        }
      }
      if (turn.finalAssistantIndex !== null) {
        pushFinal(turn.finalAssistantIndex);
      }
      if (items.length > turnStartLen + (emittedTurns > 0 ? 1 : 0)) {
        emittedTurns += 1;
      } else if (emittedTurns > 0) {
        items.pop();
      }
      continue;
    }

    // viewMode === "all"
    for (const ev of turn.leadingEvents) {
      const rowIndex = items.length;
      items.push({
        kind: "event",
        key: `ev-${ev.id}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: ev.messageIndex,
        event: ev,
      });
      if (!messageIndexToRow.has(ev.messageIndex)) {
        messageIndexToRow.set(ev.messageIndex, rowIndex);
      }
    }

    if (showInjected) {
      for (const idx of turn.messageIndexes) {
        const msg = messages[idx];
        if (msg?.meta?.injected) {
          const rowIndex = items.length;
          items.push({
            kind: "injected",
            key: `inj-${turn.turnKey}-${idx}`,
            turnIndex: turn.turnIndex,
            turnKey: turn.turnKey,
            messageIndex: idx,
            message: msg,
          });
          messageIndexToRow.set(idx, rowIndex);
        }
      }
    }

    if (turn.userIndex !== null) {
      pushQuestion(turn.userIndex);
    }

    // cc-switch 规则 1、3、4、5：
    // - 0 步：仅输出章节
    // - 1 步且非失败：直接渲染该单行（如单条「✻ 思考中… · 1.2k 字」或单次工具调用），不套摘要条
    // - 2 步及以上（含「思考 + 工具」「思考 + 说明 + 工具」），或单步失败：套「执行过程 · N 步」摘要条！
    const { steps } = turn;
    if (steps.length === 0) {
      for (const chap of turn.chapters) pushChapter(chap);
    } else if (steps.length === 1 && !isFailureStep(steps[0])) {
      for (const chap of turn.chapters.filter(
        (c) => c.beforeStepId === steps[0].id,
      )) {
        pushChapter(chap);
      }
      pushStepRows(steps[0], 0, false, false, false);
      for (const chap of turn.chapters.filter((c) => !c.beforeStepId)) {
        pushChapter(chap);
      }
    } else {
      collapsibleTurnCount += 1;
      const hasSearchHitInSteps = steps.some((s) => isStepMatched(s));
      // 规则 3、4：有最终回复时默认收起；无最终回复、已中断、查找命中或开启「展开全部过程」时默认展开
      const defaultOpen =
        expandAllTimelines ||
        turn.aborted ||
        turn.finalAssistantIndex === null ||
        hasSearchHitInSteps;
      const isExpanded = hasSearchHitInSteps
        ? true
        : expandedTurnKeys.has(turn.turnKey)
          ? true
          : collapsedTurnKeys.has(turn.turnKey)
            ? false
            : defaultOpen;

      if (!isExpanded) {
        for (const chap of turn.chapters) pushChapter(chap);
      }

      const failures = steps.filter(isFailureStep);
      const pinnedFailures = isExpanded
        ? []
        : failures.slice(0, MAX_PINNED_FAILURES);
      const hiddenFailures = isExpanded
        ? 0
        : Math.max(0, failures.length - pinnedFailures.length);

      const timelineRowIndex = items.length;
      const isAgentStart = takeAgentStart();
      if (!turnIndexToReplyRow.has(turn.turnIndex)) {
        turnIndexToReplyRow.set(turn.turnIndex, timelineRowIndex);
      }
      items.push({
        kind: "timeline",
        key: `timeline-${turn.turnKey}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        turn,
        expanded: isExpanded,
        hiddenFailures,
        isAgentStart,
      });

      if (isExpanded) {
        for (const s of steps) {
          for (const chap of turn.chapters.filter(
            (c) => c.beforeStepId === s.id,
          )) {
            pushChapter(chap);
          }
          pushStepRows(s, 0, true, false, false);
        }
        for (const chap of turn.chapters.filter((c) => !c.beforeStepId)) {
          pushChapter(chap);
        }
      } else {
        for (const s of pinnedFailures) {
          pushStepRows(s, 0, true, true, false);
        }
        for (const s of steps) {
          registerStepMessageIndexes(s, timelineRowIndex);
        }
      }
    }

    if (turn.finalAssistantIndex !== null) {
      pushFinal(turn.finalAssistantIndex);
    }

    for (const ev of turn.trailingEvents) {
      const rowIndex = items.length;
      items.push({
        kind: "event",
        key: `ev-${ev.id}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: ev.messageIndex,
        event: ev,
      });
      if (!messageIndexToRow.has(ev.messageIndex)) {
        messageIndexToRow.set(ev.messageIndex, rowIndex);
      }
    }

    if (items.length > turnStartLen + (emittedTurns > 0 ? 1 : 0)) {
      emittedTurns += 1;
    } else if (emittedTurns > 0) {
      items.pop();
    }
  }

  return {
    items,
    messageIndexToRow,
    turnIndexToQuestionRow,
    turnIndexToReplyRow,
    collapsibleTurnCount,
  };
}
