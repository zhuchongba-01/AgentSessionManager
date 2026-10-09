import { act, fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SessionManagerPage } from "@/components/sessions/SessionManagerPage";
import { ThemeProvider } from "@/components/theme-provider";
import { sessionsApi } from "@/lib/api/sessions";
import type { SessionMessage } from "@/types";
import { setSessionFixtures } from "../msw/state";

// jsdom has no layout: expose rows deterministically to test page wiring.
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({ count }: { count: number }) => ({
    getVirtualItems: () =>
      Array.from({ length: count }, (_, index) => ({
        index,
        key: index,
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
function renderReader() {
  setSessionFixtures(
    [
      {
        providerId: "claude",
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
        <SessionManagerPage appId="claude" />
      </ThemeProvider>
    </QueryClientProvider>,
  );
}

describe("session reading integration", () => {
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
});
