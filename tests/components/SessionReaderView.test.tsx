import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { ReaderContext } from "@/components/sessions/reader/context";
import { getAgentReaderStyle } from "@/components/sessions/reader/agentStyles";
import { parseDiff, SessionDiff } from "@/components/sessions/reader/SessionDiff";
import {
  SessionMarkdown,
  SessionPlainText,
} from "@/components/sessions/reader/SessionMarkdown";
import { SessionReader } from "@/components/sessions/reader/SessionReader";
import { TooltipProvider } from "@/components/ui/tooltip";
import { clearSessionImageCache } from "@/lib/query/sessions";
import type { ContentRef, ImageRef, SessionMessage, SessionMeta } from "@/types";
import {
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
    range: { startIndex: 0, endIndex: Math.max(0, count - 1) },
  }),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn(),
}));

function renderSessionReader({
  session,
  messages,
  appName = "Codex",
  listQuery = "",
  initialFind,
}: {
  session: SessionMeta;
  messages: SessionMessage[];
  appName?: string;
  listQuery?: string;
  initialFind?: { query: string; messageIndex?: number };
}) {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
    },
  });
  const onCopy = vi.fn();
  const onBack = vi.fn();
  const onReload = vi.fn();
  const onDelete = vi.fn();
  const onPrev = vi.fn();
  const onNext = vi.fn();

  const view = render(
    <QueryClientProvider client={client}>
      <TooltipProvider>
        <SessionReader
          session={session}
          appName={appName}
          transcript={{
            messages,
            isStreaming: false,
            progress: { loaded: messages.length, total: messages.length },
            isLoading: false,
            isError: false,
            error: null,
            refetch: async () => undefined,
          }}
          listQuery={listQuery}
          initialFind={initialFind}
          hasPrev={true}
          hasNext={true}
          onPrev={onPrev}
          onNext={onNext}
          onBack={onBack}
          onCopy={onCopy}
          onReload={onReload}
          onDelete={onDelete}
        />
      </TooltipProvider>
    </QueryClientProvider>,
  );

  return {
    client,
    onCopy,
    onBack,
    onReload,
    onDelete,
    onPrev,
    onNext,
    ...view,
  };
}

describe("cc-switch aligned SessionReader suite", () => {
  beforeEach(() => {
    resetProviderState();
    clearSessionImageCache();
    window.localStorage.removeItem("cc-switch.sessionReader.expandTimelines");
    window.localStorage.removeItem("cc-switch.sessionReader.outline");
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

  it("renders user plain text preserving Markdown symbols while formatting fenced code, and renders assistant GFM + PathChips", () => {
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
                content={
                  "请查看 `/repo/app/src/auth.ts:42` 并核对表格：\n\n| 字段 | 状态 |\n| --- | --- |\n| exp | 正常 |"
                }
                projectDir="/repo/app"
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
  });

  it("renders chain-of-thought preview, lazy loads full thinking via ContentRef, and keeps redacted thinking static", async () => {
    const fullThinkingRef: ContentRef = {
      kind: "jsonl",
      offset: 128,
      len: 1024,
      pointer: "/message/content/0/thinking",
    };
    setBlockContentFixture(
      "codex",
      "/mock/codex/session-1.jsonl",
      fullThinkingRef,
      "完整深度推理正文：服务端 JWT 公钥轮换后未同步到边缘网关。",
    );

    const session: SessionMeta = {
      providerId: "codex",
      sessionId: "codex-session-1",
      title: "JWT 校验排查",
      projectDir: "/repo/app",
      sourcePath: "/mock/codex/session-1.jsonl",
      resumeCommand: "codex resume codex-session-1",
    };

    const messages: SessionMessage[] = [
      {
        role: "user",
        content: "为什么 JWT 校验失败？",
        ts: 1000,
        blocks: [{ type: "text", text: "为什么 JWT 校验失败？" }],
      },
      {
        role: "assistant",
        content: "原因是边缘网关未刷新公钥。",
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
        content: "再检查加密推理块",
        ts: 2000,
        blocks: [{ type: "text", text: "再检查加密推理块" }],
      },
      {
        role: "assistant",
        content: "加密思考块已安全处理。",
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

    renderSessionReader({ session, messages, appName: "Codex" });

    const thinkingBtn = screen.getByRole("button", {
      name: /推理.*核对时钟偏移与签名密钥/,
    });
    expect(thinkingBtn).toHaveAttribute("aria-expanded", "false");

    expect(screen.getByText("思考内容不可见")).toBeInTheDocument();

    fireEvent.click(thinkingBtn);
    expect(thinkingBtn).toHaveAttribute("aria-expanded", "true");
    // Stage 1: inline preview renders immediately with "显示全部" trigger
    expect(screen.getByText(/预览摘要片段/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "显示全部" }));
    // Stage 2: full thinking text fetched via get_session_block_content
    expect(
      await screen.findByText(
        "完整深度推理正文：服务端 JWT 公钥轮换后未同步到边缘网关。",
      ),
    ).toBeInTheDocument();
  });

  it("merges consecutive Codex read/search steps into Explored, pins failed commands in collapsed timeline, filters modes, and supports outline + inline search", async () => {
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
        ],
      },
      {
        role: "user",
        content: "排查并修复 auth.ts",
        ts: 1000,
        blocks: [
          { type: "text", text: "排查并修复 auth.ts" },
          { type: "image", image: inlineImg },
        ],
      },
      {
        role: "assistant",
        content: "已定位并应用补丁修复。",
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

    renderSessionReader({ session, messages, appName: "Codex" });

    // Injected message is hidden by default
    expect(
      screen.queryByText(/# AGENTS\.md injected instructions/),
    ).not.toBeInTheDocument();

    // Right-hand SessionOutline is open by default
    expect(
      screen.getByRole("navigation", { name: "对话目录" }),
    ).toBeInTheDocument();

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

    // Expanding timeline reveals merged Explored step
    fireEvent.click(timelineBtn);
    const exploredBtn = screen.getByRole("button", {
      name: /Explored.*查看了 1 个文件、搜索 1 次/,
    });
    expect(exploredBtn).toBeInTheDocument();
    fireEvent.click(exploredBtn);
    expect(
      screen.getByRole("button", { name: /Read.*src\/auth\.ts/ }),
    ).toBeInTheDocument();

    // Filter switching: "改动" shows only file changes
    fireEvent.click(screen.getByRole("radio", { name: "改动" }));
    expect(
      screen.getByRole("button", { name: /Edited.*src\/auth\.ts/ }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Ran.*pnpm test/ }),
    ).not.toBeInTheDocument();

    // Switch back to "全部" and test inline find
    fireEvent.click(screen.getByRole("radio", { name: "全部" }));
    fireEvent.click(screen.getByRole("button", { name: "在会话中查找" }));
    const findInput = screen.getByRole("textbox", { name: "查找内容" });
    await userEvent.type(findInput, "补丁修复");
    await waitFor(() =>
      expect(screen.getByText(/1 \/ \d+/)).toBeInTheDocument(),
    );
  });
});
