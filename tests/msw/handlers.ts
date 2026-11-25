import { http, HttpResponse } from "msw";
import { APP_VERSION } from "@/lib/githubRelease";
import {
  contentIdentityHex,
  deleteSession,
  getBlockContentFixture,
  getSessionImageFixture,
  getSessionMessages,
  handleExportSaveRequest,
  listSessions,
  recordOpenedExternalUrl,
  recordRevealedPath,
} from "./state";
import type { ContentRef, ImageRef } from "@/types";

const TAURI_ENDPOINT = "http://tauri.local";
const GITHUB_RELEASE_ENDPOINT =
  "https://api.github.com/repos/zhuchongba-01/AgentSessionManager/releases/latest";
const WIRE_TRUNCATE_THRESHOLD = 4096;
const WIRE_PREVIEW_CHARS = 1500;

const success = <T>(payload: T) => HttpResponse.json(payload as never);

const readJson = async <T>(request: Request): Promise<T> => {
  const text = await request.text();
  return (text ? JSON.parse(text) : {}) as T;
};

export const handlers = [
  http.get(GITHUB_RELEASE_ENDPOINT, () =>
    success({ tag_name: `v${APP_VERSION}`, draft: false, prerelease: false }),
  ),
  http.post(`${TAURI_ENDPOINT}/list_sessions`, () =>
    success({ sessions: listSessions(), warnings: [] }),
  ),
  http.post(`${TAURI_ENDPOINT}/get_session_messages`, async ({ request }) => {
    const { providerId, sourcePath } = await readJson<{
      providerId: string;
      sourcePath: string;
    }>(request);
    const wireMessages = getSessionMessages(providerId, sourcePath).map(
      (message, index) => {
        const content = message.content ?? "";
        if (
          !message.truncated &&
          (!message.blocks || message.blocks.length === 0) &&
          content.length > WIRE_TRUNCATE_THRESHOLD
        ) {
          return {
            ...message,
            content: content.slice(0, WIRE_PREVIEW_CHARS),
            index,
            truncated: true,
            contentChars: content.length,
            contentIdentity: contentIdentityHex(message),
          };
        }
        return { ...message, content };
      },
    );
    return success(wireMessages);
  }),
  http.post(
    `${TAURI_ENDPOINT}/search_session_messages`,
    async ({ request }) => {
      const { providerId, sourcePath, query } = await readJson<{
        providerId: string;
        sourcePath: string;
        query: string;
      }>(request);
      const terms = query
        .trim()
        .toLocaleLowerCase()
        .split(/\s+/)
        .filter(Boolean);
      if (terms.length === 0) {
        return success<number[]>([]);
      }
      const matches: number[] = [];
      getSessionMessages(providerId, sourcePath).forEach((message, index) => {
        const lower = message.content.toLocaleLowerCase();
        if (terms.every((term) => lower.includes(term))) {
          matches.push(index);
        }
      });
      return success(matches);
    },
  ),
  // 与后端一致：长度或身份摘要对不上就拒绝返回，避免把另一条消息的正文当成这一条
  http.post(`${TAURI_ENDPOINT}/get_message_content`, async ({ request }) => {
    const { providerId, sourcePath, index, expectedChars, expectedIdentity } =
      await readJson<{
        providerId: string;
        sourcePath: string;
        index: number;
        expectedChars: number;
        expectedIdentity?: string;
      }>(request);
    const message = getSessionMessages(providerId, sourcePath)[index];
    if (!message || message.content.length !== expectedChars) {
      return new HttpResponse("会话已更新，请重新打开", { status: 400 });
    }
    if (
      expectedIdentity !== undefined &&
      contentIdentityHex(message) !== expectedIdentity
    ) {
      return new HttpResponse("会话已更新，请重新打开", { status: 400 });
    }
    return success(message.content);
  }),
  http.post(
    `${TAURI_ENDPOINT}/get_session_block_content`,
    async ({ request }) => {
      const {
        providerId,
        sourcePath,
        contentRef,
        offset = 0,
        limit = 512 * 1024,
      } = await readJson<{
        providerId: string;
        sourcePath: string;
        contentRef: ContentRef;
        offset?: number;
        limit?: number;
      }>(request);
      const fullText = getBlockContentFixture(
        providerId,
        sourcePath,
        contentRef,
      );
      if (fullText === undefined) {
        return new HttpResponse("未找到块内容", { status: 404 });
      }
      const chars = Array.from(fullText);
      const start = Math.min(Math.max(0, offset), chars.length);
      const end = Math.min(start + Math.max(1, limit), chars.length);
      const truncated = end < chars.length;
      return success({
        text: chars.slice(start, end).join(""),
        totalLen: chars.length,
        truncated,
        ...(truncated ? { nextOffset: end } : {}),
      });
    },
  ),
  http.post(`${TAURI_ENDPOINT}/get_session_image`, async ({ request }) => {
    const { providerId, sourcePath, image } = await readJson<{
      providerId: string;
      sourcePath: string;
      image: ImageRef;
    }>(request);
    const fixture = getSessionImageFixture(providerId, sourcePath, image);
    if (fixture !== undefined) {
      return success(fixture);
    }
    if (image.source.kind === "inline") {
      const base64 = getBlockContentFixture(
        providerId,
        sourcePath,
        image.source.content,
      );
      if (base64 !== undefined) {
        return success(`data:${image.mediaType};base64,${base64}`);
      }
    }
    return new HttpResponse("未找到图片资源", { status: 404 });
  }),
  http.post(`${TAURI_ENDPOINT}/delete_session`, async ({ request }) => {
    const input = await readJson<{
      providerId: string;
      sessionId: string;
      sourcePath: string;
    }>(request);
    return success(
      deleteSession(input.providerId, input.sessionId, input.sourcePath),
    );
  }),
  http.post(`${TAURI_ENDPOINT}/delete_sessions`, async ({ request }) => {
    const { items = [] } = await readJson<{
      items?: Array<{
        providerId: string;
        sessionId: string;
        sourcePath: string;
      }>;
    }>(request);
    return success(
      items.map((item) => ({
        ...item,
        success:
          deleteSession(item.providerId, item.sessionId, item.sourcePath)
            .status === "deleted",
        warnings: [],
      })),
    );
  }),
  http.post(`${TAURI_ENDPOINT}/get_pi_session_discovery`, () =>
    success({ status: "available" }),
  ),
  http.post(`${TAURI_ENDPOINT}/set_window_theme`, () => success(null)),
  http.post(`${TAURI_ENDPOINT}/sync_session_index`, () => {
    const total = listSessions().length;
    return success({ running: false, processed: total, total });
  }),
  http.post(`${TAURI_ENDPOINT}/search_session_content`, async ({ request }) => {
    const { query = "", providerId } = await readJson<{
      query?: string;
      providerId?: string | null;
    }>(request);
    const terms = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
    const allSessions = listSessions();
    const hits = allSessions
      .filter((s) => !providerId || s.providerId === providerId)
      .flatMap((session) => {
        if (!session.sourcePath || terms.length === 0) return [];
        const msgs = getSessionMessages(session.providerId, session.sourcePath);
        const snippets: Array<{
          messageIndex: number;
          text: string;
          highlights: Array<{ start: number; end: number }>;
        }> = [];
        let matchCount = 0;
        msgs.forEach((msg, idx) => {
          if (msg.injected) return;
          if (msg.role !== "user" && msg.role !== "assistant") return;
          const blockText =
            msg.blocks
              ?.filter(
                (b): b is Extract<typeof b, { type: "text" }> =>
                  b.type === "text",
              )
              .map((b) => b.text)
              .join("\n\n") ?? "";
          const searchable = blockText || msg.content || "";
          const lower = searchable.toLocaleLowerCase();
          if (!terms.every((term) => lower.includes(term))) return;
          matchCount += 1;
          if (snippets.length < 3) {
            const firstTerm = terms[0];
            const start = lower.indexOf(firstTerm);
            const highlights =
              start >= 0 ? [{ start, end: start + firstTerm.length }] : [];
            snippets.push({
              messageIndex: idx,
              text: searchable,
              highlights,
            });
          }
        });
        if (matchCount === 0) return [];
        return [
          {
            providerId: session.providerId,
            sourcePath: session.sourcePath,
            snippets,
            matchCount,
          },
        ];
      });
    return success({
      hits,
      status: {
        running: false,
        processed: allSessions.length,
        total: allSessions.length,
      },
    });
  }),
  http.post(`${TAURI_ENDPOINT}/cancel_session_content_search`, () =>
    success(null),
  ),
  http.post(`${TAURI_ENDPOINT}/clear_session_index`, () => success(null)),
  http.post(`${TAURI_ENDPOINT}/reveal_session_path`, async ({ request }) => {
    const { path } = await readJson<{ path: string }>(request);
    recordRevealedPath(path);
    return success(null);
  }),
  http.post(`${TAURI_ENDPOINT}/open_external_url`, async ({ request }) => {
    const { url } = await readJson<{ url: string }>(request);
    recordOpenedExternalUrl(url);
    return success(null);
  }),
  http.post(`${TAURI_ENDPOINT}/save_markdown_export`, async ({ request }) => {
    const input = await readJson<{
      defaultFilename: string;
      defaultDir?: string;
      content: string;
    }>(request);
    const result = handleExportSaveRequest(input);
    if (!result.ok) {
      return new HttpResponse(result.message, { status: 500 });
    }
    return success(result.savedPath);
  }),
];
