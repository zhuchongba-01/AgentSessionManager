import React, { useCallback, useEffect, useRef, useState } from "react";
import { cn } from "@/lib/utils";

interface OverlayScrollAreaProps {
  children: React.ReactNode;
  className?: string;
  wrapperClassName?: string;
}

const MIN_THUMB_HEIGHT = 28;
const TRACK_INSET_Y = 4;

/**
 * Antigravity 风格悬浮胶囊滚动容器：
 * 隐藏原生占位滚动条以保持内容左右对称，仅在悬停或滚动时浮现细胶囊滑块并自动淡出。
 */
export function OverlayScrollArea({
  children,
  className,
  wrapperClassName,
}: OverlayScrollAreaProps) {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  const trackRef = useRef<HTMLDivElement | null>(null);
  const hideTimerRef = useRef<number | null>(null);
  const dragCleanupRef = useRef<(() => void) | null>(null);
  const dragStateRef = useRef<{
    startY: number;
    startScrollTop: number;
    scrollableRange: number;
    trackTravel: number;
  } | null>(null);

  const [thumb, setThumb] = useState({ visible: false, height: 0, top: 0 });
  const [hovered, setHovered] = useState(false);
  const [scrolling, setScrolling] = useState(false);
  const [dragging, setDragging] = useState(false);

  const clearHideTimer = useCallback(() => {
    if (hideTimerRef.current !== null) {
      window.clearTimeout(hideTimerRef.current);
      hideTimerRef.current = null;
    }
  }, []);

  const syncMetrics = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport) return;
    const { clientHeight, scrollHeight, scrollTop } = viewport;
    if (scrollHeight <= clientHeight + 1 || clientHeight <= TRACK_INSET_Y * 2) {
      setThumb((prev) =>
        prev.visible ? { visible: false, height: 0, top: 0 } : prev,
      );
      return;
    }

    const trackHeight = Math.max(0, clientHeight - TRACK_INSET_Y * 2);
    const ratio = clientHeight / scrollHeight;
    const thumbHeight = Math.min(
      trackHeight,
      Math.max(MIN_THUMB_HEIGHT, Math.round(trackHeight * ratio)),
    );
    const maxScroll = Math.max(1, scrollHeight - clientHeight);
    const maxTop = Math.max(0, trackHeight - thumbHeight);
    const thumbTop = Math.round((scrollTop / maxScroll) * maxTop);

    setThumb({ visible: true, height: thumbHeight, top: thumbTop });
  }, []);

  useEffect(() => {
    syncMetrics();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      syncMetrics();
    });
    if (viewportRef.current) observer.observe(viewportRef.current);
    if (contentRef.current) observer.observe(contentRef.current);
    return () => observer.disconnect();
  }, [syncMetrics]);

  useEffect(
    () => () => {
      clearHideTimer();
      dragCleanupRef.current?.();
    },
    [clearHideTimer],
  );

  const handleScroll = useCallback(() => {
    syncMetrics();
    setScrolling(true);
    clearHideTimer();
    hideTimerRef.current = window.setTimeout(() => {
      setScrolling(false);
      hideTimerRef.current = null;
    }, 850);
  }, [clearHideTimer, syncMetrics]);

  const handleThumbPointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const viewport = viewportRef.current;
      const track = trackRef.current;
      if (!viewport || !track) return;
      event.preventDefault();
      event.stopPropagation();

      const trackHeight = track.clientHeight;
      const scrollableRange = viewport.scrollHeight - viewport.clientHeight;
      const trackTravel = Math.max(1, trackHeight - thumb.height);
      if (scrollableRange <= 0) return;

      dragStateRef.current = {
        startY: event.clientY,
        startScrollTop: viewport.scrollTop,
        scrollableRange,
        trackTravel,
      };
      setDragging(true);

      const handlePointerMove = (moveEvent: PointerEvent) => {
        const drag = dragStateRef.current;
        const vp = viewportRef.current;
        if (!drag || !vp) return;
        const deltaY = moveEvent.clientY - drag.startY;
        const nextScrollTop =
          drag.startScrollTop +
          (deltaY / drag.trackTravel) * drag.scrollableRange;
        vp.scrollTop = Math.max(
          0,
          Math.min(drag.scrollableRange, nextScrollTop),
        );
      };

      const cleanupDrag = () => {
        dragStateRef.current = null;
        dragCleanupRef.current = null;
        window.removeEventListener("pointermove", handlePointerMove);
        window.removeEventListener("pointerup", handlePointerUp);
        window.removeEventListener("pointercancel", handlePointerUp);
      };

      const handlePointerUp = () => {
        cleanupDrag();
        setDragging(false);
      };

      dragCleanupRef.current?.();
      dragCleanupRef.current = cleanupDrag;
      window.addEventListener("pointermove", handlePointerMove);
      window.addEventListener("pointerup", handlePointerUp);
      window.addEventListener("pointercancel", handlePointerUp);
    },
    [thumb.height],
  );

  const handleTrackPointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const viewport = viewportRef.current;
      const track = trackRef.current;
      if (!viewport || !track || event.target !== track) return;
      const rect = track.getBoundingClientRect();
      const clickY = event.clientY - rect.top - thumb.height / 2;
      const trackTravel = Math.max(1, rect.height - thumb.height);
      const ratio = Math.max(0, Math.min(1, clickY / trackTravel));
      viewport.scrollTop =
        ratio * (viewport.scrollHeight - viewport.clientHeight);
    },
    [thumb.height],
  );

  const isShown = thumb.visible && (hovered || scrolling || dragging);

  return (
    <div
      className={cn("asm-overlay-scroll-wrap", wrapperClassName)}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
    >
      <div
        ref={viewportRef}
        className={cn("asm-overlay-scroll-viewport", className)}
        onScroll={handleScroll}
      >
        <div ref={contentRef} className="asm-overlay-scroll-content">
          {children}
        </div>
      </div>

      {thumb.visible && (
        <div
          ref={trackRef}
          className={cn(
            "asm-overlay-scrollbar",
            isShown && "is-visible",
            dragging && "is-dragging",
          )}
          aria-hidden="true"
          onPointerDown={handleTrackPointerDown}
        >
          <div
            className="asm-overlay-scrollbar-thumb"
            style={{
              height: `${thumb.height}px`,
              transform: `translate3d(0, ${thumb.top}px, 0)`,
            }}
            onPointerDown={handleThumbPointerDown}
          />
        </div>
      )}
    </div>
  );
}
