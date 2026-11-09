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
   * `expectedChars` / `expectedIdentity` 描述列表里那一版正文：会话在两次请求之间
   * 被改写时，后端会拒绝返回并提示重新打开。长度单独不足以确认身份，摘要才是判据。
   */
  async getMessageContent(
    providerId: string,
    sourcePath: string,
    index: number,
    expectedChars: number,
    expectedIdentity?: string,
  ): Promise<string> {
    return await invoke("get_message_content", {
      providerId,
      sourcePath,
      index,
      expectedChars,
      expectedIdentity,
    });
  },

  /**
   * 分块流式读取会话正文：每收到一块就通过 `onProgress` 交出累计结果，供界面边到
   * 边渲染。invoke 完成不代表 Channel 已送达（大包经异步 fetch 传输）；只有命令
   * 成功且收到完整的 header/messages/done 才返回独立快照。停滞或取消会结束等待。
   */
  async streamMessages(
    providerId: string,
    sourcePath: string,
    onProgress?: (messages: SessionMessage[]) => void,
    signal?: AbortSignal,
  ): Promise<SessionMessage[]> {
    if (signal?.aborted) {
      throw new DOMException("会话读取已取消", "AbortError");
    }
    const channel = new Channel<TranscriptChunk>();
    return new Promise<SessionMessage[]>((resolve, reject) => {
      const messages: SessionMessage[] = [];
      let total: number | undefined;
      let done = false;
      let invoked = false;
      let settled = false;
      let timer: ReturnType<typeof setTimeout>;

      const cleanup = () => {
        settled = true;
        clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
        // 传输层仍可收到迟到的包，但不得再改动结果或触发界面更新。
        channel.onmessage = () => {};
      };
      const fail = (error: unknown) => {
        if (settled) return;
        cleanup();
        reject(error);
      };
      const abort = () =>
        fail(new DOMException("会话读取已取消", "AbortError"));
      const armTimeout = () => {
        clearTimeout(timer);
        // 空会话同样必须有 header + done；缺包/命令停滞不应永久显示加载中。
        timer = setTimeout(
          () => fail(new Error("会话读取超时：消息流未完整结束，请重试")),
          30_000,
        );
      };
      const complete = () => {
        if (settled || !invoked || !done) return;
        const snapshot = messages.slice();
        cleanup();
        resolve(snapshot);
      };
      const protocolError = (detail: string) =>
        new Error(`会话消息流不完整：${detail}`);
      const isCount = (value: number) =>
        Number.isSafeInteger(value) && value >= 0;

      signal?.addEventListener("abort", abort, { once: true });
      armTimeout();
      channel.onmessage = (chunk) => {
        if (settled) return;
        try {
          if (chunk.kind === "error") throw new Error(chunk.message);
          if (done) throw protocolError("done 后收到额外数据");
          switch (chunk.kind) {
            case "header":
              if (total !== undefined || !isCount(chunk.total)) {
                throw protocolError("header total 无效或重复");
              }
              total = chunk.total;
              break;
            case "messages":
              if (
                total === undefined ||
                !isCount(chunk.start) ||
                chunk.start !== messages.length ||
                !Array.isArray(chunk.messages) ||
                messages.length + chunk.messages.length > total
              ) {
                throw protocolError("messages start/total 不匹配");
              }
              for (const message of chunk.messages) messages.push(message);
              onProgress?.(messages.slice());
              break;
            case "done":
              if (
                total === undefined ||
                !isCount(chunk.delivered) ||
                chunk.delivered !== total ||
                messages.length !== total
              ) {
                throw protocolError("done delivered/total 与已收消息不匹配");
              }
              done = true;
              break;
            default:
              throw protocolError("未知分块类型");
          }
          // onProgress 的调用方也可能同步取消请求。
          if (settled) return;
          armTimeout();
          complete();
        } catch (error) {
          fail(error);
        }
      };
      // 同时观察两条完成路径；Done 即使先到，也不能吞掉命令稍后的 rejection。
      try {
        void invoke("stream_session_messages", {
          providerId,
          sourcePath,
          onChunk: channel,
        }).then(() => {
          if (settled) return;
          invoked = true;
          armTimeout();
          complete();
        }, fail);
      } catch (error) {
        fail(error);
      }
    });
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
