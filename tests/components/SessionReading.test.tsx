import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import * as core from "@tauri-apps/api/core";
import { deliverChannelMessage } from "../msw/tauriMocks";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionManagerPage } from "@/components/sessions/SessionManagerPage";
import { ThemeProvider } from "@/components/theme-provider";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMessage } from "@/types";
import { setSessionFixtures } from "../msw/state";

// jsdom has no layout: expose rows deterministically to test page wiring.
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
        start: index * 120,
        end: (index + 1) * 120,
      })),
    getTotalSize: () => count * 120,
    measureElement: () => undefined,
    scrollToIndex: () => undefined,
    scrollOffset: 0,
    scrollRect: { height: 600 },
  }),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));
vi.mock("@/lib/githubRelease", async (original) => ({
  ...(await original<typeof import("@/lib/githubRelease")>()),
  checkForAvailableUpdate: vi.fn(async () => null),
}));

const clients: QueryClient[] = [];
afterEach(() => {
  clients.forEach((client) => client.clear());
  clients.length = 0;
  vi.restoreAllMocks();
});

const preview: SessionMessage = {
  role: "tool",
  content: "same preview",
  index: 0,
  truncated: true,
  contentChars: 6000,
};
function renderReader(providerId = "claude") {
  setSessionFixtures(
    [
      {
        providerId,
        sessionId: "read-1",
        title: "Reading fixture",
        sourcePath: "/mock/reading.jsonl",
      },
    ],
    {},
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  clients.push(client);
  return render(
    <QueryClientProvider client={client}>
      <ThemeProvider defaultTheme="light" storageKey="reading-test-theme">
        <SessionManagerPage appId={providerId} />
      </ThemeProvider>
    </QueryClientProvider>,
  );
}

describe("session reading integration", () => {
  it("keeps delayed Codex messages and navigation after invoke finishes before Channel fetch", async () => {
    const originalInvoke = core.invoke;
    let channel: unknown;
    let invoked = false;
    vi.spyOn(core, "invoke").mockImplementation(async (command, args) => {
      if (command !== "stream_session_messages")
        return originalInvoke(command, args);
      channel = (args as { onChunk: unknown }).onChunk;
      deliverChannelMessage(channel, 0, {
        kind: "header",
        total: 3,
        approxBytes: 10_000,
      });
      // Small Done arrives first on the wire, but waits behind payload index 1.
      deliverChannelMessage(channel, 2, { kind: "done", delivered: 3 });
      invoked = true;
      return undefined as never;
    });
    renderReader("codex");
    await waitFor(() => expect(invoked).toBe(true));
    expect(screen.getByText("正在读取会话…")).toBeInTheDocument();
    const key = ["sessionMessages", "codex", "/mock/reading.jsonl"];
    expect(clients[0].getQueryData(key)).toBeUndefined();
    const messages = [
      {
        role: "user",
        content:
          "# AGENTS.md instructions for hidden workspace\n" + "x".repeat(9000),
      },
      { role: "user", content: "Codex delayed visible question" },
      { role: "assistant", content: "Codex delayed visible answer" },
    ];
    expect(
      new TextEncoder().encode(JSON.stringify(messages)).length,
    ).toBeGreaterThanOrEqual(8192);
    await act(async () =>
      deliverChannelMessage(channel, 1, {
        kind: "messages",
        start: 0,
        messages,
      }),
    );
    expect(
      await screen.findByText("Codex delayed visible question"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Codex delayed visible answer"),
    ).toBeInTheDocument();
    const tick = screen.getByRole("button", { name: "跳转到第 1 个用户问题" });
    expect(tick).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "跳转到第 2 个用户问题" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText(/# AGENTS.md instructions/),
    ).not.toBeInTheDocument();
    await waitFor(() =>
      expect(clients[0].getQueryState(key)?.fetchStatus).toBe("idle"),
    );
    expect(clients[0].getQueryData(key)).toMatchObject({ messages });
    expect(screen.queryByText("正在读取会话…")).not.toBeInTheDocument();
    fireEvent.click(tick);
    expect(
      screen.getByText("Codex delayed visible answer"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "跳转到第 1 个用户问题" }),
    ).toBeInTheDocument();
  });

  it("aborts the stream on unmount and ignores later Channel callbacks", async () => {
    const originalInvoke = core.invoke;
    let channel: unknown;
    vi.spyOn(core, "invoke").mockImplementation(async (command, args) => {
      if (command !== "stream_session_messages")
        return originalInvoke(command, args);
      channel = (args as { onChunk: unknown }).onChunk;
      deliverChannelMessage(channel, 0, {
        kind: "header",
        total: 1,
        approxBytes: 0,
      });
      return undefined as never;
    });
    const spy = vi.spyOn(sessionsApi, "streamMessages");
    const view = renderReader();
    await waitFor(() => expect(channel).toBeDefined());
    const signal = spy.mock.calls[0][3]!;
    view.unmount();
    expect(signal.aborted).toBe(true);
    await act(async () => {
      deliverChannelMessage(channel, 1, {
        kind: "messages",
        start: 0,
        messages: [{ role: "user", content: "late data" }],
      });
      deliverChannelMessage(channel, 2, { kind: "done", delivered: 1 });
    });
    expect(
      clients[0].getQueryData([
        "sessionMessages",
        "claude",
        "/mock/reading.jsonl",
      ]),
    ).toBeUndefined();
  });

  it("shows the first chunk while the rest is still pending", async () => {
    let progress!: (messages: SessionMessage[]) => void;
    let finish!: (messages: SessionMessage[]) => void;
    vi.spyOn(sessionsApi, "streamMessages").mockImplementation(
      (_p, _s, onProgress) => {
        progress = onProgress!;
        return new Promise((resolve) => {
          finish = resolve;
        });
      },
    );
    renderReader();
    await screen.findByText("正在读取会话…");
    const first = [{ role: "assistant", content: "first chunk body" }];
    act(() => progress(first));
    expect(await screen.findByText("first chunk body")).toBeInTheDocument();
    expect(screen.getByText("正在继续读取会话…")).toBeInTheDocument();
    expect(screen.queryByText("正在读取会话…")).not.toBeInTheDocument();
    await act(async () =>
      finish([...first, { role: "assistant", content: "last chunk body" }]),
    );
    expect(await screen.findByText("last chunk body")).toBeInTheDocument();
    expect(screen.queryByText("正在继续读取会话…")).not.toBeInTheDocument();
  });

  it("discards full-text cache on rescan even with identical previews and lengths", async () => {
    vi.spyOn(sessionsApi, "streamMessages").mockImplementation(async () => [
      { ...preview },
    ]);
    const fullSpy = vi
      .spyOn(sessionsApi, "getMessageContent")
      .mockResolvedValueOnce("old full body")
      .mockResolvedValueOnce("new full body");
    renderReader();
    fireEvent.click(
      await screen.findByRole("button", { name: /展开完整内容/ }),
    );
    expect(await screen.findByText("old full body")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /重新扫描/ }));
    await screen.findByRole("button", { name: /展开完整内容/ });
    expect(screen.queryByText("old full body")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /展开完整内容/ }));
    expect(await screen.findByText("new full body")).toBeInTheDocument();
    expect(fullSpy).toHaveBeenCalledTimes(2);
  });

  it("reports a stream failure after partial delivery instead of keeping it as complete data", async () => {
    let progress!: (messages: SessionMessage[]) => void;
    let fail!: (error: Error) => void;
    vi.spyOn(sessionsApi, "streamMessages").mockImplementation(
      (_p, _s, onProgress) => {
        progress = onProgress!;
        return new Promise((_resolve, reject) => {
          fail = reject;
        });
      },
    );
    renderReader();
    await screen.findByText("正在读取会话…");
    act(() => progress([{ role: "assistant", content: "partial body" }]));
    await screen.findByText("partial body");
    await act(async () => fail(new Error("stream interrupted")));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "stream interrupted",
    );
    expect(screen.queryByText("partial body")).not.toBeInTheDocument();
    expect(
      clients[0].getQueryData([
        "sessionMessages",
        "claude",
        "/mock/reading.jsonl",
      ]),
    ).toBeUndefined();
  });

  it("does not refetch on window focus and preserves existing transcript during background refetch", async () => {
    let resolveRefetch!: (messages: SessionMessage[]) => void;
    const streamSpy = vi
      .spyOn(sessionsApi, "streamMessages")
      .mockResolvedValueOnce([{ role: "assistant", content: "cached reply" }])
      .mockImplementationOnce(
        (_p, _s, onProgress) =>
          new Promise((resolve) => {
            onProgress?.([]);
            resolveRefetch = resolve;
          }),
      );

    renderReader();
    expect(await screen.findByText("cached reply")).toBeInTheDocument();
    expect(streamSpy).toHaveBeenCalledTimes(1);

    // Window focus should NOT trigger a refetch (refetchOnWindowFocus: false)
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(streamSpy).toHaveBeenCalledTimes(1);

    // Manual background invalidation keeps existing transcript visible without skeleton flash
    act(() => {
      void clients[0].invalidateQueries({ queryKey: ["sessionMessages"] });
    });
    await waitFor(() => expect(streamSpy).toHaveBeenCalledTimes(2));
    expect(screen.getByText("cached reply")).toBeInTheDocument();
    expect(screen.queryByText("正在读取会话…")).not.toBeInTheDocument();

    await act(async () => {
      resolveRefetch([{ role: "assistant", content: "refreshed reply" }]);
    });
    expect(await screen.findByText("refreshed reply")).toBeInTheDocument();
  });
});
