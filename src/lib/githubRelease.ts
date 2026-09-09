import packageMetadata from "../../package.json";

export const APP_VERSION = packageMetadata.version;
export const GITHUB_REPOSITORY_URL =
  "https://github.com/zhuchongba-01/AgentSessionManager";
export const GITHUB_LATEST_RELEASE_URL = `${GITHUB_REPOSITORY_URL}/releases/latest`;
export const GITHUB_LATEST_RELEASE_API =
  "https://api.github.com/repos/zhuchongba-01/AgentSessionManager/releases/latest";

export type AvailableUpdate = {
  version: string;
  notes: string | null;
};

type GitHubReleaseResponse = {
  tag_name?: unknown;
  body?: unknown;
  draft?: unknown;
  prerelease?: unknown;
};

const summarizeReleaseNotes = (value: unknown): string | null => {
  if (typeof value !== "string") return null;

  const lines = value
    .replace(/\r\n?/g, "\n")
    .split("\n")
    .map((line) => ({
      isHeading: /^\s{0,3}#{1,6}\s+/.test(line),
      text: line
        .replace(/^\s{0,3}#{1,6}\s+/, "")
        .replace(/^\s*(?:[-*+] |\d+[.)]\s+)/, "")
        .replace(/\[([^\]]+)]\([^\s)]+(?:\s+"[^"]*")?\)/g, "$1")
        .replace(/[`*_~]/g, "")
        .replace(/<[^>]+>/g, "")
        .trim(),
    }))
    .filter(({ isHeading, text }) => !isHeading && Boolean(text))
    .map(({ text }) => text)
    .slice(0, 2);

  if (lines.length === 0) return null;
  const summary = lines.join("\n");
  return summary.length > 420 ? `${summary.slice(0, 417).trimEnd()}…` : summary;
};

const parseVersion = (value: string): number[] | null => {
  const match = value.trim().match(/^v?(\d+(?:\.\d+)*)(?:[-+].*)?$/i);
  if (!match) return null;
  return match[1].split(".").map(Number);
};

export const isVersionNewer = (candidate: string, current: string): boolean => {
  const candidateParts = parseVersion(candidate);
  const currentParts = parseVersion(current);
  if (!candidateParts || !currentParts) return false;

  const length = Math.max(candidateParts.length, currentParts.length);
  for (let index = 0; index < length; index += 1) {
    const candidatePart = candidateParts[index] ?? 0;
    const currentPart = currentParts[index] ?? 0;
    if (candidatePart !== currentPart) return candidatePart > currentPart;
  }
  return false;
};

export const checkForAvailableUpdate = async (
  currentVersion = APP_VERSION,
  fetcher: typeof fetch = fetch,
): Promise<AvailableUpdate | null> => {
  const controller = new AbortController();
  const timeoutId = window.setTimeout(() => controller.abort(), 6000);

  try {
    const response = await fetcher(GITHUB_LATEST_RELEASE_API, {
      headers: {
        Accept: "application/vnd.github+json",
      },
      cache: "no-store",
      signal: controller.signal,
    });
    if (!response.ok) return null;

    const release = (await response.json()) as GitHubReleaseResponse;
    if (
      release.draft === true ||
      release.prerelease === true ||
      typeof release.tag_name !== "string"
    ) {
      return null;
    }

    const version = release.tag_name.replace(/^v/i, "");
    return isVersionNewer(version, currentVersion)
      ? { version, notes: summarizeReleaseNotes(release.body) }
      : null;
  } catch {
    // 更新检查不应影响本地会话管理；离线、限流或仓库尚无 Release 时静默隐藏。
    return null;
  } finally {
    window.clearTimeout(timeoutId);
  }
};
