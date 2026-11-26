import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { SessionDetailPane } from "@/components/sessions/SessionDetailPane";
import { ReaderContext } from "@/components/sessions/reader/context";
import { getAgentReaderStyle } from "@/components/sessions/reader/agentStyles";
import {
  parseDiff,
  SessionDiff,
} from "@/components/sessions/reader/SessionDiff";
import {
  copyImage,
  dataUrlToBlob,
} from "@/components/sessions/reader/SessionImageLightbox";
import {
  SessionMarkdown,
  SessionPlainText,
} from "@/components/sessions/reader/SessionMarkdown";
import { getVisibleSessionMessages } from "@/components/sessions/utils";
import { TooltipProvider } from "@/components/ui/tooltip";
import { sessionsApi } from "@/lib/api/sessions";
import { clearSessionImageCache } from "@/lib/query/sessions";
import type {
  ContentRef,
  ImageRef,
  SessionMessage,
  SessionMeta,
} from "@/types";
import {
  getExportedMarkdownRecords,
  getOpenedExternalUrls,
  getRevealedPaths,
  resetProviderState,
  setBlockContentFixture,
  setSessionImageFixture,
} from "../msw/state";

vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({
    count,
    getItemKey,
  }: {
    count: number;
    getItemKey?: (index: number) => string | number;
  }) => ({
    getVirtualItems: () =>
      Array.from({ length: count }, (_, index) => ({
        index,
        key: getItemKey ? getItemKey(index) : index,
        start: index * 80,
        end: (index + 1) * 80,
      })),
    getTotalSize: () => count * 80,
    measureElement: () => undefined,
    scrollToIndex: () => undefined,
    scrollOffset: 0,
    scrollRect: { width: 800, height: 600 },
    range: { startIndex: 0, endIndex: Math.max(0, count - 1) },
  }),
}));

function renderDetailPane({
  session,
  messages,
}: {
  session: SessionMeta;
  messages: SessionMessage[];
}) {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
    },
  });
  const onCopy = vi.fn();
  const onOpenUrl = vi.fn((url: string) => {
    void sessionsApi.openExternalUrl(url);
  });
  const onRevealPath = vi.fn((path: string) => {
    void sessionsApi.revealPath(path);
  });
  const onNotifyExport = vi.fn();
  const isCodexSession = session.providerId === "codex";
  const visibleMessages = getVisibleSessionMessages(messages, isCodexSession);

  const view = render(
    <QueryClientProvider client={client}>
      <TooltipProvider>
        <SessionDetailPane
          selectedSession={session}
          selectedKey={`${session.providerId}:${session.sessionId}:${session.sourcePath ?? ""}`}
          isCodexSession={isCodexSession}
          selectedSessionChildCount={0}
          sessionTitles={new Map()}
          rawMessages={messages}
          visibleMessages={visibleMessages}
          isLoadingMessages={false}
          isFetchingMessages={false}
          messageRevision={1}
          messagesError={null}
          onRefetchMessages={vi.fn()}
          onLoadFullContent={async (msg) => msg.content}
          hasBatchSelection={false}
          selectedDeletableCount={0}
          isDeleting={false}
          onBatchDelete={vi.fn()}
          onSingleDelete={vi.fn()}
          onCopy={onCopy}
          onCopyMessage={vi.fn()}
          onCopyCode={vi.fn()}
          onOpenUrl={onOpenUrl}
          onRevealPath={onRevealPath}
          onNotifyExport={onNotifyExport}
        />
      </TooltipProvider>
    </QueryClientProvider>,
  );

  return {
    client,
    onCopy,
    onOpenUrl,
    onRevealPath,
    onNotifyExport,
    ...view,
  };
}

