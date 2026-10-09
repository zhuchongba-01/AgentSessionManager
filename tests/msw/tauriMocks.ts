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

vi.mock("@tauri-apps/api/core", () => {
  /** 与后端 `tauri::ipc::Channel` 对应的测试替身：只保留 onmessage 回调。 */
  class Channel<T> {
    onmessage: ((message: T) => void) | null = null;
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
        onChunk?: { onmessage?: (chunk: unknown) => void };
      };
      const messages =
        ((await callCommand("get_session_messages", {
          providerId,
          sourcePath,
        })) as Array<unknown> | undefined) ?? [];

      onChunk?.onmessage?.({
        kind: "header",
        total: messages.length,
        approxBytes: 0,
      });
      for (
        let start = 0;
        start < messages.length;
        start += STREAM_CHUNK_MESSAGES
      ) {
        onChunk?.onmessage?.({
          kind: "messages",
          start,
          messages: messages.slice(start, start + STREAM_CHUNK_MESSAGES),
        });
      }
      onChunk?.onmessage?.({ kind: "done", delivered: messages.length });
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
