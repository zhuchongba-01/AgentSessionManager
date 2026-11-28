import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  sessionsApi,
  type DeleteSessionOptions,
  type DeleteSessionReply,
} from "@/lib/api/sessions";
import type {
  ContentRef,
  ImageRef,
  SessionMessage,
  SessionMeta,
} from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";

export const stableKey = (value: unknown): string => {
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

export const useSessionsQuery = () => {
  const client = useQueryClient();
  const { data: warnings = [] } = useQuery<string[]>({
    queryKey: ["sessionScanWarnings"],
    queryFn: async () => [],
    initialData: [],
    enabled: false,
  });
  const query = useQuery<SessionMeta[]>({
    queryKey: ["sessions"],
    queryFn: () =>
      sessionsApi.list((issues) =>
        client.setQueryData(["sessionScanWarnings"], issues),
      ),
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
  return { ...query, warnings };
};

interface SessionTranscript {
  messages: SessionMessage[];
  revision: number;
}

let nextMessageRevision = 0;

export const useSessionMessagesQuery = (
  providerId?: string,
  sourcePath?: string,
) => {
  const activeRequest = useRef(0);
  const [streamed, setStreamed] = useState<
    (SessionTranscript & { providerId: string; sourcePath: string }) | null
  >(null);

  const query = useQuery<SessionTranscript>({
    queryKey: ["sessionMessages", providerId, sourcePath],
    queryFn: async ({ signal }) => {
      const revision = ++nextMessageRevision;
      activeRequest.current = revision;
      setStreamed({
        providerId: providerId!,
        sourcePath: sourcePath!,
        revision,
        messages: [],
      });
      let rafId: number | null = null;
      let latestPartial: SessionMessage[] | null = null;
      const cancelPendingProgress = () => {
        if (rafId !== null && typeof cancelAnimationFrame === "function") {
          cancelAnimationFrame(rafId);
          rafId = null;
        }
        latestPartial = null;
      };

      try {
        const messages = await sessionsApi.streamMessages(
          providerId!,
          sourcePath!,
          (partial) => {
            if (signal.aborted || activeRequest.current !== revision) return;
            latestPartial = partial;
            if (typeof requestAnimationFrame !== "function") {
              setStreamed({
                providerId: providerId!,
                sourcePath: sourcePath!,
                revision,
                messages: partial,
              });
              return;
            }
            if (rafId !== null) return;
            rafId = requestAnimationFrame(() => {
              rafId = null;
              if (
                signal.aborted ||
                activeRequest.current !== revision ||
                latestPartial === null
              ) {
                return;
              }
              setStreamed({
                providerId: providerId!,
                sourcePath: sourcePath!,
                revision,
                messages: latestPartial,
              });
            });
          },
          signal,
        );
        return { messages, revision };
      } finally {
        cancelPendingProgress();
      }
    },
    enabled: Boolean(providerId && sourcePath),
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });

  const isStreamingActive =
    !query.data &&
    query.isFetching &&
    streamed !== null &&
    streamed.providerId === providerId &&
    streamed.sourcePath === sourcePath;

  const snapshot = isStreamingActive ? streamed : query.data;

  const stampedMessages = useMemo(
    () =>
      snapshot?.messages.map((msg, idx) =>
        msg.transcriptIndex !== undefined
          ? msg
          : { ...msg, transcriptIndex: msg.index ?? idx },
      ),
    [snapshot?.messages],
  );

  return {
    ...query,
    data: stampedMessages,
    isStreaming: isStreamingActive,
    messageRevision: snapshot?.revision ?? 0,
  };
};

// ─── 按需分页读取块全文（512K/页，前端上限 2M 字符） ─────────────────────

export const BLOCK_PAGE_CHARS = 512 * 1024;
export const BLOCK_MAX_CHARS = 2 * 1024 * 1024;

export interface BlockContentResult {
  text: string;
  totalLen: number | null;
  hasMore: boolean;
  tooLarge: boolean;
  loadMore: () => void;
  isLoading: boolean;
  isFetchingMore: boolean;
  error: unknown;
}

export const useBlockContent = (
  providerId: string | undefined,
  sourcePath: string | undefined,
  ref: ContentRef | undefined,
  options: { enabled?: boolean } = {},
): BlockContentResult => {
  const query = useInfiniteQuery({
    queryKey: [
      "sessionBlockContent",
      providerId,
      sourcePath,
      ref ? stableKey(ref) : null,
    ],
    queryFn: ({ pageParam }) =>
      sessionsApi.getBlockContent(providerId!, sourcePath!, ref!, {
        offset: pageParam,
        limit: BLOCK_PAGE_CHARS,
      }),
    initialPageParam: 0,
    getNextPageParam: (last, pages) => {
      const loaded = pages.reduce((sum, page) => sum + page.text.length, 0);
      return last.truncated &&
        last.nextOffset !== undefined &&
        loaded < BLOCK_MAX_CHARS
        ? last.nextOffset
        : undefined;
    },
    enabled:
      Boolean(providerId && sourcePath && ref) && options.enabled !== false,
    staleTime: Infinity,
  });

  const pages = query.data?.pages ?? [];
  const text = useMemo(() => pages.map((page) => page.text).join(""), [pages]);
  const last = pages[pages.length - 1];

  return {
    text,
    totalLen: last?.totalLen ?? null,
    hasMore: Boolean(query.hasNextPage),
    tooLarge: Boolean(last?.truncated) && !query.hasNextPage,
    loadMore: () => {
      void query.fetchNextPage();
    },
    isLoading: query.isLoading,
    isFetchingMore: query.isFetchingNextPage,
    error: query.error,
  };
};

// ─── 会话图片 Data URL LRU 缓存（32 张，兼容 img-src 'self' data: CSP） ──

export const SESSION_IMAGE_CACHE_SIZE = 32;
const imageCache = new Map<string, string>();
const imageLoads = new Map<string, Promise<string>>();
let imageCacheGeneration = 0;

export const clearSessionImageCache = () => {
  imageCacheGeneration += 1;
  imageCache.clear();
  imageLoads.clear();
};

const loadImageDataUrl = (
  key: string,
  providerId: string,
  sourcePath: string,
  image: ImageRef,
): Promise<string> => {
  const existing = imageCache.get(key);
  if (existing) {
    imageCache.delete(key);
    imageCache.set(key, existing);
    return Promise.resolve(existing);
  }
  let pending = imageLoads.get(key);
  if (!pending) {
    const gen = imageCacheGeneration;
    pending = sessionsApi
      .getImage(providerId, sourcePath, image)
      .then((dataUrl) => {
        if (gen !== imageCacheGeneration) return "";
        imageCache.delete(key);
        imageCache.set(key, dataUrl);
        while (imageCache.size > SESSION_IMAGE_CACHE_SIZE) {
          const oldest = imageCache.keys().next().value;
          if (oldest === undefined) break;
          imageCache.delete(oldest);
        }
        return dataUrl;
      })
      .finally(() => {
        imageLoads.delete(key);
      });
    imageLoads.set(key, pending);
  }
  return pending;
};

export interface SessionImageResult {
  url: string | null;
  isLoading: boolean;
  error: unknown;
}

export const useSessionImage = (
  providerId: string | undefined,
  sourcePath: string | undefined,
  image: ImageRef | undefined,
  options: { enabled?: boolean } = {},
): SessionImageResult => {
  const enabled =
    Boolean(providerId && sourcePath && image) && options.enabled !== false;
  const key = image ? `${providerId}|${sourcePath}|${stableKey(image)}` : "";
  const [state, setState] = useState<SessionImageResult>({
    url: null,
    isLoading: false,
    error: null,
  });

  useEffect(() => {
    if (!enabled) return;
    const cached = imageCache.get(key);
    if (cached) {
      setState({ url: cached, isLoading: false, error: null });
      return;
    }
    let cancelled = false;
    setState({ url: null, isLoading: true, error: null });
    loadImageDataUrl(key, providerId!, sourcePath!, image!).then(
      (url) => {
        if (!cancelled) {
          setState({ url: url || null, isLoading: false, error: null });
        }
      },
      (error: unknown) => {
        if (!cancelled) {
          setState({ url: null, isLoading: false, error });
        }
      },
    );
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabled, key]);

  return enabled ? state : { url: null, isLoading: false, error: null };
};

export const useDeleteSessionMutation = () => {
  const queryClient = useQueryClient();
  const { t } = useTranslation();

  return useMutation({
    mutationFn: async (
      input: DeleteSessionOptions,
    ): Promise<DeleteSessionReply> => {
      const reply = await sessionsApi.delete(input);
      if (reply.status === "deleted") {
        queryClient.setQueryData<SessionMeta[]>(["sessions"], (current) =>
          (current ?? []).filter(
            (session) =>
              !(
                session.providerId === input.providerId &&
                session.sessionId === input.sessionId &&
                session.sourcePath === input.sourcePath
              ),
          ),
        );
        toast.success(
          t("sessionManager.sessionDeleted", { defaultValue: "会话已删除" }),
        );
        reply.warnings?.forEach((warning) => toast.warning(warning));
      } else if (reply.status === "cleanup_pending") {
        toast.warning(
          t("sessionManager.cleanupPendingToast", {
            defaultValue: "清理尚未完成，可在列表中继续清理",
          }),
          {
            description: reply.warnings.join("；"),
            duration: 12_000,
          },
        );
      } else if (reply.status === "not_found") {
        toast.info(
          t("sessionManager.sessionNotFound", {
            defaultValue: "会话记录已不存在，无需删除",
          }),
        );
      }
      return reply;
    },
    onError: (error: Error) => {
      toast.error(
        t("sessionManager.deleteFailed", {
          defaultValue: "删除会话失败: {{error}}",
          error: extractErrorMessage(error) || t("common.unknown"),
        }),
      );
    },
    onSettled: async (_reply, _error, input) => {
      await queryClient.cancelQueries({
        queryKey: ["sessionMessages", input.providerId, input.sourcePath],
      });
      queryClient.removeQueries({
        queryKey: ["sessionMessages", input.providerId, input.sourcePath],
      });
      await queryClient.invalidateQueries({ queryKey: ["sessions"] });
    },
  });
};
