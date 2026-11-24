import { afterEach, describe, expect, it, vi } from "vitest";
import * as core from "@tauri-apps/api/core";

import { sessionsApi, type TranscriptChunk } from "@/lib/api/sessions";
import { deliverChannelMessage } from "../msw/tauriMocks";

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

function controlledStream() {
  let channel: unknown;
  let resolve!: () => void;
  let reject!: (error: unknown) => void;
  const command = new Promise<void>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  const invoke = vi
    .spyOn(core, "invoke")
    .mockImplementation((_command, args) => {
      channel = (args as { onChunk: unknown }).onChunk;
      return command as Promise<never>;
    });
  return {
    invoke,
    resolve,
    reject,
    send: (index: number, chunk: TranscriptChunk) =>
      deliverChannelMessage(channel, index, chunk),
  };
}
import { contentIdentityHex, setSessionFixtures } from "../msw/state";

const SOURCE_PATH = "/mock/stream/session.jsonl";

describe("contentIdentityHex 镜像", () => {
  /**
   * 与 Rust `wire::content_identity_hex` 的固定向量逐字节对齐。这组值也写在
   * src-tauri/src/session_manager/wire.rs 的同名测试里：任一侧改了算法都会失败，
   * 避免前端 mock 与后端实现"各自自洽"地漂移。
   */
  it("与 Rust 侧固定向量一致", () => {
    expect(
      [
        { role: "tool", content: "hello" },
        { role: "user", content: "", ts: 0 },
        { role: "assistant", content: "中", ts: 1_700_000_000_000 },
        { role: "tool", content: "混合 mixed 内容", ts: -1 },
      ].map((message) => contentIdentityHex(message)),
    ).toEqual([
      "89d353117a8bfbab",
      "cfae74179de2f5f6",
      "b9aa5e9f6edb5fe8",
      "3de0a87aad98a68b",
    ]);
  });

  it("同长度不同正文得到不同身份", () => {
    const a = "AAAA".repeat(1100);
    const b = "BBBB".repeat(1100);
    expect(a.length).toBe(b.length);
    expect(contentIdentityHex({ role: "tool", content: a })).not.toBe(
      contentIdentityHex({ role: "tool", content: b }),
    );
  });
});

describe("sessionsApi.streamMessages", () => {
  it("按块累计并返回完整消息", async () => {
    const messages = Array.from({ length: 320 }, (_, index) => ({
      role: "user",
      content: `message-${index}`,
    }));
    setSessionFixtures(
      [{ providerId: "codex", sessionId: "stream-1", sourcePath: SOURCE_PATH }],
      { [`codex:${SOURCE_PATH}`]: messages },
    );

    const progress: number[] = [];
    const result = await sessionsApi.streamMessages(
      "codex",
      SOURCE_PATH,
      (partial) => progress.push(partial.length),
    );

    expect(result).toHaveLength(320);
    expect(result[0].content).toBe("message-0");
    expect(result[0].transcriptIndex).toBe(0);
    expect(result[150].transcriptIndex).toBe(150);
    expect(result[319].content).toBe("message-319");
    expect(result[319].transcriptIndex).toBe(319);
    // 每块最多 150 条：320 条分三块，进度必须单调递增且以全量收尾
    expect(progress).toEqual([150, 300, 320]);
  });

  it("没有消息时直接返回空列表", async () => {
    setSessionFixtures([], {});

    const progress: number[] = [];
    const result = await sessionsApi.streamMessages(
      "codex",
      "/mock/stream/missing.jsonl",
      (partial) => progress.push(partial.length),
    );

    expect(result).toEqual([]);
    expect(progress).toEqual([]);
  });

  it("未传回调也能拿到完整消息", async () => {
    setSessionFixtures(
      [
        {
          providerId: "claude",
          sessionId: "stream-2",
          sourcePath: SOURCE_PATH,
        },
      ],
      {
        [`claude:${SOURCE_PATH}`]: [
          { role: "user", content: "hello" },
          { role: "assistant", content: "world" },
        ],
      },
    );

    await expect(
      sessionsApi.streamMessages("claude", SOURCE_PATH),
    ).resolves.toHaveLength(2);
  });
});

