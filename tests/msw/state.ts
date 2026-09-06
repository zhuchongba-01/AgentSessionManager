import type { SessionMessage, SessionMeta } from "@/types";

const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;
const messageKey = (providerId: string, sourcePath: string) =>
  `${providerId}:${sourcePath}`;

const defaultSessions = (): SessionMeta[] => [
  {
    providerId: "codex",
    sessionId: "codex-session-1",
    title: "Codex Session One",
    projectDir: "/mock/codex",
    sourcePath: "/mock/codex/session-1.jsonl",
    resumeCommand: "codex resume codex-session-1",
  },
];

let sessions = defaultSessions();
let messages: Record<string, SessionMessage[]> = {
  [messageKey("codex", "/mock/codex/session-1.jsonl")]: [
    { role: "user", content: "First codex message" },
  ],
};

export const resetProviderState = () => {
  sessions = defaultSessions();
  messages = {
    [messageKey("codex", "/mock/codex/session-1.jsonl")]: [
      { role: "user", content: "First codex message" },
    ],
  };
};

export const listSessions = () => clone(sessions);

export const getSessionMessages = (providerId: string, sourcePath: string) =>
  clone(messages[messageKey(providerId, sourcePath)] ?? []);

export const deleteSession = (
  providerId: string,
  sessionId: string,
  sourcePath: string,
) => {
  sessions = sessions.filter(
    (session) =>
      session.providerId !== providerId ||
      session.sessionId !== sessionId ||
      session.sourcePath !== sourcePath,
  );
  delete messages[messageKey(providerId, sourcePath)];
  return { status: "deleted" as const };
};

export const setSessionFixtures = (
  nextSessions: SessionMeta[],
  nextMessages: Record<string, SessionMessage[]>,
) => {
  sessions = clone(nextSessions);
  messages = clone(nextMessages);
};
