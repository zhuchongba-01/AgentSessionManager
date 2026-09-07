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
  createdAt?: number;
  lastActiveAt?: number;
  sourcePath?: string;
  resumeCommand?: string;
}

export interface SessionMessage {
  role: string;
  content: string;
  ts?: number;
}
