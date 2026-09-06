import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import App from "@/App";
import { ThemeProvider } from "@/components/theme-provider";
import { setSessionFixtures } from "../msw/state";

const renderApp = () => {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });

  return render(
    <QueryClientProvider client={client}>
      <ThemeProvider defaultTheme="light" storageKey="test-app-theme">
        <App />
      </ThemeProvider>
    </QueryClientProvider>,
  );
};

describe("standalone Agent session manager", () => {
  beforeEach(() => {
    localStorage.removeItem("test-app-theme");
    localStorage.removeItem("agent-session-manager.listViewMode");
    localStorage.removeItem("agent-session-manager.groupExpansionState");
    document.documentElement.classList.remove("light", "dark");

    setSessionFixtures(
      [
        {
          providerId: "codex",
          sessionId: "session-1",
          title: "设计统一的 Agent 会话工具",
          projectDir: "E:\\项目\\Cat",
          sourcePath: "C:\\sessions\\session-1.jsonl",
          createdAt: Date.now() - 60_000,
          lastActiveAt: Date.now() - 30_000,
          resumeCommand: "codex resume session-1",
        },
      ],
      {
        "codex:C:\\sessions\\session-1.jsonl": [
          {
            role: "user",
            content: "做一个独立的 Agent 会话管理器",
            ts: Date.now() - 30_000,
          },
        ],
      },
    );
  });

  it("renders the compact standalone session manager shell", async () => {
    renderApp();

    // 应用名与版本在原生标题栏；应用内以右上角记录数作为壳层断言
    expect(await screen.findByText("1 个会话")).toBeInTheDocument();
    expect(
      await screen.findByRole("button", { name: "重新扫描" }),
    ).toBeInTheDocument();

    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: "设计统一的 Agent 会话工具" }),
      ).toBeInTheDocument(),
    );
    expect(screen.queryByText("本地会话总览")).not.toBeInTheDocument();
    expect(document.querySelector(".asm-rail")).not.toBeInTheDocument();
  });

  it("uses one button to switch between light and dark themes", async () => {
    renderApp();

    const themeButton = screen.getByRole("button", {
      name: "切换到暗色主题",
    });
    expect(
      screen.getAllByRole("button", { name: /切换到.*主题/ }),
    ).toHaveLength(1);

    fireEvent.click(themeButton);
    await waitFor(() => expect(document.documentElement).toHaveClass("dark"));
    expect(
      screen.getByRole("button", { name: "切换到明亮主题" }),
    ).toBeInTheDocument();
  });
});
