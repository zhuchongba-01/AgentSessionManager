import type {
  ContentRef,
  ImageRef,
  SessionMessage,
  SessionMeta,
} from "@/types";

const stableKey = (value: unknown): string => {
  if (Array.isArray(value)) return `[${value.map(stableKey).join(",")}]`;
  if (value && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, item]) => item !== undefined)
      .sort(([a], [b]) => (a < b ? -1 : 1))
      .map(([key, item]) => `${JSON.stringify(key)}:${stableKey(item)}`);
    return `{${entries.join(",")}}`;
  }
  return JSON.stringify(value);
};

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
  },
];

let sessions = defaultSessions();
let messages: Record<string, SessionMessage[]> = {
  [messageKey("codex", "/mock/codex/session-1.jsonl")]: [
    { role: "user", content: "First codex message" },
  ],
};
let blockContents: Record<string, string> = {};
let imageDataUrls: Record<string, string> = {};
let nextExportSaveOutcome:
  | { kind: "saved"; path?: string }
  | { kind: "cancelled" }
  | { kind: "error"; message: string } = { kind: "saved" };
let exportedMarkdownRecords: Array<{
  defaultFilename: string;
  defaultDir?: string;
  content: string;
  savedPath: string | null;
}> = [];
let openedExternalUrls: string[] = [];
let revealedPaths: string[] = [];

const blockKey = (providerId: string, sourcePath: string, ref: ContentRef) =>
  `${providerId}|${sourcePath}|${stableKey(ref)}`;

const imageKey = (providerId: string, sourcePath: string, image: ImageRef) =>
  `${providerId}|${sourcePath}|${stableKey(image)}`;

export const resetProviderState = () => {
  sessions = defaultSessions();
  messages = {
    [messageKey("codex", "/mock/codex/session-1.jsonl")]: [
      { role: "user", content: "First codex message" },
    ],
  };
  blockContents = {};
  imageDataUrls = {};
  nextExportSaveOutcome = { kind: "saved" };
  exportedMarkdownRecords = [];
  openedExternalUrls = [];
  revealedPaths = [];
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

export const setBlockContentFixture = (
  providerId: string,
  sourcePath: string,
  ref: ContentRef,
  fullText: string,
) => {
  blockContents[blockKey(providerId, sourcePath, ref)] = fullText;
};

export const getBlockContentFixture = (
  providerId: string,
  sourcePath: string,
  ref: ContentRef,
): string | undefined => blockContents[blockKey(providerId, sourcePath, ref)];

export const setSessionImageFixture = (
  providerId: string,
  sourcePath: string,
  image: ImageRef,
  dataUrl: string,
) => {
  imageDataUrls[imageKey(providerId, sourcePath, image)] = dataUrl;
};

export const getSessionImageFixture = (
  providerId: string,
  sourcePath: string,
  image: ImageRef,
): string | undefined => imageDataUrls[imageKey(providerId, sourcePath, image)];

export const setExportSaveOutcome = (
  outcome:
    | { kind: "saved"; path?: string }
    | { kind: "cancelled" }
    | { kind: "error"; message: string },
) => {
  nextExportSaveOutcome = outcome;
};

export const handleExportSaveRequest = (input: {
  defaultFilename: string;
  defaultDir?: string;
  content: string;
}): { ok: true; savedPath: string | null } | { ok: false; message: string } => {
  if (nextExportSaveOutcome.kind === "error") {
    return { ok: false, message: nextExportSaveOutcome.message };
  }
  const savedPath =
    nextExportSaveOutcome.kind === "cancelled"
      ? null
      : (nextExportSaveOutcome.path ??
        `/mock/exports/${input.defaultFilename}`);
  exportedMarkdownRecords.push({
    ...input,
    savedPath,
  });
  return { ok: true, savedPath };
};

export const getExportedMarkdownRecords = () => clone(exportedMarkdownRecords);

export const recordOpenedExternalUrl = (url: string) => {
  openedExternalUrls.push(url);
};

export const getOpenedExternalUrls = () => clone(openedExternalUrls);

export const recordRevealedPath = (path: string) => {
  revealedPaths.push(path);
};

export const getRevealedPaths = () => clone(revealedPaths);
