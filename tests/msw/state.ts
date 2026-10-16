import type { SessionMessage, SessionMeta } from "@/types";

const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;
const messageKey = (providerId: string, sourcePath: string) =>
  `${providerId}:${sourcePath}`;

/**
 * `wire::content_identity_hex` 的镜像实现。
 *
 * 必须与 Rust 侧逐字节一致：FNV-1a 64 位，输入是 role 字节 + 0 分隔 + 时间戳的
 * 8 字节小端（缺失时为 i64::MIN 的补码）+ 正文 UTF-8 字节，输出 16 位小写 16 进制。
 * 用 BigInt 是为了不丢 64 位精度。
 */
const U64_MASK = (1n << 64n) - 1n;
const FNV_PRIME = 0x0000_0100_0000_01b3n;
const FNV_OFFSET_BASIS = 0xcbf2_9ce4_8422_2325n;
const I64_MIN_AS_U64 = 1n << 63n;

export const contentIdentityHex = (message: SessionMessage): string => {
  let hash = FNV_OFFSET_BASIS;
  const mix = (byte: number) => {
    hash = ((hash ^ BigInt(byte)) * FNV_PRIME) & U64_MASK;
  };

  for (const byte of new TextEncoder().encode(message.role)) mix(byte);
  mix(0);

  const ts = message.ts === undefined ? -I64_MIN_AS_U64 : BigInt(message.ts);
  const unsigned = ts < 0n ? ts + (1n << 64n) : ts;
  for (let index = 0; index < 8; index += 1) {
    mix(Number((unsigned >> BigInt(index * 8)) & 0xffn));
  }

  for (const byte of new TextEncoder().encode(message.content)) mix(byte);

  return hash.toString(16).padStart(16, "0");
};

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
