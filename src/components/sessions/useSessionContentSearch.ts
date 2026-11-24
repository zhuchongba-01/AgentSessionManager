import { useCallback, useEffect, useMemo, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { sessionsApi } from "@/lib/api/sessions";
import { safeLocalStorageGet, safeLocalStorageSet } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";
import type { SessionContentHit, SessionIndexStatus } from "@/types";

const SEARCH_DEBOUNCE_MS = 220;
const CONTENT_SEARCH_STORAGE_KEY = "agent-session-manager.contentSearchEnabled";

export const sessionContentKeys = {
  all: ["sessionContentSearch"] as const,
  search: (query: string, providerId: string) =>
    ["sessionContentSearch", query, providerId] as const,
};

export const getContentHitKey = (
  providerId: string,
  sourcePath?: string | null,
) => `${providerId}:${sourcePath ?? ""}`;

interface UseSessionContentSearchOptions {
  query: string;
  providerFilter: string;
  listIsSuccess: boolean;
  listUpdatedAt: number;
}

export function useSessionContentSearch({
  query,
  providerFilter,
  listIsSuccess,
  listUpdatedAt,
}: UseSessionContentSearchOptions) {
  const queryClient = useQueryClient();
  const [enabled, setEnabledState] = useState<boolean>(
    () => safeLocalStorageGet(CONTENT_SEARCH_STORAGE_KEY) === "true",
  );
  const [status, setStatus] = useState<SessionIndexStatus | null>(null);
  const [debounced, setDebounced] = useState(() => query.trim());

  const setEnabled = useCallback(
    (next: boolean) => {
      setEnabledState(next);
      safeLocalStorageSet(CONTENT_SEARCH_STORAGE_KEY, next ? "true" : "false");
      if (!next) {
        setStatus(null);
        queryClient.removeQueries({ queryKey: sessionContentKeys.all });
        void sessionsApi.clearContentIndex().catch(() => undefined);
      }
    },
    [queryClient],
  );

  useEffect(() => {
    const next = query.trim();
    if (!next) {
      setDebounced("");
      return;
    }
    const timer = window.setTimeout(
      () => setDebounced(next),
      SEARCH_DEBOUNCE_MS,
    );
    return () => window.clearTimeout(timer);
  }, [query]);

  useEffect(() => {
    if (!enabled) return;
    let unlisten: UnlistenFn | undefined;
    let disposed = false;

    void (async () => {
      try {
        const off = await listen<SessionIndexStatus>(
          "session-index-status",
          (event) => {
            if (disposed) return;
            setStatus(event.payload);
            void queryClient.invalidateQueries({
              queryKey: sessionContentKeys.all,
            });
          },
        );
        if (disposed) {
          off();
        } else {
          unlisten = off;
        }
      } catch {
        // 非 Tauri 原生环境（如单元测试或浏览器预览）忽略事件监听失败
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [enabled, queryClient]);

  useEffect(() => {
    if (!enabled || !listIsSuccess || listUpdatedAt === 0) return;
    let cancelled = false;
    void sessionsApi
      .syncContentIndex()
      .then((next) => {
        if (!cancelled && next) setStatus(next);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [enabled, listIsSuccess, listUpdatedAt]);

  const trimmedQuery = enabled ? query.trim() : "";

  const search = useQuery({
    queryKey: sessionContentKeys.search(trimmedQuery, providerFilter),
    queryFn: ({ signal }) =>
      sessionsApi.searchContent(trimmedQuery, providerFilter, signal),
    enabled: enabled && trimmedQuery !== "" && debounced === trimmedQuery,
    staleTime: 0,
    retry: false,
  });

  const hits = useMemo(() => {
    const map = new Map<string, SessionContentHit>();
    if (!enabled || !debounced || debounced !== query.trim()) return map;
    search.data?.hits.forEach((hit) => {
      map.set(getContentHitKey(hit.providerId, hit.sourcePath), hit);
    });
    return map;
  }, [debounced, enabled, query, search.data]);

  const settled = enabled && debounced !== "" && debounced === query.trim();
  const error =
    settled && search.isError
      ? extractErrorMessage(search.error) || "正文搜索失败"
      : null;

  const retry = useCallback(() => {
    void search.refetch();
    void sessionsApi
      .syncContentIndex()
      .then((next) => {
        if (next) setStatus(next);
      })
      .catch(() => undefined);
  }, [search]);

  return {
    enabled,
    setEnabled,
    hits,
    status: enabled ? (status ?? search.data?.status ?? null) : null,
    isSearching: enabled && query.trim() !== "" && search.isFetching,
    error,
    retry,
  };
}
