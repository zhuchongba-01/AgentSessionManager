import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ChevronLeft, ChevronRight } from "lucide-react";
import type { SessionMeta } from "@/types";
import { getSessionKey } from "./utils";

const PAGE_SIZE = 100;

/** Bound DOM size even when a provider has years of session history. */
export function SessionRows({
  sessions,
  selectedKey,
  resetKey,
  renderItem,
}: {
  sessions: SessionMeta[];
  selectedKey?: string | null;
  resetKey?: string;
  renderItem: (session: SessionMeta) => ReactNode;
}) {
  const { t } = useTranslation();
  const [page, setPage] = useState(0);
  const [prevResetKey, setPrevResetKey] = useState(resetKey);
  const [prevSelectedKey, setPrevSelectedKey] = useState(selectedKey);

  if (resetKey !== prevResetKey) {
    setPrevResetKey(resetKey);
    if (page !== 0) {
      setPage(0);
    }
  }

  if (selectedKey !== prevSelectedKey) {
    setPrevSelectedKey(selectedKey);
    if (selectedKey) {
      const selectedIndex = sessions.findIndex(
        (session) => getSessionKey(session) === selectedKey,
      );
      if (selectedIndex >= 0) {
        const targetPage = Math.floor(selectedIndex / PAGE_SIZE);
        if (page !== targetPage) {
          setPage(targetPage);
        }
      }
    }
  }

  const lastPage = Math.max(0, Math.ceil(sessions.length / PAGE_SIZE) - 1);
  const visiblePage = Math.min(page, lastPage);
  const prevLabel = t("sessionManager.prevPage", { defaultValue: "上一页" });
  const nextLabel = t("sessionManager.nextPage", { defaultValue: "下一页" });

  return (
    <>
      {sessions
        .slice(visiblePage * PAGE_SIZE, (visiblePage + 1) * PAGE_SIZE)
        .map(renderItem)}
      {lastPage > 0 && (
        <nav
          aria-label={t("sessionManager.paginationNav", {
            defaultValue: "会话分页",
          })}
          className="asm-pagination-bar"
        >
          <button
            type="button"
            className="asm-pagination-btn"
            aria-label={prevLabel}
            disabled={visiblePage === 0}
            onClick={() => setPage(visiblePage - 1)}
          >
            <ChevronLeft className="size-3.5" aria-hidden="true" />
            <span>{prevLabel}</span>
          </button>
          <span className="asm-pagination-meta">
            {t("sessionManager.paginationStatus", {
              defaultValue: "{{current}} / {{total}} 页 · {{count}} 条",
              current: visiblePage + 1,
              total: lastPage + 1,
              count: sessions.length,
            })}
          </span>
          <button
            type="button"
            className="asm-pagination-btn"
            aria-label={nextLabel}
            disabled={visiblePage === lastPage}
            onClick={() => setPage(visiblePage + 1)}
          >
            <span>{nextLabel}</span>
            <ChevronRight className="size-3.5" aria-hidden="true" />
          </button>
        </nav>
      )}
    </>
  );
}
