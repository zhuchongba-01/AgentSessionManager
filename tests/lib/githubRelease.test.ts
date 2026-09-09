import { describe, expect, it, vi } from "vitest";
import {
  checkForAvailableUpdate,
  GITHUB_LATEST_RELEASE_API,
  isVersionNewer,
} from "@/lib/githubRelease";

describe("GitHub release update check", () => {
  it("compares numeric release versions", () => {
    expect(isVersionNewer("1.3.9", "1.3.8")).toBe(true);
    expect(isVersionNewer("1.10.0", "1.9.9")).toBe(true);
    expect(isVersionNewer("v1.3.8", "1.3.8")).toBe(false);
    expect(isVersionNewer("1.3.7", "1.3.8")).toBe(false);
    expect(isVersionNewer("not-a-version", "1.3.8")).toBe(false);
  });

  it("returns a newer stable release", async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        JSON.stringify({
          tag_name: "v1.4.0",
          body: "## 更新内容\n- 修复会话扫描\n- 优化列表布局",
          draft: false,
          prerelease: false,
        }),
        { status: 200 },
      ),
    );

    await expect(checkForAvailableUpdate("1.3.8", fetcher)).resolves.toEqual({
      version: "1.4.0",
      notes: "修复会话扫描\n优化列表布局",
    });
    expect(fetcher).toHaveBeenCalledWith(
      GITHUB_LATEST_RELEASE_API,
      expect.objectContaining({ cache: "no-store" }),
    );
  });

  it("silently hides missing, old, draft, and unavailable releases", async () => {
    const oldRelease = vi
      .fn<typeof fetch>()
      .mockResolvedValue(
        new Response(JSON.stringify({ tag_name: "v1.3.8" }), { status: 200 }),
      );
    const draftRelease = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(JSON.stringify({ tag_name: "v2.0.0", draft: true }), {
        status: 200,
      }),
    );
    const unavailable = vi
      .fn<typeof fetch>()
      .mockResolvedValue(new Response(null, { status: 404 }));

    await expect(
      checkForAvailableUpdate("1.3.8", oldRelease),
    ).resolves.toBeNull();
    await expect(
      checkForAvailableUpdate("1.3.8", draftRelease),
    ).resolves.toBeNull();
    await expect(
      checkForAvailableUpdate("1.3.8", unavailable),
    ).resolves.toBeNull();
  });
});
