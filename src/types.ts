export interface SessionMeta {
  providerId: string;
  sessionId: string;
  residual?: boolean;
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

export interface SessionMessage {
  role: string;
  content: string;
  ts?: number;
  /** 正文被截断时用于取回全文的下标（在会话正文里的绝对位置） */
  index?: number;
  /** 该条正文只下发了预览：展开或复制前需要先取回全文 */
  truncated?: boolean;
  /** 全文的字符数，用于提示大小并在取全文时校验会话是否已变化 */
  contentChars?: number;
}