describe("sessionsApi.streamMessages transport completion", () => {
  const message = { role: "user", content: "late payload ".repeat(900) };
  const header: TranscriptChunk = {
    kind: "header",
    total: 1,
    approxBytes: 9000,
  };
  const block: TranscriptChunk = {
    kind: "messages",
    start: 0,
    messages: [message],
  };
  const done: TranscriptChunk = { kind: "done", delivered: 1 };

  it("waits after invoke resolves and buffers Done arriving before the large payload", async () => {
    const transport = controlledStream();
    const progress = vi.fn();
    const settled = vi.fn();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH, progress);
    void result.then(settled);
    transport.send(0, header);
    transport.resolve();
    await Promise.resolve();
    transport.send(2, done); // 小 Done 先到传输层，Channel 必须缓存索引 2。
    await Promise.resolve();
    expect(settled).not.toHaveBeenCalled();
    expect(progress).not.toHaveBeenCalled();
    transport.send(1, block);
    const snapshot = await result;
    expect(snapshot).toEqual([{ ...message, transcriptIndex: 0 }]);
    expect(snapshot).not.toBe(progress.mock.calls[0][0]);
    transport.send(3, { kind: "messages", start: 1, messages: [message] });
    expect(snapshot).toHaveLength(1);
    expect(progress).toHaveBeenCalledTimes(1);
  });

  it.each(["resolve", "reject"] as const)(
    "waits for invoke %s even after a complete Done",
    async (outcome) => {
      const transport = controlledStream();
      const result = sessionsApi.streamMessages("codex", SOURCE_PATH);
      const settled = vi.fn();
      void result.then(settled, settled);
      transport.send(0, header);
      transport.send(1, block);
      transport.send(2, done);
      await Promise.resolve();
      expect(settled).not.toHaveBeenCalled();
      if (outcome === "resolve") {
        transport.resolve();
        await expect(result).resolves.toEqual([
          { ...message, transcriptIndex: 0 },
        ]);
      } else {
        const error = new Error("invoke failed after Done");
        transport.reject(error);
        await expect(result).rejects.toBe(error);
      }
    },
  );

  it("preserves invoke rejection before any Channel data", async () => {
    const transport = controlledStream();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH);
    const error = new Error("command failed");
    transport.reject(error);
    await expect(result).rejects.toBe(error);
  });

  it("rejects an error chunk after invoke success and partial progress", async () => {
    const transport = controlledStream();
    const progress = vi.fn();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH, progress);
    transport.resolve();
    await Promise.resolve();
    transport.send(0, header);
    transport.send(1, block);
    transport.send(2, { kind: "error", message: "channel read failed" });
    await expect(result).rejects.toThrow("channel read failed");
    expect(progress).toHaveBeenCalledTimes(1);
  });

  const malformed: [string, TranscriptChunk[]][] = [
    ["missing header", [done]],
    ["missing messages / logical Done before payload", [header, done]],
    ["gap in start", [header, { ...block, start: 1 }]],
    ["duplicate block", [header, block, block]],
    ["negative start", [header, { ...block, start: -1 }]],
    ["total overrun", [{ ...header, total: 0 }, block]],
    ["negative total", [{ ...header, total: -1 }]],
    ["fractional total", [{ ...header, total: 1.5 }]],
    ["duplicate header", [header, header]],
    ["delivered mismatch", [header, block, { ...done, delivered: 2 }]],
    ["fractional delivered", [header, block, { ...done, delivered: 1.5 }]],
    ["duplicate Done", [header, block, done, done]],
  ];
  it.each(malformed)("rejects %s", async (_name, chunks) => {
    const transport = controlledStream();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH);
    chunks.forEach((chunk, index) => transport.send(index, chunk));
    transport.resolve();
    await expect(result).rejects.toThrow("会话消息流不完整");
  });

  it.each([
    "missing transport block",
    "missing Done",
    "missing header",
    "hung invoke",
  ])("times out rather than hanging on %s", async (missing) => {
    vi.useFakeTimers();
    const transport = controlledStream();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH);
    const rejected = expect(result).rejects.toThrow("会话读取超时");
    if (missing !== "missing header") transport.send(0, header);
    if (missing === "missing transport block") transport.send(2, done);
    if (missing === "missing Done" || missing === "hung invoke")
      transport.send(1, block);
    if (missing === "hung invoke") transport.send(2, done);
    else transport.resolve();
    await vi.advanceTimersByTimeAsync(30_000);
    await rejected;
    expect(vi.getTimerCount()).toBe(0);
  });

  it("waits for Done even for an empty session", async () => {
    const transport = controlledStream();
    const result = sessionsApi.streamMessages("codex", SOURCE_PATH);
    const settled = vi.fn();
    void result.then(settled);
    transport.send(0, { kind: "header", total: 0, approxBytes: 0 });
    transport.resolve();
    await Promise.resolve();
    expect(settled).not.toHaveBeenCalled();
    transport.send(1, { kind: "done", delivered: 0 });
    await expect(result).resolves.toEqual([]);
  });

  it("cancels pending delivery, clears the timer and ignores late data", async () => {
    vi.useFakeTimers();
    const transport = controlledStream();
    const controller = new AbortController();
    const progress = vi.fn();
    const result = sessionsApi.streamMessages(
      "codex",
      SOURCE_PATH,
      progress,
      controller.signal,
    );
    transport.send(0, header);
    transport.resolve();
    controller.abort();
    await expect(result).rejects.toMatchObject({ name: "AbortError" });
    transport.send(1, block);
    transport.send(2, done);
    expect(progress).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("clears the timer when invoke throws synchronously", async () => {
    vi.useFakeTimers();
    vi.spyOn(core, "invoke").mockImplementation(() => {
      throw new Error("synchronous invoke failure");
    });
    await expect(
      sessionsApi.streamMessages("codex", SOURCE_PATH),
    ).rejects.toThrow("synchronous invoke failure");
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not rearm the timer when progress synchronously cancels", async () => {
    vi.useFakeTimers();
    const transport = controlledStream();
    const controller = new AbortController();
    const result = sessionsApi.streamMessages(
      "codex",
      SOURCE_PATH,
      () => controller.abort(),
      controller.signal,
    );
    transport.send(0, header);
    transport.send(1, block);
    transport.resolve();
    await expect(result).rejects.toMatchObject({ name: "AbortError" });
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not invoke for an already aborted signal", async () => {
    const transport = controlledStream();
    const controller = new AbortController();
    controller.abort();
    await expect(
      sessionsApi.streamMessages(
        "codex",
        SOURCE_PATH,
        undefined,
        controller.signal,
      ),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(transport.invoke).not.toHaveBeenCalled();
  });
});

describe("sessionsApi.getMessageContent", () => {
  it("按定位取回被截断消息的全文", async () => {
    const full = "完整正文".repeat(30);
    setSessionFixtures(
      [{ providerId: "codex", sessionId: "cut-1", sourcePath: SOURCE_PATH }],
      // 后端缓存里存的是全文；预览只是下发形态，长度校验针对全文
      { [`codex:${SOURCE_PATH}`]: [{ role: "tool", content: full }] },
    );
    const identity = contentIdentityHex({ role: "tool", content: full });

    await expect(
      sessionsApi.getMessageContent(
        "codex",
        SOURCE_PATH,
        0,
        full.length,
        identity,
      ),
    ).resolves.toBe(full);
  });

  it("正文长度对不上时拒绝返回，避免张冠李戴", async () => {
    setSessionFixtures(
      [{ providerId: "codex", sessionId: "cut-2", sourcePath: SOURCE_PATH }],
      {
        [`codex:${SOURCE_PATH}`]: [{ role: "tool", content: "已经变了" }],
      },
    );

    await expect(
      sessionsApi.getMessageContent("codex", SOURCE_PATH, 0, 999),
    ).rejects.toThrow();
  });

  /**
   * 只比长度无法发现「同一下标换成了另一条长度相同的消息」：这里两条正文长度完全
   * 相同，只有身份摘要能区分。修复前这种情况会静默返回另一条消息的全文。
   */
  it("长度相同但正文已换成另一条消息时拒绝返回", async () => {
    const original = "AAAA".repeat(1100);
    const replacement = "BBBB".repeat(1100);
    expect(replacement.length).toBe(original.length);

    setSessionFixtures(
      [{ providerId: "codex", sessionId: "cut-3", sourcePath: SOURCE_PATH }],
      { [`codex:${SOURCE_PATH}`]: [{ role: "tool", content: replacement }] },
    );

    // 前端手里还拿着 original 那一版的摘要
    const staleIdentity = contentIdentityHex({
      role: "tool",
      content: original,
    });

    await expect(
      sessionsApi.getMessageContent(
        "codex",
        SOURCE_PATH,
        0,
        original.length,
        staleIdentity,
      ),
    ).rejects.toThrow(/会话已更新/);

    // 对照：不带摘要（旧调用）时长度校验会放行——这正是要根除的行为
    await expect(
      sessionsApi.getMessageContent("codex", SOURCE_PATH, 0, original.length),
    ).resolves.toBe(replacement);
  });

  it("时间戳不同也构成另一条消息的身份", async () => {
    const content = "同一段正文".repeat(700);
    setSessionFixtures(
      [{ providerId: "codex", sessionId: "cut-4", sourcePath: SOURCE_PATH }],
      {
        [`codex:${SOURCE_PATH}`]: [{ role: "tool", content, ts: 200 }],
      },
    );

    await expect(
      sessionsApi.getMessageContent(
        "codex",
        SOURCE_PATH,
        0,
        content.length,
        contentIdentityHex({ role: "tool", content, ts: 100 }),
      ),
    ).rejects.toThrow(/会话已更新/);

    await expect(
      sessionsApi.getMessageContent(
        "codex",
        SOURCE_PATH,
        0,
        content.length,
        contentIdentityHex({ role: "tool", content, ts: 200 }),
      ),
    ).resolves.toBe(content);
  });
});

describe("sessionsApi.searchMessages", () => {
  it("searches full un-truncated messages on the backend and returns transcript indices", async () => {
    setSessionFixtures(
      [
        {
          providerId: "codex",
          sessionId: "search-full-1",
          sourcePath: SOURCE_PATH,
        },
      ],
      {
        [`codex:${SOURCE_PATH}`]: [
          { role: "user", content: "short question" },
          {
            role: "assistant",
            content: `${"prefix ".repeat(800)}deep_tail_token alpha`,
          },
          { role: "assistant", content: "alpha only" },
        ],
      },
    );

    await expect(
      sessionsApi.searchMessages("codex", SOURCE_PATH, "deep_tail_token alpha"),
    ).resolves.toEqual([1]);
  });

  it("returns null gracefully when search_session_messages is unavailable", async () => {
    vi.spyOn(core, "invoke").mockRejectedValueOnce(
      new Error("unknown command"),
    );

    await expect(
      sessionsApi.searchMessages("codex", SOURCE_PATH, "needle"),
    ).resolves.toBeNull();
  });
});
