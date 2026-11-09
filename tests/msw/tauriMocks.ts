import crossFetch, {
  Headers as CrossFetchHeaders,
  Request as CrossFetchRequest,
  Response as CrossFetchResponse,
} from "cross-fetch";
import { vi } from "vitest";
import { server } from "./server";

const TAURI_ENDPOINT = "http://tauri.local";

globalThis.fetch = crossFetch as typeof fetch;
globalThis.Headers = CrossFetchHeaders as typeof Headers;
globalThis.Request = CrossFetchRequest as typeof Request;
globalThis.Response = CrossFetchResponse as typeof Response;

/** 传输层入口：index 是 Channel 包序号，不是 TranscriptChunk.start。 */
export const deliverChannelMessage = (
  channel: unknown,
  index: number,
  message: unknown,
) => {
  (channel as { receive: (raw: unknown) => void }).receive({ index, message });
};

vi.mock("@tauri-apps/api/core", () => {
  /** 镜像 @tauri-apps/api 2.8 Channel 的索引缓冲和 end 清理行为。 */
  class Channel<T> {
    onmessage: (message: T) => void = () => {};
    private nextIndex = 0;
    private pending = new Map<number, T>();
    private endIndex?: number;
    private closed = false;

    receive(raw: { index: number; message: T } | { index: number; end: true }) {
      if (this.closed) return;
      if ("end" in raw) {
        this.endIndex = raw.index;
        this.closed = raw.index === this.nextIndex;
        return;
      }
      if (raw.index !== this.nextIndex) {
        this.pending.set(raw.index, raw.message);
        return;
      }
      this.onmessage(raw.message);
      this.nextIndex++;
      while (this.pending.has(this.nextIndex)) {
        this.onmessage(this.pending.get(this.nextIndex)!);
        this.pending.delete(this.nextIndex++);
      }
      this.closed = this.nextIndex === this.endIndex;
    }
  }

  /** 后端的 150 条/块粒度，测试替身按同一边界推块。 */
  const STREAM_CHUNK_MESSAGES = 150;

  const callCommand = async (
    command: string,
    payload: Record<string, unknown>,
  ) => {
    const response = await fetch(`${TAURI_ENDPOINT}/${command}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify(payload ?? {}),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Invoke failed for ${command}`);
    }

    const text = await response.text();
    if (!text) return undefined;
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  };

  return {
    Channel,
    invoke: async (command: string, payload: Record<string, unknown> = {}) => {
      if (command !== "stream_session_messages") {
        return callCommand(command, payload);
      }

      // Channel 无法过 HTTP：复用 get_session_messages 的 handler 取数据，
      // 再按后端的分块契约逐块推给 onChunk
      const { providerId, sourcePath, onChunk } = payload as {
        providerId: string;
        sourcePath: string;
        onChunk?: Channel<unknown>;
      };
      const messages =
        ((await callCommand("get_session_messages", {
          providerId,
          sourcePath,
        })) as Array<unknown> | undefined) ?? [];

      let index = 0;
      const send = (message: unknown) => {
        const raw = { index: index++, message };
        // >=8192 字节的 Channel 包由异步 fetch 送达，invoke/小包可先完成。
        if (new TextEncoder().encode(JSON.stringify(message)).length >= 8192) {
          setTimeout(() => onChunk?.receive(raw), 0);
        } else {
          onChunk?.receive(raw);
        }
      };
      send({ kind: "header", total: messages.length, approxBytes: 0 });
      for (
        let start = 0;
        start < messages.length;
        start += STREAM_CHUNK_MESSAGES
      ) {
        send({
          kind: "messages",
          start,
          messages: messages.slice(start, start + STREAM_CHUNK_MESSAGES),
        });
      }
      send({ kind: "done", delivered: messages.length });
      onChunk?.receive({ index, end: true });
      return undefined;
    },
  };
});

const listeners = new Map<string, Set<(event: { payload: unknown }) => void>>();

const ensureListenerSet = (event: string) => {
  if (!listeners.has(event)) {
    listeners.set(event, new Set());
  }
  return listeners.get(event)!;
};

export const emitTauriEvent = (event: string, payload: unknown) => {
  const handlers = listeners.get(event);
  handlers?.forEach((handler) => handler({ payload }));
};

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => {
    const set = ensureListenerSet(event);
    set.add(handler);
    return () => {
      set.delete(handler);
    };
  },
}));

// Ensure the MSW server is referenced so tree shaking doesn't remove imports
void server;

vi.mock("@tauri-apps/api/path", () => ({
  homeDir: async () => "/home/mock",
  join: async (...segments: string[]) => segments.join("/"),
}));
