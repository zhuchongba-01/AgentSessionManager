import { http, HttpResponse } from "msw";
import { deleteSession, getSessionMessages, listSessions } from "./state";

const TAURI_ENDPOINT = "http://tauri.local";
const GITHUB_RELEASE_ENDPOINT =
  "https://api.github.com/repos/zhuchongba-01/AgentSessionManager/releases/latest";
const success = <T>(payload: T) => HttpResponse.json(payload as never);

const readJson = async <T>(request: Request): Promise<T> => {
  const text = await request.text();
  return (text ? JSON.parse(text) : {}) as T;
};

export const handlers = [
  http.get(GITHUB_RELEASE_ENDPOINT, () =>
    success({ tag_name: "v1.3.9", draft: false, prerelease: false }),
  ),
  http.post(`${TAURI_ENDPOINT}/list_sessions`, () =>
    success({ sessions: listSessions(), warnings: [] }),
  ),
  http.post(`${TAURI_ENDPOINT}/get_session_messages`, async ({ request }) => {
    const { providerId, sourcePath } = await readJson<{
      providerId: string;
      sourcePath: string;
    }>(request);
    return success(getSessionMessages(providerId, sourcePath));
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
  http.post(`${TAURI_ENDPOINT}/session_resume`, async ({ request }) => {
    const { providerId, sourcePath } = await readJson<{
      providerId: string;
      sourcePath: string;
    }>(request);
    const session = listSessions().find(
      (item) =>
        item.providerId === providerId && item.sourcePath === sourcePath,
    );
    if (
      !session?.resumeCommand ||
      session.cleanupPending ||
      session.archived ||
      session.residual
    ) {
      return HttpResponse.json(
        { error: "Session cannot be resumed" },
        { status: 400 },
      );
    }
    return success(session.resumeCommand);
  }),
  http.post(`${TAURI_ENDPOINT}/get_pi_session_discovery`, () =>
    success({ status: "available" }),
  ),
  http.post(`${TAURI_ENDPOINT}/set_window_theme`, () => success(null)),
];
