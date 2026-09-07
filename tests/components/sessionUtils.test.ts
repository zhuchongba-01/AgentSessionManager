import { describe, expect, it } from "vitest";
import {
  extractCodexPromptPreview,
  formatSessionMessagePreview,
  getProviderIconName,
  getProviderLabel,
  getVisibleSessionMessages,
  groupSessionsByProviderAndDirectory,
  shouldHideCodexMessageFromToc,
} from "@/components/sessions/utils";
import type { SessionMessage, SessionMeta } from "@/types";

describe("session utils", () => {
  it("uses canonical Agent brand names and normalized icon keys", () => {
    const untranslated = (key: string) => key;
    expect(getProviderLabel("codex", untranslated)).toBe("Codex");
    expect(getProviderLabel("claude", untranslated)).toBe("Claude Code");
    expect(getProviderLabel("opencode", untranslated)).toBe("OpenCode");
    expect(getProviderLabel("zcode", untranslated)).toBe("ZCode");
    expect(getProviderLabel("grokbuild", untranslated)).toBe("Grok Build");
    expect(getProviderLabel("pi", untranslated)).toBe("Pi");
    expect(getProviderIconName("Codex")).toBe("openai");
    expect(getProviderIconName("GROKBUILD")).toBe("grok");
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
});
