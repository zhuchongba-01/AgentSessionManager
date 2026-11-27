export interface SessionMeta {
  providerId: string;
  sessionId: string;
  residual?: boolean;
  /**
   * 父会话的原生 ID（子 Agent 会话 / 分支会话）。
   */
  parentSessionId?: string;
  archived?: boolean;
  cleanupPending?: boolean;
  title?: string;
  summary?: string;
  projectDir?: string | null;
  projectName?: string | null;
  sidebarSection?: { id: string; name: string } | null;
  createdAt?: number;
  lastActiveAt?: number;
  sourcePath?: string;
}

export type ToolKind =
  | "shell"
  | "read"
  | "search"
  | "edit"
  | "write"
  | "web"
  | "mcp"
  | "agent"
  | "ask"
  | "todo"
  | "other";

export type ToolStatus =
  | "success"
  | "error"
  | "interrupted"
  | "pending"
  | "unknown";

export type EventKind =
  | "aborted"
  | "compaction"
  | "model_change"
  | "thinking_level"
  | "hook"
  | "pr_link"
  | "slash_command"
  | "info"
  | "error"
  | "sub_agent"
  | "goal"
  | "other";

export type StepPhase = "start" | "finish";

export type DiffOp = "add" | "update" | "delete" | "rename";

export interface DiffFile {
  path: string;
  op: DiffOp;
  added: number;
  removed: number;
}

export interface DiffSummary {
  files: DiffFile[];
  added: number;
  removed: number;
  full?: ContentRef;
}

export type ContentRef =
  | {
      kind: "jsonl";
      offset: number;
      len: number;
      pointer: string;
      segment?: number;
    }
  | {
      kind: "sqlite";
      table: string;
      id: string;
      column: string;
      pointer: string;
    }
  | {
      kind: "file";
      relPath: string;
      pointer: string;
    }
  | {
      kind: "sidecar";
      relPath: string;
    };

export type ImageSource =
  | { kind: "inline"; content: ContentRef }
  | { kind: "local_file"; path: string };

export interface ImageRef {
  source: ImageSource;
  mediaType: string;
  size: number;
  alt?: string;
}

export interface TextBlock {
  type: "text";
  text: string;
  full?: ContentRef;
}

export interface ThinkingBlock {
  type: "thinking";
  text: string;
  summary?: string;
  redacted?: boolean;
  durationMs?: number;
  full?: ContentRef;
}

export interface ToolCallBlock {
  type: "tool_call";
  id: string;
  rawName: string;
  kind: ToolKind;
  title: string;
  detail?: string;
  server?: string;
  inputPreview: string;
  inputTotalLen: number;
  inputFull?: ContentRef;
  diff?: DiffSummary;
  byUser?: boolean;
}

export interface ToolResultBlock {
  type: "tool_result";
  callId: string;
  status: ToolStatus;
  preview: string;
  totalLen: number;
  lineCount: number;
  truncated: boolean;
  full?: ContentRef;
  exitCode?: number;
  durationMs?: number;
  images?: ImageRef[];
  savedPath?: string;
}

export interface ImageBlock {
  type: "image";
  image: ImageRef;
}

export interface EventBlock {
  type: "event";
  kind: EventKind;
  text?: string;
  url?: string;
  full?: ContentRef;
}

export interface StepBlock {
  type: "step";
  phase: StepPhase;
  tokens?: number;
  costUsd?: number;
  reason?: string;
}

export type SessionBlock =
  | TextBlock
  | ThinkingBlock
  | ToolCallBlock
  | ToolResultBlock
  | ImageBlock
  | EventBlock
  | StepBlock;

export interface MessageMeta {
  model?: string;
  inputTokens?: number;
  outputTokens?: number;
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
  reasoningTokens?: number;
  costUsd?: number;
  durationMs?: number;
  stopReason?: string;
}

export interface TurnIndex {
  turnId: string;
  firstMessageIndex: number;
  lastMessageIndex: number;
  questionPreview: string;
  ts?: number;
  stepCount: number;
  errorCount: number;
  hasFinalReply: boolean;
  aborted: boolean;
}

export interface SessionMessage {
  role: string;
  content: string;
  ts?: number;
  id?: string;
  turnId?: string;
  injected?: boolean;
  blocks?: SessionBlock[];
  meta?: MessageMeta;
  /** 正文被截断时用于取回全文的下标（在会话正文里的绝对位置） */
  index?: number;
  /** 该条消息在后端完整会话记录中的绝对下标（0..total-1），用于会话内全文搜索定位 */
  transcriptIndex?: number;
  /** 该条正文只下发了预览：展开或复制前需要先取回全文 */
  truncated?: boolean;
  /** 全文的字符数，用于提示大小 */
  contentChars?: number;
  /** 全文的身份摘要（16 进制字符串） */
  contentIdentity?: string;
}

export interface ContentHighlight {
  start: number;
  end: number;
}

export type SessionTextRange = ContentHighlight;

export interface ContentSnippet {
  messageIndex: number;
  text: string;
  highlights: ContentHighlight[];
}

export interface SessionContentHit {
  providerId: string;
  sourcePath: string;
  snippets: ContentSnippet[];
  matchCount: number;
}

export interface SessionIndexStatus {
  running: boolean;
  processed: number;
  total: number;
}

export interface ContentSearchResult {
  hits: SessionContentHit[];
  status: SessionIndexStatus;
}

export type SessionContentSearchResult = ContentSearchResult;
