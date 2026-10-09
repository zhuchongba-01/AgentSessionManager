import { Channel, invoke } from "@tauri-apps/api/core";
import type { SessionMessage, SessionMeta } from "@/types";

export interface DeleteSessionOptions {
  providerId: string;
  sessionId: string;
  sourcePath: string;
  includeProject?: boolean;
}

/** 后端 `TranscriptChunk` 的分块消息，字段一一对应。 */
export type TranscriptChunk =
  | { kind: "header"; total: number; approxBytes: number }
  | { kind: "messages"; start: number; messages: SessionMessage[] }
  | { kind: "done"; delivered: number }
  | { kind: "error"; message: string };

/** 后端 delete_session 的结构化结果（tag + snake_case 变体名）。 */
export type DeleteSessionReply =
  | { status: "deleted"; warnings?: string[] }
  | { status: "not_found" }
  | { status: "cleanup_pending"; warnings: string[] };

export interface DeleteSessionResult extends DeleteSessionOptions {
  success: boolean;
  error?: string;
  warnings?: string[];
}

export const sessionsApi = {
  async list(
    onWarnings?: (warnings: string[]) => void,
  ): Promise<SessionMeta[]> {
    const report = await invoke<
      { sessions: SessionMeta[]; warnings: string[] } | SessionMeta[]
    >("list_sessions");
    if (Array.isArray(report)) {
      onWarnings?.([]);
      return report;
    }
    onWarnings?.(report.warnings);
    return report.sessions;
  },

  async getMessages(
    providerId: string,
    sourcePath: string,
  ): Promise<SessionMessage[]> {
    return await invoke("get_session_messages", { providerId, sourcePath });
  },

  /**
   * 取回一条被截断消息的全文。
   *
   * `expectedChars` 是列表里那一版正文的长度：会话在两次请求之间被改写时，同一下标
   * 可能已指向另一条消息，后端会拒绝返回并提示重新打开。
   */
  async getMessageContent(
    providerId: string,
    sourcePath: string,
    index: number,
    expectedChars: number,
  ): Promise<string> {
    return await invoke("get_message_content", {
      providerId,
      sourcePath,
      index,
      expectedChars,
    });
  },

  /**
   * 分块流式读取会话正文：每收到一块就通过 `onProgress` 交出累计结果，供界面边到
   * 边渲染；函数在流结束后返回完整消息。失败时命令会 reject，错误文案与后端一致。
   */
  async streamMessages(
    providerId: string,
    sourcePath: string,
    onProgress?: (messages: SessionMessage[]) => void,
  ): Promise<SessionMessage[]> {
    const channel = new Channel<TranscriptChunk>();
    const messages: SessionMessage[] = [];
    channel.onmessage = (chunk) => {
      if (chunk.kind !== "messages") return;
      messages.push(...chunk.messages);
      onProgress?.(messages.slice());
    };
    await invoke("stream_session_messages", {
      providerId,
      sourcePath,
      onChunk: channel,
    });
    return messages;
  },

  async delete(options: DeleteSessionOptions): Promise<DeleteSessionReply> {
    const {
      providerId,
      sessionId,
      sourcePath,
      includeProject = false,
    } = options;
    return await invoke("delete_session", {
      providerId,
      sessionId,
      sourcePath,
      includeProject,
    });
  },

  async deleteMany(
    items: DeleteSessionOptions[],
  ): Promise<DeleteSessionResult[]> {
    return await invoke("delete_sessions", { items });
  },
};
