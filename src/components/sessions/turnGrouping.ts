import type { SessionMessage } from "@/types";
import { classifyToolKind } from "./agentReaderStyles";
import { extractDiffPayload } from "./SessionDiffView";
import type { SessionReaderViewMode } from "./types";

const TOOL_HEADER_RE = /^\[Tool:\s*([^\]]+)\]/i;
const WRITE_TOOL_NAMES = new Set([
  "edit",
  "multiedit",
  "write",
  "write_file",
  "edit_file",
  "replace",
  "notebookedit",
  "apply_patch",
  "patch",
  "create_file",
  "str_replace_editor",
  "str_replace_based_edit_tool",
]);
const DIFF_CONTENT_RE =
  /(?:^|\n)(?:```(?:diff|patch)\b|\*\*\* (?:Update|Add|Delete) File:|@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@)/m;
const ERROR_CONTENT_RE =
  /(?:^|\n)(?:Exit code\s+[1-9]\d*|\[(?:error|failed|Web search error)[^\]]*\]|Error:|Traceback \(most recent call last\):|thread '.*' panicked at)/i;

export interface ToolCountChip {
  name: string;
  count: number;
}

export interface SessionTurnGroup {
  turnKey: string;
  turnIndex: number;
  /** 第几个含用户提问的轮次（从 1 开始，用于「跳转到第 N 个用户问题」无障碍标签） */
  userTurnOrdinal: number | null;
  /** 本轮在 displayedMessages 里的下标集合（有序） */
  messageIndexes: number[];
  userIndex: number | null;
  /** 中间过程消息下标（thinking / tool / 最终回复之前的中间说明） */
  stepIndexes: number[];
  finalAssistantIndex: number | null;
  /** 本轮 Agent 侧首条消息的时间戳 */
  agentStartTs?: number;
  toolCount: number;
  commandCount: number;
  thinkingCount: number;
  editCount: number;
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
      isAgentStart: boolean;
    }
  | {
      kind: "step";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      message: SessionMessage;
      inTimeline: boolean;
      standaloneOpen: boolean;
      isAgentStart: boolean;
    }
  | {
      kind: "final";
      key: string;
      turnIndex: number;
      turnKey: string;
      messageIndex: number;
      message: SessionMessage;
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
  const match = content.match(/(?:^|\n)Exit code[:\s]+(\d+)\b/i);
  if (!match) return undefined;
  const parsed = Number(match[1]);
  return Number.isFinite(parsed) ? parsed : undefined;
}

export function isModificationMessage(message: SessionMessage): boolean {
  const role = message.role.toLowerCase();
  if (role === "user" || role === "thinking") return false;
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
  if (role === "user" || role === "assistant" || role === "thinking") {
    return false;
  }
  return ERROR_CONTENT_RE.test(message.content);
}

export function groupMessagesIntoTurns(
  messages: SessionMessage[],
): SessionTurnGroup[] {
  if (messages.length === 0) return [];

  const rawSlices: Array<{ start: number; end: number }> = [];
  let currentStart = 0;

  for (let i = 0; i < messages.length; i += 1) {
    const isUser = messages[i].role.toLowerCase() === "user";
    if (isUser && i > currentStart) {
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

    const userIndex =
      messages[start]?.role.toLowerCase() === "user" ? start : null;
    const userTurnOrdinal =
      userIndex !== null ? (userOrdinalCounter += 1) : null;

    let finalAssistantIndex: number | null = null;
    for (let i = end - 1; i >= start; i -= 1) {
      const role = messages[i].role.toLowerCase();
      if (role === "user") break;
      if (role === "assistant") {
        const hasLaterTool = indexes.some(
          (idx) =>
            idx > i &&
            !["user", "assistant", "thinking"].includes(
              messages[idx].role.toLowerCase(),
            ),
        );
        if (!hasLaterTool) {
          finalAssistantIndex = i;
        }
        break;
      }
    }

    const stepIndexes = indexes.filter(
      (idx) => idx !== userIndex && idx !== finalAssistantIndex,
    );

    let toolCount = 0;
    let commandCount = 0;
    let thinkingCount = 0;
    let editCount = 0;
    let errorCount = 0;
    let assistantHasDiff = false;
    let agentStartTs: number | undefined;
    const toolOrder: string[] = [];
    const toolCounts = new Map<string, number>();

    for (const idx of stepIndexes) {
      const msg = messages[idx];
      if (agentStartTs === undefined && msg.ts !== undefined) {
        agentStartTs = msg.ts;
      }
      const role = msg.role.toLowerCase();
      if (role === "thinking") {
        thinkingCount += 1;
      } else if (role !== "assistant" && role !== "user") {
        toolCount += 1;
        const name = extractToolName(msg.content) ?? "Tool";
        if (classifyToolKind(name) === "shell") {
          commandCount += 1;
        }
        if (!toolCounts.has(name)) toolOrder.push(name);
        toolCounts.set(name, (toolCounts.get(name) ?? 0) + 1);
        if (isModificationMessage(msg)) editCount += 1;
        if (isErrorToolMessage(msg)) errorCount += 1;
      } else if (isModificationMessage(msg)) {
        editCount += 1;
      }
    }

    if (
      agentStartTs === undefined &&
      finalAssistantIndex !== null &&
      messages[finalAssistantIndex]?.ts !== undefined
    ) {
      agentStartTs = messages[finalAssistantIndex].ts;
    }

    if (
      finalAssistantIndex !== null &&
      isModificationMessage(messages[finalAssistantIndex])
    ) {
      assistantHasDiff = true;
    }

    const toolChips: ToolCountChip[] = toolOrder.slice(0, 4).map((name) => ({
      name,
      count: toolCounts.get(name) ?? 1,
    }));

    const anchorMsg = messages[userIndex ?? start];
    const anchorIdentity =
      anchorMsg?.transcriptIndex ?? anchorMsg?.index ?? start;

    return {
      turnKey: `turn-${turnIndex}-${anchorIdentity}`,
      turnIndex,
      userTurnOrdinal,
      messageIndexes: indexes,
      userIndex,
      stepIndexes,
      finalAssistantIndex,
      agentStartTs,
      toolCount,
      commandCount,
      thinkingCount,
      editCount,
      errorCount,
      hasChanges: editCount > 0 || assistantHasDiff,
      toolChips,
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
  matchedMessageSet: Set<number>;
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
  matchedMessageSet,
}: BuildReaderItemsOptions): ReaderLayout {
  const items: ReaderItem[] = [];
  const messageIndexToRow = new Map<number, number>();
  const turnIndexToQuestionRow = new Map<number, number>();
  const turnIndexToReplyRow = new Map<number, number>();
  let collapsibleTurnCount = 0;
  let emittedTurns = 0;

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

    const pushStep = (
      idx: number,
      inTimeline: boolean,
      standaloneOpen = false,
    ) => {
      const message = messages[idx];
      if (!message) return;
      const rowIndex = items.length;
      const isAgentStart = takeAgentStart();
      if (isAgentStart && !turnIndexToReplyRow.has(turn.turnIndex)) {
        turnIndexToReplyRow.set(turn.turnIndex, rowIndex);
      }
      items.push({
        kind: "step",
        key: `s-${message.transcriptIndex ?? message.index ?? idx}-${idx}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        messageIndex: idx,
        message,
        inTimeline,
        standaloneOpen,
        isAgentStart,
      });
      messageIndexToRow.set(idx, rowIndex);
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
        isAgentStart,
      });
      messageIndexToRow.set(idx, rowIndex);
    };

    if (viewMode === "dialogue") {
      if (turn.userIndex !== null) {
        pushQuestion(turn.userIndex);
      }
      if (turn.finalAssistantIndex !== null) {
        pushFinal(turn.finalAssistantIndex);
      } else {
        for (const idx of turn.stepIndexes) {
          if (messages[idx]?.role.toLowerCase() === "assistant") {
            pushFinal(idx);
          }
        }
      }
      if (items.length > turnStartLen + (emittedTurns > 0 ? 1 : 0)) {
        emittedTurns += 1;
      } else if (emittedTurns > 0) {
        items.pop();
      }
      continue;
    }

    if (viewMode === "changes") {
      if (turn.userIndex !== null) {
        pushQuestion(turn.userIndex);
      }
      for (const idx of turn.stepIndexes) {
        const msg = messages[idx];
        if (msg && (isModificationMessage(msg) || isErrorToolMessage(msg))) {
          pushStep(idx, false, true);
        }
      }
      if (turn.finalAssistantIndex !== null) {
        pushFinal(turn.finalAssistantIndex);
      }
      emittedTurns += 1;
      continue;
    }

    // viewMode === "all"
    if (turn.userIndex !== null) {
      pushQuestion(turn.userIndex);
    }

    const isCollapsible =
      turn.toolCount >= 2 && turn.finalAssistantIndex !== null;

    if (!isCollapsible) {
      // 单步且没有最终回复、且无 [Tool: Name] 头部的裸输出默认展开，便于直接阅读
      const soleRawTool =
        turn.stepIndexes.length === 1 &&
        turn.finalAssistantIndex === null &&
        turn.userIndex === null;
      for (const idx of turn.stepIndexes) {
        pushStep(idx, false, soleRawTool);
      }
    } else {
      collapsibleTurnCount += 1;
      const hasSearchHitInSteps = turn.stepIndexes.some((idx) =>
        matchedMessageSet.has(idx),
      );
      const defaultOpen =
        expandAllTimelines || turn.errorCount > 0 || hasSearchHitInSteps;
      const isExpanded = hasSearchHitInSteps
        ? true
        : expandedTurnKeys.has(turn.turnKey)
          ? true
          : collapsedTurnKeys.has(turn.turnKey)
            ? false
            : defaultOpen;

      const timelineRowIndex = items.length;
      const isAgentStart = takeAgentStart();
      turnIndexToReplyRow.set(turn.turnIndex, timelineRowIndex);
      items.push({
        kind: "timeline",
        key: `timeline-${turn.turnKey}`,
        turnIndex: turn.turnIndex,
        turnKey: turn.turnKey,
        turn,
        expanded: isExpanded,
        isAgentStart,
      });

      if (isExpanded) {
        for (const idx of turn.stepIndexes) {
          pushStep(idx, true, false);
        }
      } else {
        for (const idx of turn.stepIndexes) {
          messageIndexToRow.set(idx, timelineRowIndex);
        }
      }
    }

    if (turn.finalAssistantIndex !== null) {
      pushFinal(turn.finalAssistantIndex);
    }

    emittedTurns += 1;
  }

  return {
    items,
    messageIndexToRow,
    turnIndexToQuestionRow,
    turnIndexToReplyRow,
    collapsibleTurnCount,
  };
}
