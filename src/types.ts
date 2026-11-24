export interface SessionMeta {
  providerId: string;
  sessionId: string;
  residual?: boolean;
  /**
   * 父会话的原生 ID（子 Agent 会话 / 分支会话）。
   *
   * 它是「归属」的唯一线索：子会话在列表里常常只显示为「残留」，用户无法判断
   * 该删哪一条；删除父会话时也靠它把整条子会话链一次带上。
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
  resumeCommand?: string;
}

export type ReaderBlock =
  | {
      type: "text";
      text: string;
    }
  | {
      type: "thinking";
      text: string;
      summary?: string;
      redacted?: boolean;
      durationMs?: number;
    }
  | {
      type: "tool_call";
      id: string;
      rawName: string;
      server?: string;
      title: string;
      detail?: string;
      inputPreview?: string;
      inputTruncated?: boolean;
      diffAdded?: number;
      diffRemoved?: number;
      diffText?: string;
    }
  | {
      type: "tool_result";
      callId: string;
      status: "success" | "error" | "interrupted" | string;
      output: string;
      lineCount: number;
      exitCode?: number;
      durationMs?: number;
    }
  | {
      type: "event";
      kind: string;
      text?: string;
      detail?: string;
      url?: string;
    };

export interface MessageReaderMeta {
  model?: string;
  thinkingSummary?: string;
  redacted?: boolean;
  durationMs?: number;
  tokens?: number;
  costUsd?: number;
  injected?: boolean;
  blocks?: ReaderBlock[];
}

export interface SessionMessage {
  role: string;
  content: string;
  ts?: number;
  meta?: MessageReaderMeta;
  /** 正文被截断时用于取回全文的下标（在会话正文里的绝对位置） */
  index?: number;
  /** 该条消息在后端完整会话记录中的绝对下标（0..total-1），用于会话内全文搜索定位 */
  transcriptIndex?: number;
  /** 该条正文只下发了预览：展开或复制前需要先取回全文 */
  truncated?: boolean;
  /** 全文的字符数，用于提示大小 */
  contentChars?: number;
  /**
   * 全文的身份摘要（16 进制字符串）。取回全文时带回后端校验，确认还是同一条消息
   * ——只比长度不够，会话改写后同一下标可能换成另一条长度恰好相同的消息。
   * 用字符串而非数字：64 位摘要会超出 JS 整数的精确范围。
   */
  contentIdentity?: string;
}

export interface ContentHighlight {
  start: number;
  end: number;
}

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