describe("SessionDetailPane & reader components suite", () => {
  beforeEach(() => {
    resetProviderState();
    clearSessionImageCache();
    window.localStorage.removeItem("cc-switch.sessionReader.expandTimelines");
    window.localStorage.removeItem("agent-session-manager.outlineOpen");
    Element.prototype.scrollIntoView = vi.fn();
  });

  it("maps all 13 providers to appropriate reader visual styles", () => {
    expect(getAgentReaderStyle("claude").id).toBe("claude");
    expect(getAgentReaderStyle("workbuddy").id).toBe("claude");
    expect(getAgentReaderStyle("codebuddy").id).toBe("claude");
    expect(getAgentReaderStyle("codex").id).toBe("codex");
    expect(getAgentReaderStyle("gemini").id).toBe("gemini");
    expect(getAgentReaderStyle("opencode").id).toBe("opencode");
    expect(getAgentReaderStyle("zcode").id).toBe("opencode");
    expect(getAgentReaderStyle("pi").id).toBe("pi");
    expect(getAgentReaderStyle("openclaw").id).toBe("pi");
    expect(getAgentReaderStyle("deepseek").id).toBe("generic");
    expect(getAgentReaderStyle("kimi").id).toBe("generic");
    expect(getAgentReaderStyle("grokbuild").id).toBe("generic");
    expect(getAgentReaderStyle("copilot").id).toBe("generic");
  });

  it("parses unified diffs and Codex apply_patch payloads with file PathChip headers and line numbers", () => {
    const patch = [
      "*** Begin Patch",
      "*** Update File: /repo/app/src/login.ts",
      "@@ -12,2 +12,3 @@",
      " const ok = true;",
      "-return false;",
      "+const verified = ok;",
      "+return verified;",
      "*** End Patch",
    ].join("\n");

    const parsed = parseDiff(patch);
    expect(parsed[0]).toEqual({
      type: "file",
      path: "/repo/app/src/login.ts",
    });

    const client = new QueryClient();
    const { container } = render(
      <QueryClientProvider client={client}>
        <TooltipProvider>
          <ReaderContext.Provider
            value={{
              providerId: "codex",
              sourcePath: "/mock/codex/session.jsonl",
              projectDir: "/repo/app",
              style: getAgentReaderStyle("codex"),
              appName: "Codex",
              onCopy: vi.fn(),
            }}
          >
            <SessionDiff text={patch} projectDir="/repo/app" showViewToggle />
          </ReaderContext.Provider>
        </TooltipProvider>
      </QueryClientProvider>,
    );

    expect(screen.getByText("src/login.ts")).toBeInTheDocument();
    expect(container.querySelectorAll(".bg-diff-add")).toHaveLength(2);
    expect(container.querySelectorAll(".bg-diff-del")).toHaveLength(1);
  });

  it("renders user plain text preserving Markdown symbols, renders assistant GFM + PathChips, and opens external links & remote images via backend", async () => {
    const client = new QueryClient();
    render(
      <QueryClientProvider client={client}>
        <TooltipProvider>
          <ReaderContext.Provider
            value={{
              providerId: "claude",
              sourcePath: "/mock/claude/s.jsonl",
              projectDir: "/repo/app",
              style: getAgentReaderStyle("claude"),
              appName: "Claude Code",
              onCopy: vi.fn(),
            }}
          >
            <div data-testid="plain-box">
              <SessionPlainText
                content={
                  "# Literal heading *not italic*\n```ts\nconst x = 1;\n```"
                }
              />
            </div>
            <div data-testid="md-box">
              <SessionMarkdown
                content={[
                  "请查看 `src/auth.ts:42` 与 [官方文档](https://docs.example.com/guide) 以及 `~/notes.md`：",
                  "",
                  "![架构图](https://cdn.example.com/arch.png)",
                  "",
                  "| 字段 | 状态 |",
                  "| --- | --- |",
                  "| exp | 正常 |",
                ].join("\n")}
              />
            </div>
          </ReaderContext.Provider>
        </TooltipProvider>
      </QueryClientProvider>,
    );

    expect(
      screen.getByText("# Literal heading *not italic*"),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: /Literal heading/ }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("ts")).toBeInTheDocument();

    expect(screen.getByText("src/auth.ts:42")).toBeInTheDocument();
    expect(screen.getByRole("table")).toBeInTheDocument();
    expect(screen.getByText("exp")).toBeInTheDocument();

    // Reveal relative path and ~/ path via PathChip folder buttons
    const revealBtns = screen.getAllByRole("button", {
      name: "在文件管理器中定位",
    });
    expect(revealBtns).toHaveLength(2);
    fireEvent.click(revealBtns[0]);
    fireEvent.click(revealBtns[1]);
    await waitFor(() =>
      expect(getRevealedPaths()).toEqual([
        "/repo/app/src/auth.ts",
        "~/notes.md",
      ]),
    );

    // Click external Markdown link and remote image external-open card
    fireEvent.click(screen.getByRole("link", { name: /官方文档/ }));
    fireEvent.click(
      screen.getByRole("button", { name: /在浏览器中打开远程图片: 架构图/ }),
    );
    await waitFor(() =>
      expect(getOpenedExternalUrls()).toEqual([
        "https://docs.example.com/guide",
        "https://cdn.example.com/arch.png",
      ]),
    );
  });

  it("renders chain-of-thought preview, lazy loads full thinking, toggles thinking visibility live, and syncs with copy/export", async () => {
    const fullThinkingRef: ContentRef = {
      kind: "jsonl",
      offset: 128,
      len: 1024,
      pointer: "/message/content/0/thinking",
    };
    setBlockContentFixture(
      "codex",
      "sqlite:/repo/app/opencode.db:codex-session-1",
      fullThinkingRef,
      "完整深度推理正文：服务端 JWT 公钥轮换后未同步到边缘网关。",
    );

    const session: SessionMeta = {
      providerId: "codex",
      sessionId: "codex-session-1",
      title: "JWT 校验排查",
      projectDir: "/repo/app",
      sourcePath: "sqlite:/repo/app/opencode.db:codex-session-1",
      resumeCommand: "codex resume codex-session-1",
    };

    const messages: SessionMessage[] = [
      {
        role: "user",
        content: "",
        ts: 1000,
        blocks: [{ type: "text", text: "为什么 JWT 校验失败？" }],
      },
      {
        role: "assistant",
        content: "",
        ts: 1005,
        meta: {
          model: "gpt-5-codex",
          inputTokens: 1200,
          outputTokens: 340,
          durationMs: 4200,
        },
        blocks: [
          {
            type: "thinking",
            text: "预览摘要片段",
            summary: "**核对时钟偏移与签名密钥**",
            durationMs: 4200,
            full: fullThinkingRef,
          },
          { type: "text", text: "原因是边缘网关未刷新公钥。" },
        ],
      },
      {
        role: "user",
        content: "",
        ts: 2000,
        blocks: [{ type: "text", text: "再检查加密推理块" }],
      },
      {
        role: "assistant",
        content: "",
        ts: 2005,
        blocks: [
          {
            type: "thinking",
            text: "",
            redacted: true,
            durationMs: 1800,
          },
          { type: "text", text: "加密思考块已安全处理。" },
        ],
      },
    ];

    const { onCopy } = renderDetailPane({ session, messages });

    const thinkingBtn = screen.getByRole("button", {
      name: /推理.*核对时钟偏移与签名密钥/,
    });
    expect(thinkingBtn).toHaveAttribute("aria-expanded", "false");
    expect(screen.getByText("思考内容不可见")).toBeInTheDocument();

    fireEvent.click(thinkingBtn);
    expect(thinkingBtn).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/预览摘要片段/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "显示全部" }));
    expect(
      await screen.findByText(
        "完整深度推理正文：服务端 JWT 公钥轮换后未同步到边缘网关。",
      ),
    ).toBeInTheDocument();

    // Switching to "对话" hides thinking/steps and syncs clean Q&A with Copy & Export .md
    fireEvent.click(screen.getByRole("button", { name: "对话" }));
    expect(
      screen.queryByRole("button", { name: /推理.*核对时钟偏移与签名密钥/ }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("思考内容不可见")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "复制 Markdown" }));
    expect(onCopy).toHaveBeenCalledWith(
      expect.stringContaining("加密思考块已安全处理。"),
      "已复制会话 Markdown",
    );

    fireEvent.click(screen.getByRole("button", { name: "导出 .md" }));
    await waitFor(() => expect(getExportedMarkdownRecords()).toHaveLength(1));
    expect(getExportedMarkdownRecords()[0]?.content).not.toContain(
      "核对时钟偏移与签名密钥",
    );

    // SQLite virtual sourcePath chip copies real db path instead of sqlite: prefix
    fireEvent.click(screen.getByRole("button", { name: /opencode\.db/ }));
    expect(onCopy).toHaveBeenCalledWith(
      "/repo/app/opencode.db",
      "原始文件已复制",
    );
  });

  it("merges consecutive read/search steps, collapses even no-final turns via 收起过程, hides 改动 when only shell fails, and supports outline + search", async () => {
    const inlineImg: ImageRef = {
      source: {
        kind: "local_file",
        path: "/repo/app/screenshot.png",
      },
      mediaType: "image/png",
      size: 1024,
      alt: "UI 截图",
    };
    setSessionImageFixture(
      "codex",
      "/mock/codex/session-2.jsonl",
      inlineImg,
      "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    );

    const diffFullRef: ContentRef = {
      kind: "jsonl",
      offset: 512,
      len: 200,
      pointer: "/payload/input",
    };
    setBlockContentFixture(
      "codex",
      "/mock/codex/session-2.jsonl",
      diffFullRef,
      "*** Begin Patch\n*** Update File: /repo/app/src/auth.ts\n@@ -1,1 +1,1 @@\n-export const verify = () => false;\n+export const verify = () => true;\n*** End Patch",
    );

    const session: SessionMeta = {
      providerId: "codex",
      sessionId: "codex-session-2",
      title: "单测与补丁修复",
      projectDir: "/repo/app",
      sourcePath: "/mock/codex/session-2.jsonl",
      resumeCommand: "codex resume codex-session-2",
    };

    const messages: SessionMessage[] = [
      {
        role: "user",
        content: "# AGENTS.md injected instructions",
        injected: true,
        blocks: [{ type: "text", text: "# AGENTS.md injected instructions" }],
      },
      {
        role: "system",
        content: "已归纳前序上下文摘要。",
        blocks: [
          {
            type: "event",
            kind: "compaction",
            text: "已归纳前序上下文摘要。",
          },
          {
            type: "event",
            kind: "pr_link",
            text: "PR #128",
            url: "https://github.com/example/repo/pull/128",
          },
        ],
      },
      {
        role: "user",
        content: "",
        ts: 1000,
        blocks: [
          { type: "text", text: "排查并修复 auth.ts" },
          { type: "image", image: inlineImg },
        ],
      },
      {
        role: "assistant",
        content: "",
        ts: 1010,
        blocks: [
          { type: "text", text: "先读取文件并搜索验证逻辑" },
          {
            type: "tool_call",
            id: "c_read",
            rawName: "read_file",
            kind: "read",
            title: "/repo/app/src/auth.ts",
            inputPreview: '{"path":"/repo/app/src/auth.ts"}',
            inputTotalLen: 32,
          },
          {
            type: "tool_result",
            callId: "c_read",
            status: "success",
            preview: "export const verify = () => false;",
            totalLen: 34,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "c_search",
            rawName: "grep",
            kind: "search",
            title: "verify",
            inputPreview: '{"pattern":"verify"}',
            inputTotalLen: 20,
          },
          {
            type: "tool_result",
            callId: "c_search",
            status: "success",
            preview: "src/auth.ts:1:verify",
            totalLen: 20,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "c_patch",
            rawName: "apply_patch",
            kind: "edit",
            title: "/repo/app/src/auth.ts",
            inputPreview: '{"path":"/repo/app/src/auth.ts"}',
            inputTotalLen: 32,
            diff: {
              files: [
                {
                  path: "/repo/app/src/auth.ts",
                  op: "update",
                  added: 1,
                  removed: 1,
                },
              ],
              added: 1,
              removed: 1,
              full: diffFullRef,
            },
          },
          {
            type: "tool_result",
            callId: "c_patch",
            status: "success",
            preview: "Success. Updated 1 file.",
            totalLen: 24,
            lineCount: 1,
            truncated: false,
          },
          {
            type: "tool_call",
            id: "c_sh",
            rawName: "exec_command",
            kind: "shell",
            title: "pnpm test",
            inputPreview: '{"cmd":"pnpm test"}',
            inputTotalLen: 19,
          },
          {
            type: "tool_result",
            callId: "c_sh",
            status: "error",
            preview: "FAIL src/auth.test.ts\nExpected 200, received 401",
            totalLen: 48,
            lineCount: 2,
            truncated: false,
            exitCode: 1,
          },
          { type: "text", text: "已定位并应用补丁修复。" },
        ],
      },
    ];

    const { onCopy } = renderDetailPane({ session, messages });

    // Injected message is hidden by default until toggled
    expect(
      screen.queryByText(/# AGENTS\.md injected instructions/),
    ).not.toBeInTheDocument();

    // PR link event button opens via backend open_external_url
    fireEvent.click(
      screen.getByRole("button", {
        name: /https:\/\/github\.com\/example\/repo\/pull\/128/,
      }),
    );
    await waitFor(() =>
      expect(getOpenedExternalUrls()).toContain(
        "https://github.com/example/repo/pull/128",
      ),
    );

    // Compaction event row expands on click
    const compactBtn = screen.getByRole("button", { name: /上下文已压缩/ });
    fireEvent.click(compactBtn);
    expect(screen.getByText("已归纳前序上下文摘要。")).toBeInTheDocument();

    // Collapsed timeline pins the failed command step directly underneath
    const timelineBtn = screen.getByRole("button", { name: /执行过程/ });
    expect(timelineBtn).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.getByRole("button", { name: /Ran.*pnpm test/ }),
    ).toBeInTheDocument();

    // Toolbar "展开过程" expands both the timeline AND merged Explored children
    fireEvent.click(screen.getByRole("button", { name: "展开过程" }));
    expect(
      screen.getByRole("button", { name: /Read.*src\/auth\.ts/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "收起过程" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    // Filter switching: "改动" shows only file changes and syncs Copy Markdown to changes only
    fireEvent.click(screen.getByRole("button", { name: "改动" }));
    expect(
      screen.getByRole("button", { name: /Edited.*src\/auth\.ts/ }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Ran.*pnpm test/ }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "复制 Markdown" }));
    expect(onCopy).toHaveBeenLastCalledWith(
      expect.stringContaining("src/auth.ts"),
      "已复制会话 Markdown",
    );
    expect(onCopy.mock.lastCall?.[0]).not.toContain("pnpm test");

    // Toggle outline and inline search (and verify search respects "对话" filter)
    fireEvent.click(screen.getByRole("button", { name: "全部" }));
    fireEvent.click(screen.getByRole("button", { name: "切换对话目录" }));
    expect(
      screen.getByRole("navigation", { name: "对话目录" }),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "在当前会话中搜索" }));
    const findInput = screen.getByRole("searchbox", {
      name: "在当前会话中搜索",
    });
    await userEvent.type(findInput, "pnpm test");
    await waitFor(() =>
      expect(screen.getByText(/1 \/ \d+/)).toBeInTheDocument(),
    );
    // Switching to "对话" excludes hidden tool steps from search hit count
    fireEvent.click(screen.getByRole("button", { name: "对话" }));
    await waitFor(() => expect(screen.getByText("0 / 0")).toBeInTheDocument());

    fireEvent.change(findInput, { target: { value: "补丁修复" } });
    await waitFor(() => expect(screen.getByText("1 / 1")).toBeInTheDocument());
    fireEvent.click(screen.getByRole("button", { name: "下一处匹配" }));
    expect(screen.getByText("1 / 1")).toBeInTheDocument();
  });

  it("collapses no-final turns via 收起过程 and omits 改动 button when only non-edit steps fail", () => {
    const session: SessionMeta = {
      providerId: "claude",
      sessionId: "claude-no-final",
      title: "仅执行未收尾",
      projectDir: "/repo/app",
      sourcePath: "/mock/claude/no-final.jsonl",
    };

    const messages: SessionMessage[] = [
      {
        role: "user",
        content: "",
        blocks: [{ type: "text", text: "跑一下检查" }],
      },
      {
        role: "assistant",
        content: "",
        blocks: [
          {
            type: "thinking",
            text: "先跑测试看看状态",
          },
          {
            type: "tool_call",
            id: "sh-1",
            rawName: "Bash",
            kind: "shell",
            title: "cargo check",
            inputPreview: '{"command":"cargo check"}',
            inputTotalLen: 25,
          },
          {
            type: "tool_result",
            callId: "sh-1",
            status: "error",
            preview: "error: failed to compile",
            totalLen: 24,
            lineCount: 1,
            truncated: false,
            exitCode: 101,
          },
        ],
      },
    ];

    renderDetailPane({ session, messages });

    // No edit/write/diff -> "改动" filter button must NOT appear
    expect(
      screen.queryByRole("button", { name: "改动" }),
    ).not.toBeInTheDocument();

    // No final reply -> timeline auto-expands initially, so toolbar button says "收起过程"
    const collapseBtn = screen.getByRole("button", { name: "收起过程" });
    expect(collapseBtn).toHaveAttribute("aria-pressed", "true");
    expect(
      screen.getByRole("button", { name: /思考中… · 8 字/ }),
    ).toBeInTheDocument();

    // Clicking "收起过程" genuinely collapses the timeline even though !turn.final
    fireEvent.click(collapseBtn);
    expect(screen.getByRole("button", { name: "展开过程" })).toHaveAttribute(
      "aria-pressed",
      "false",
    );
    expect(
      screen.queryByRole("button", { name: /思考中… · 8 字/ }),
    ).not.toBeInTheDocument();
  });

  it("decodes data:image/png;base64 URLs directly without calling fetch() when copying lightbox images", async () => {
    const pngDataUrl =
      "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    const blob = dataUrlToBlob(pngDataUrl);
    expect(blob).not.toBeNull();
    expect(blob?.type).toBe("image/png");
    expect(blob?.size).toBeGreaterThan(0);

    const fetchSpy = vi.spyOn(globalThis, "fetch");
    const writeMock = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { write: writeMock },
    });
    if (typeof globalThis.ClipboardItem === "undefined") {
      vi.stubGlobal(
        "ClipboardItem",
        class MockClipboardItem {
          items: Record<string, Blob>;
          constructor(items: Record<string, Blob>) {
            this.items = items;
          }
        },
      );
    }

    await copyImage(pngDataUrl);
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(writeMock).toHaveBeenCalledTimes(1);
    fetchSpy.mockRestore();
  });
});
