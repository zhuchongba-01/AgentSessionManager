import { describe, expect, it } from "vitest";
import {
  buildSessionTitleIndex,
  buildUserMessageToc,
  collectSessionDescendants,
  extractCodexPromptPreview,
  formatSessionMessagePreview,
  getProviderIconName,
  getProviderLabel,
  getVisibleSessionMessages,
  groupSessionsByProviderAndDirectory,
  resolveParentLabel,
  shouldHideCodexMessageFromToc,
  shouldShowSessionMinimap,
} from "@/components/sessions/utils";
import type { SessionMessage, SessionMeta } from "@/types";

describe("session utils", () => {
  /**
   * 目录/缩略导航用的下标必须是「可见消息」空间的下标，而不是正文里的绝对下标。
   *
   * 虚拟列表的 count 就是 visibleMessages 的长度，scrollToIndex 与 SessionMinimap
   * 都吃这个空间；一旦这里改用 wire 的 `SessionMessage.index`（那是取全文用的绝对
   * 下标），点击缩略导航就会跳到错误的条目上——被过滤掉的注入消息越多，偏得越远。
   */
  it("derives TOC indexes in the visible-message space, not the transcript space", () => {
    const messages: SessionMessage[] = [
      // 被过滤的注入消息位于用户消息之前：绝对下标与可见下标必然错位
      { role: "developer", content: "developer instructions", index: 0 },
      { role: "user", content: "第一个问题", index: 1 },
      { role: "assistant", content: "回答", index: 2 },
      { role: "system", content: "system note", index: 3 },
      { role: "user", content: "第二个问题", index: 4 },
    ];

    const visible = getVisibleSessionMessages(messages, true);
    expect(visible.map((message) => message.content)).toEqual([
      "第一个问题",
      "回答",
      "第二个问题",
    ]);

    const toc = buildUserMessageToc(visible, true);
    expect(toc.map((entry) => entry.index)).toEqual([0, 2]);
    expect(toc.map((entry) => entry.preview)).toEqual([
      "第一个问题",
      "第二个问题",
    ]);

    // 每个下标都必须能在「可见消息」里定位到同一条用户消息
    for (const entry of toc) {
      expect(visible[entry.index].role).toBe("user");
      expect(visible[entry.index].content).toBe(entry.preview);
    }
    // 与正文绝对下标对照：它们确实不同（1/4 vs 0/2），这正是不能混用的原因
    expect(toc.map((entry) => visible[entry.index].index)).toEqual([1, 4]);
  });

  it("keeps transcript order and timestamps in the TOC", () => {
    const visible: SessionMessage[] = [
      { role: "user", content: "a".repeat(80), ts: 10 },
      { role: "assistant", content: "reply", ts: 20 },
      { role: "user", content: "short", ts: 30 },
    ];

    const toc = buildUserMessageToc(visible, false);
    expect(toc).toHaveLength(2);
    expect(toc[0].ts).toBe(10);
    expect(toc[1].ts).toBe(30);
    // 预览按既有规则截断
    expect(toc[0].preview.endsWith("...")).toBe(true);
  });

  it("groups native sidebar sections without changing underlying project paths", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "one",
        projectDir: "E:/Codex11",
        projectName: "Codex11",
        sidebarSection: { id: "section-a", name: "Deepseek11" },
      },
      {
        providerId: "codex",
        sessionId: "two",
        projectName: "最近",
        sidebarSection: { id: "section-a", name: "Deepseek11" },
      },
      {
        providerId: "codex",
        sessionId: "three",
        projectDir: "E:/Deepseek11",
        projectName: "Deepseek11",
      },
    ];
    const groups = groupSessionsByProviderAndDirectory(sessions, "未知目录");
    expect(groups[0].directories).toHaveLength(2);
    expect(groups[0].directories[0]).toMatchObject({
      label: "Deepseek11",
      projectDir: null,
    });
    expect(groups[0].directories[0].sessions.map((s) => s.sessionId)).toEqual([
      "one",
      "two",
    ]);
    expect(sessions[0].projectDir).toBe("E:/Codex11");
    expect(groups[0].directories[0].key).not.toBe(groups[0].directories[1].key);
  });
  it("uses canonical Agent brand names and normalized icon keys", () => {
    const untranslated = (key: string) => key;
    expect(getProviderLabel("codex", untranslated)).toBe("Codex");
    expect(getProviderLabel("claude", untranslated)).toBe("Claude Code");
    expect(getProviderLabel("opencode", untranslated)).toBe("OpenCode");
    expect(getProviderLabel("zcode", untranslated)).toBe("ZCode");
    expect(getProviderLabel("grokbuild", untranslated)).toBe("Grok Build");
    expect(getProviderLabel("pi", untranslated)).toBe("Pi");
    expect(getProviderLabel("deepseek", untranslated)).toBe("DeepSeek Harness");
    expect(getProviderIconName("Codex")).toBe("openai");
    expect(getProviderIconName("GROKBUILD")).toBe("grok");
    expect(getProviderIconName("DeepSeek")).toBe("deepseek");
  });

  it("hides Codex internal context from the readable transcript", () => {
    const messages: SessionMessage[] = [
      { role: "developer", content: "INTERNAL_DEVELOPER_PROMPT" },
      { role: "system", content: "INTERNAL_SYSTEM_PROMPT" },
      {
        role: "user",
        content:
          "<environment_context>INTERNAL_ENVIRONMENT_CONTEXT</environment_context>",
      },
      { role: "user", content: "用户真正输入的问题" },
      { role: "assistant", content: "助手真正显示的回答" },
    ];

    expect(getVisibleSessionMessages(messages, true)).toEqual([
      { role: "user", content: "用户真正输入的问题" },
      { role: "assistant", content: "助手真正显示的回答" },
    ]);
    expect(getVisibleSessionMessages(messages, false)).toBe(messages);
  });

  it("extracts Codex VS Code prompts after the request marker", () => {
    const content = [
      "# Context from my IDE setup:",
      "",
      "## Active file: src/main.ts",
      "",
      "## My request for Codex:",
      "Fix the session title preview",
    ].join("\n");

    expect(extractCodexPromptPreview(content)).toBe(
      "Fix the session title preview",
    );
  });

  it("extracts inline Codex VS Code prompts", () => {
    const content = [
      "# Context from my IDE setup:",
      "",
      "## My request for Codex: Fix the TOC preview",
    ].join("\n");

    expect(extractCodexPromptPreview(content)).toBe("Fix the TOC preview");
  });

  it("ignores marker mentions before the Codex request heading", () => {
    const content = [
      "# Context from my IDE setup:",
      "",
      "## Active selection:",
      "My request for Codex: not the prompt",
      "",
      "## My request for Codex:",
      "Use the real request heading",
    ].join("\n");

    expect(extractCodexPromptPreview(content)).toBe(
      "Use the real request heading",
    );
  });

  it("uses the last request heading when the selection contains one", () => {
    const content = [
      "# Context from my IDE setup:",
      "",
      "## Active selection: docs/codex-format.md:10-14",
      "## My request for Codex:",
      "selected document content, not the real request",
      "",
      "## My request for Codex:",
      "the real injected request",
    ].join("\n");

    expect(extractCodexPromptPreview(content)).toBe(
      "the real injected request",
    );
  });

  // Known limitation: the IDE marker is matched purely by text, so a
  // "## My request for Codex:" line inside the real request body is treated as
  // a new boundary and only the trailing part is kept. Pinning this documents
  // the best-effort behavior; fully fixing it needs structured IDE section data
  // that the Codex VS Code context does not provide.
  it("keeps only the trailing part when the request body repeats the heading", () => {
    const content = [
      "# Context from my IDE setup:",
      "",
      "## Active file: foo.ts",
      "",
      "## My request for Codex:",
      "Document the format, for example:",
      "## My request for Codex:",
      "and the rest follows.",
    ].join("\n");

    expect(extractCodexPromptPreview(content)).toBe("and the rest follows.");
  });

  it("does not extract from ordinary messages that mention the marker", () => {
    const content = "Please explain the phrase My request for Codex.";

    expect(extractCodexPromptPreview(content)).toBe(content);
  });

  it("hides Codex context messages without user prompts from the TOC", () => {
    expect(
      shouldHideCodexMessageFromToc("# AGENTS.md instructions for F:/project"),
    ).toBe(true);
    expect(
      shouldHideCodexMessageFromToc(
        "<environment_context>\n<cwd>F:/project</cwd>",
      ),
    ).toBe(true);
    expect(shouldHideCodexMessageFromToc("# Context from my IDE setup:")).toBe(
      true,
    );
    expect(
      shouldHideCodexMessageFromToc(
        "# Context from my IDE setup:\n\n## My request for Codex:\nFix it",
      ),
    ).toBe(false);
  });

  it("formats message previews with truncation", () => {
    expect(formatSessionMessagePreview("short message")).toBe("short message");
    expect(formatSessionMessagePreview("a".repeat(51))).toBe(
      `${"a".repeat(50)}...`,
    );
  });

  /**
   * 缩略导航「压根不出来」的回归。
   *
   * 判据曾经有两个：先是「正文是否超过两屏」的像素高度（同一个会话随窗口大小
   * 时有时无），后来换成正文字符数门槛（很短的单轮会话永远不出现）。用户看到的
   * 都是同一句「不显示」，所以现在只留一个判据：有没有用户提问。
   */
  it("shows the session minimap whenever the session has a user question", () => {
    // 单轮短会话也要出现：用户不知道背后有没有长度门槛，只会认为功能坏了
    expect(shouldShowSessionMinimap(1)).toBe(true);
    expect(shouldShowSessionMinimap(2)).toBe(true);
    expect(shouldShowSessionMinimap(50)).toBe(true);
    // 没有提问就没有刻度
    expect(shouldShowSessionMinimap(0)).toBe(false);
  });

  it("groups sessions by provider and project directory", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "codex-1",
        projectDir: "/workspace/app",
      },
      {
        providerId: "codex",
        sessionId: "codex-2",
        projectDir: "/workspace/app",
      },
      {
        providerId: "claude",
        sessionId: "claude-1",
        projectDir: "/workspace/docs",
      },
    ];

    const groups = groupSessionsByProviderAndDirectory(sessions, "未知目录");

    expect(groups).toHaveLength(2);
    expect(groups[0].providerId).toBe("codex");
    expect(groups[0].sessions.map((session) => session.sessionId)).toEqual([
      "codex-1",
      "codex-2",
    ]);
    expect(groups[0].directories).toHaveLength(1);
    expect(groups[0].directories[0]).toMatchObject({
      projectDir: "/workspace/app",
      label: "app",
    });
    expect(
      groups[0].directories[0].sessions.map((session) => session.sessionId),
    ).toEqual(["codex-1", "codex-2"]);
    expect(groups[1].providerId).toBe("claude");
    expect(groups[1].directories[0].label).toBe("docs");
  });

  it("uses the native project name and merges equivalent Windows paths", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "codex-1",
        projectDir: "C:/Users/zcb/Desktop/Cat/",
        projectName: "Cat",
      },
      {
        providerId: "codex",
        sessionId: "codex-2",
        projectDir: "c:\\users\\zcb\\desktop\\cat",
        projectName: "Cat",
      },
    ];

    const groups = groupSessionsByProviderAndDirectory(sessions, "未知目录");

    expect(groups[0].directories).toHaveLength(1);
    expect(groups[0].directories[0].label).toBe("Cat");
    expect(
      groups[0].directories[0].sessions.map((session) => session.sessionId),
    ).toEqual(["codex-1", "codex-2"]);
  });

  it("uses an unknown directory group for sessions without project directories", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "codex-1",
        projectDir: null,
      },
      {
        providerId: "codex",
        sessionId: "codex-2",
        projectDir: "   ",
      },
    ];

    const groups = groupSessionsByProviderAndDirectory(sessions, "未知目录");

    expect(groups).toHaveLength(1);
    expect(groups[0].directories).toHaveLength(1);
    expect(groups[0].directories[0]).toMatchObject({
      projectDir: null,
      label: "未知目录",
    });
    expect(
      groups[0].directories[0].sessions.map((session) => session.sessionId),
    ).toEqual(["codex-1", "codex-2"]);
  });

  it("uses the native ungrouped label for unassigned DeepSeek sessions", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "deepseek",
        sessionId: "deepseek-1",
        projectDir: null,
        projectName: null,
      },
    ];

    const groups = groupSessionsByProviderAndDirectory(
      sessions,
      "未知目录",
      "未分组",
    );

    expect(groups[0].directories[0]).toMatchObject({
      projectDir: null,
      projectName: null,
      label: "未分组",
    });
  });

  it("preserves filtered session order inside provider and directory groups", () => {
    const sessions: SessionMeta[] = [
      {
        providerId: "codex",
        sessionId: "newest",
        projectDir: "/workspace/app",
        lastActiveAt: 30,
      },
      {
        providerId: "codex",
        sessionId: "middle",
        projectDir: "/workspace/docs",
        lastActiveAt: 20,
      },
      {
        providerId: "codex",
        sessionId: "oldest",
        projectDir: "/workspace/app",
        lastActiveAt: 10,
      },
    ];

    const groups = groupSessionsByProviderAndDirectory(sessions, "未知目录");

    expect(groups[0].sessions.map((session) => session.sessionId)).toEqual([
      "newest",
      "middle",
      "oldest",
    ]);
    expect(groups[0].directories.map((group) => group.label)).toEqual([
      "app",
      "docs",
    ]);
    expect(
      groups[0].directories[0].sessions.map((session) => session.sessionId),
    ).toEqual(["newest", "oldest"]);
  });

  describe("subagent lineage", () => {
    const lineage: SessionMeta[] = [
      {
        providerId: "deepseek",
        sessionId: "parent",
        title: "Parent Chat",
        sourcePath: "/ds/parent/session.v3.jsonl",
        lastActiveAt: 50,
      },
      {
        providerId: "deepseek",
        sessionId: "child-a",
        title: "Sub Task A",
        residual: true,
        parentSessionId: "parent",
        sourcePath: "/ds/child-a/session.v3.jsonl",
        lastActiveAt: 40,
      },
      {
        providerId: "deepseek",
        sessionId: "child-b",
        title: "Sub Task B",
        residual: true,
        parentSessionId: "parent",
        sourcePath: "/ds/child-b/session.v3.jsonl",
        lastActiveAt: 30,
      },
      {
        providerId: "deepseek",
        sessionId: "grandchild",
        title: "Nested Helper",
        residual: true,
        parentSessionId: "child-a",
        sourcePath: "/ds/grandchild/session.v3.jsonl",
        lastActiveAt: 20,
      },
      {
        providerId: "opencode",
        sessionId: "parent",
        title: "同 ID 的另一家",
        sourcePath: "/oc/parent",
        lastActiveAt: 10,
      },
    ];

    it("collects every descendant transitively and stops at the selected roots", () => {
      const descendants = collectSessionDescendants(lineage, [lineage[0]]);

      expect(descendants.map((session) => session.sessionId)).toEqual([
        "child-a",
        "child-b",
        "grandchild",
      ]);
    });

    it("still walks through an explicitly selected child to reach its own children", () => {
      // 批量删除里同时勾选了父会话与其中一个子会话：那个子会话不出现在「后代」里
      // （它已经是删除目标），但它自己的孩子仍然必须被带上，否则父会话删不掉。
      const descendants = collectSessionDescendants(lineage, [
        lineage[0],
        lineage[1],
      ]);

      expect(descendants.map((session) => session.sessionId)).toEqual([
        "child-b",
        "grandchild",
      ]);
    });

    it("does not treat a same-id session from another provider as lineage", () => {
      const descendants = collectSessionDescendants(lineage, [lineage[4]]);

      expect(descendants).toEqual([]);
    });

    it("terminates on a cyclic parent chain instead of expanding forever", () => {
      const cyclic: SessionMeta[] = [
        { providerId: "deepseek", sessionId: "a", parentSessionId: "b" },
        { providerId: "deepseek", sessionId: "b", parentSessionId: "a" },
      ];

      expect(
        collectSessionDescendants(cyclic, [cyclic[0]]).map(
          (session) => session.sessionId,
        ),
      ).toEqual(["b"]);
    });

    it("names the parent from the scan and falls back to the raw id", () => {
      const titles = buildSessionTitleIndex(lineage);

      expect(resolveParentLabel(lineage[1], titles)).toBe("Parent Chat");
      // 父会话不在扫描结果里（被隐藏或已删除）时不能退回「残留」这种看不出归属
      // 的标签：宁可显示原生 ID 前 8 位。
      expect(resolveParentLabel(lineage[3], new Map())).toBe("child-a");
      expect(resolveParentLabel(lineage[0], titles)).toBeUndefined();
    });
  });
});
