import { describe, expect, it } from "vitest";

import { sessionsApi } from "@/lib/api/sessions";
import { setSessionFixtures } from "../msw/state";

const SOURCE_PATH = "/mock/stream/session.jsonl";

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
    expect(result[319].content).toBe("message-319");
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

describe("sessionsApi.getMessageContent", () => {
  it("按定位取回被截断消息的全文", async () => {
    const full = "完整正文".repeat(30);
    setSessionFixtures(
      [{ providerId: "codex", sessionId: "cut-1", sourcePath: SOURCE_PATH }],
      // 后端缓存里存的是全文；预览只是下发形态，长度校验针对全文
      { [`codex:${SOURCE_PATH}`]: [{ role: "tool", content: full }] },
    );

    await expect(
      sessionsApi.getMessageContent("codex", SOURCE_PATH, 0, full.length),
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
});
