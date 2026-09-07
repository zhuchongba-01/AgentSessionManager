import { useEffect, useState, type ReactNode } from "react";
import type { SessionMeta } from "@/types";
import { Button } from "@/components/ui/button";

const PAGE_SIZE = 100;

/** Bound DOM size even when a provider has years of session history. */
export function SessionRows({
  sessions,
  renderItem,
}: {
  sessions: SessionMeta[];
  renderItem: (session: SessionMeta) => ReactNode;
}) {
  const [page, setPage] = useState(0);
  const lastPage = Math.max(0, Math.ceil(sessions.length / PAGE_SIZE) - 1);
  const visiblePage = Math.min(page, lastPage);
  useEffect(() => setPage(0), [sessions]);
  return (
    <>
      {sessions
        .slice(visiblePage * PAGE_SIZE, (visiblePage + 1) * PAGE_SIZE)
        .map(renderItem)}
      {lastPage > 0 && (
        <nav
          aria-label="会话分页"
          className="flex items-center justify-between gap-2 p-2 text-xs"
        >
          <Button
            size="sm"
            variant="outline"
            disabled={visiblePage === 0}
            onClick={() => setPage(visiblePage - 1)}
          >
            上一页
          </Button>
          <span>
            {visiblePage + 1} / {lastPage + 1} 页 · {sessions.length} 条
          </span>
          <Button
            size="sm"
            variant="outline"
            disabled={visiblePage === lastPage}
            onClick={() => setPage(visiblePage + 1)}
          >
            下一页
          </Button>
        </nav>
      )}
    </>
  );
}
