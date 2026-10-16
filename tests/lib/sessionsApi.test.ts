import { describe, expect, it } from "vitest";

import { sessionsApi } from "@/lib/api/sessions";
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
