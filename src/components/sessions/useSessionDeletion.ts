import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import type { ConfirmDialogCheckbox } from "@/components/ConfirmDialog";
import { sessionsApi } from "@/lib/api/sessions";
import { useDeleteSessionMutation } from "@/lib/query/sessions";
import type { SessionMeta } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import {
  collectSessionDescendants,
  formatSessionTitle,
  getProviderLabel,
  getSessionKey,
  resolveParentLabel,
} from "./utils";

/**
 * 确认框里逐条列出的目标数上限。
 *
 * 「全选筛选结果（含其他分页）」的选择范围可以远大于屏幕上渲染的行数（列表每页 100
 * 条），而删除不可恢复。列出前 N 条用于核对；超出的部分不假装已核对，而是要求用户在
 * 对话框里明确确认「已核对」才能继续——是知情确认，不是禁止。
 */
const CONFIRM_DETAIL_LIMIT = 50;

interface UseSessionDeletionOptions {
  sessions: SessionMeta[];
  sessionTitles: Map<string, string>;
  setSelectedSessionKeys: Dispatch<SetStateAction<Set<string>>>;
}

export function useSessionDeletion({
  sessions,
  sessionTitles,
  setSelectedSessionKeys,
}: UseSessionDeletionOptions) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const deleteSessionMutation = useDeleteSessionMutation();

  const [deleteTargets, setDeleteTargets] = useState<SessionMeta[] | null>(
    null,
  );
  // 删除确认框里的两个勾选项状态。勾选状态由页面持有（ConfirmDialog 受控），
  // 这样每次打开删除框时都能明确地重置，而不是靠副作用猜测。
  const [deleteWithChildren, setDeleteWithChildren] = useState(true);
  const [acknowledgedDeleteScope, setAcknowledgedDeleteScope] = useState<
    string | null
  >(null);
  const [isBatchDeleting, setIsBatchDeleting] = useState(false);

  const isDeleting = deleteSessionMutation.isPending || isBatchDeleting;

  /**
   * 待删会话的全部子会话（子 Agent 会话及其后继）。
   *
   * 从**完整扫描结果**里算，不用过滤后的列表：搜索/筛选把某条子会话藏起来不代表它
   * 不存在，漏掉它只会让父会话的预检失败（后端会拒绝，不会真的留下孤儿）。
   */
  const deleteDescendants = useMemo(
    () => collectSessionDescendants(sessions, deleteTargets ?? []),
    [sessions, deleteTargets],
  );

  /** 本次确认框实际会删除的范围：勾选「同时删除子会话」时含全部后代。 */
  const plannedDeleteTargets = useMemo(
    () =>
      deleteWithChildren
        ? [...(deleteTargets ?? []), ...deleteDescendants]
        : (deleteTargets ?? []),
    [deleteTargets, deleteDescendants, deleteWithChildren],
  );

  // 核对绑定精确目标集合；级联范围或扫描结果改变时旧核对立即失效。
  const deleteScope = useMemo(
    () => JSON.stringify(plannedDeleteTargets.map(getSessionKey).sort()),
    [plannedDeleteTargets],
  );
  const deleteAcknowledged = acknowledgedDeleteScope === deleteScope;

  useEffect(() => {
    if (
      acknowledgedDeleteScope !== null &&
      acknowledgedDeleteScope !== deleteScope
    ) {
      setAcknowledgedDeleteScope(null);
    }
  }, [acknowledgedDeleteScope, deleteScope]);

  /**
   * 打开删除确认框。
   *
   * 默认勾选「同时删除子会话」：子会话在列表里往往只显示为「残留」，用户既认不出
   * 归属、也没法先逐个删干净，逐条手删正是要修掉的麻烦。范围照旧在确认框里逐条列出，
   * 用户随时可以取消勾选只删父会话。
   */
  const openDeleteDialog = useCallback((targets: SessionMeta[]) => {
    if (targets.length === 0) return;
    setDeleteWithChildren(true);
    setAcknowledgedDeleteScope(null);
    setDeleteTargets(targets);
  }, []);

  const closeDeleteDialog = useCallback(() => {
    if (!isDeleting) {
      setDeleteTargets(null);
    }
  }, [isDeleting]);

  /**
   * 确认框里逐条列出待删会话。数量超过上限时前半段逐条列出、其余只报数量并
   * 明确标注「未列出」，避免用一份不完整的清单冒充已核对。
   *
   * 清单列的是**实际会删除的范围**（含勾选后一并删除的子会话），每条子会话都标出
   * 它属于哪条父会话——这正是列表里「残留」标签看不出、用户最需要核对的信息。
   */
  const deleteConfirmDetails = useMemo(() => {
    if (plannedDeleteTargets.length <= 1) return null;
    const unlisted = Math.max(
      0,
      plannedDeleteTargets.length - CONFIRM_DETAIL_LIMIT,
    );
    return {
      items: plannedDeleteTargets
        .slice(0, CONFIRM_DETAIL_LIMIT)
        .map((session) => {
          const parent = resolveParentLabel(session, sessionTitles);
          return {
            title: formatSessionTitle(session),
            meta: [
              getProviderLabel(session.providerId, t),
              parent
                ? t("sessionManager.batchDeleteChildMeta", {
                    defaultValue: "子会话 · 属于「{{parent}}」",
                    parent,
                  })
                : session.sessionId,
            ].join(" · "),
          };
        }),
      unlisted,
      overflowLabel:
        unlisted > 0
          ? t("sessionManager.batchDeleteDetailsOverflow", {
              defaultValue:
                "上面 {{listed}} 条已逐条列出；另有 {{count}} 条未列出，但同样在本次删除范围内。",
              listed: CONFIRM_DETAIL_LIMIT,
              count: unlisted,
            })
          : undefined,
    };
  }, [plannedDeleteTargets, sessionTitles, t]);

  /** 勾选后一并删除的子会话里，有源路径可直接删除的数量。 */
  const deletableDescendantCount = useMemo(
    () => deleteDescendants.filter((session) => session.sourcePath).length,
    [deleteDescendants],
  );

  /**
   * 删除确认框里的勾选项。
   *
   * - 「同时删除子会话」：本次要修的核心。默认勾选，取消勾选时后端预检会拒绝删除
   *   仍被依赖的父会话，不会留下孤儿。
   * - 「已核对未列出的条目」：只有当清单列不全时才出现，且必须勾选才能确认。
   */
  const deleteDialogCheckboxes = useMemo<ConfirmDialogCheckbox[]>(() => {
    const items: ConfirmDialogCheckbox[] = [];
    if (deleteDescendants.length > 0) {
      const blocked = deleteDescendants.length - deletableDescendantCount;
      items.push({
        id: "delete-with-children",
        checked: deleteWithChildren,
        onCheckedChange: setDeleteWithChildren,
        label:
          deleteTargets && deleteTargets.length > 1
            ? t("sessionManager.cascadeDeleteLabelMulti", {
                defaultValue:
                  "同时删除这 {{roots}} 条会话下的 {{count}} 个子会话",
                roots: deleteTargets.length,
                count: deleteDescendants.length,
              })
            : t("sessionManager.cascadeDeleteLabel", {
                defaultValue: "同时删除「{{title}}」的 {{count}} 个子会话",
                title: deleteTargets?.[0]
                  ? formatSessionTitle(deleteTargets[0])
                  : "",
                count: deleteDescendants.length,
              }),
        warning: [
          t("sessionManager.cascadeDeleteWarning", {
            defaultValue:
              "子会话不与父会话一起删除就没法单独清理：它们不会进入回收站，文件与索引记录都会被永久删除。",
          }),
          blocked > 0
            ? t("sessionManager.cascadeDeleteBlocked", {
                defaultValue:
                  "另有 {{count}} 个子会话没有可用的源路径，管理器无法删除；父会话的删除会因此被拒绝，请重新扫描后再试。",
                count: blocked,
              })
            : "",
        ]
          .filter(Boolean)
          .join("\n"),
      });
    }
    if (deleteConfirmDetails && deleteConfirmDetails.unlisted > 0) {
      items.push({
        id: "acknowledge-unlisted",
        checked: deleteAcknowledged,
        onCheckedChange: (checked) =>
          setAcknowledgedDeleteScope(checked ? deleteScope : null),
        required: true,
        label: t("sessionManager.batchDeleteAcknowledge", {
          defaultValue:
            "我已核对将删除的 {{count}} 条会话（含未列出的 {{unlisted}} 条）",
          count: plannedDeleteTargets.length,
          unlisted: deleteConfirmDetails.unlisted,
        }),
        warning: t("sessionManager.batchDeleteAcknowledgeWarning", {
          defaultValue:
            "这些会话不会进入回收站：文件与数据库记录都会被永久删除，且无法撤销。",
        }),
      });
    }
    return items;
  }, [
    deleteAcknowledged,
    deleteScope,
    deleteConfirmDetails,
    deleteDescendants,
    deleteTargets,
    deleteWithChildren,
    deletableDescendantCount,
    plannedDeleteTargets.length,
    t,
  ]);

  const handleDeleteConfirm = useCallback(async () => {
    if (
      !deleteTargets ||
      deleteTargets.length === 0 ||
      isDeleting ||
      (plannedDeleteTargets.length > CONFIRM_DETAIL_LIMIT &&
        !deleteAcknowledged)
    ) {
      return;
    }

    const targets = plannedDeleteTargets.filter((session) =>
      Boolean(session.sourcePath),
    );
    if (targets.length === 0) {
      setDeleteTargets(null);
      return;
    }

    if (targets.length === 1) {
      const [target] = targets;
      try {
        await deleteSessionMutation.mutateAsync({
          providerId: target.providerId,
          sessionId: target.sessionId,
          sourcePath: target.sourcePath!,
          includeProject: false,
        });
        setDeleteTargets(null);
        setSelectedSessionKeys((current) => {
          const next = new Set(current);
          next.delete(getSessionKey(target));
          return next;
        });
      } catch (error) {
        // mutation 的 onError 已提示原因；但对话框不能就此消失——否则用户
        // 既看不到失败的是哪一条，也没有重试入口。保留对话框与选择状态。
        toast.error(
          t("sessionManager.deleteFailedRetry", {
            defaultValue:
              "删除失败，会话仍在列表中。请先退出对应 Agent 后重试。",
            error: extractErrorMessage(error),
          }),
        );
      }
      return;
    }

    setIsBatchDeleting(true);
    try {
      const results = await sessionsApi.deleteMany(
        targets.map((session) => ({
          providerId: session.providerId,
          sessionId: session.sessionId,
          sourcePath: session.sourcePath!,
        })),
      );

      const deletedKeys = results
        .filter((result) => result.success)
        .map(
          (result) =>
            `${result.providerId}:${result.sessionId}:${result.sourcePath ?? ""}`,
        );
      const deletedKeySet = new Set(deletedKeys);

      const failedResults = results.filter((result) => !result.success);
      const failedErrors = failedResults.map((result) =>
        [
          `${formatSessionTitle(result)}（${getProviderLabel(result.providerId, t)}）`,
          result.error || t("common.unknown"),
          ...(result.warnings ?? []),
        ].join("："),
      );

      const deleteWarnings = results
        .filter((result) => result.success)
        .flatMap((result) => result.warnings ?? []);

      if (deletedKeys.length > 0) {
        queryClient.setQueryData<SessionMeta[]>(["sessions"], (current) =>
          (current ?? []).filter(
            (session) => !deletedKeySet.has(getSessionKey(session)),
          ),
        );
      }

      results
        .filter((result) => result.success)
        .forEach((result) => {
          queryClient.removeQueries({
            queryKey: ["sessionMessages", result.providerId, result.sourcePath],
          });
        });

      setSelectedSessionKeys((current) => {
        const next = new Set(current);
        deletedKeys.forEach((key) => next.delete(key));
        return next;
      });

      if (deletedKeys.length > 0) {
        toast.success(
          t("sessionManager.batchDeleteSuccess", {
            defaultValue: "已删除 {{count}} 个会话",
            count: deletedKeys.length,
          }),
        );
      }

      const uniqueWarnings = Array.from(new Set(deleteWarnings));
      if (uniqueWarnings.length > 0) {
        toast.warning(uniqueWarnings.join("；"));
      }

      if (failedResults.length === 0) {
        setDeleteTargets(null);
      } else {
        const failedKeySet = new Set(
          failedResults.map(
            (result) =>
              `${result.providerId}:${result.sessionId}:${result.sourcePath ?? ""}`,
          ),
        );
        const remainingSessions = sessions.filter(
          (session) => !deletedKeySet.has(getSessionKey(session)),
        );
        const remainingRoots = deleteTargets.filter(
          (session) => !deletedKeySet.has(getSessionKey(session)),
        );
        const coveredByRemainingRoots = new Set(
          [
            ...remainingRoots,
            ...(deleteWithChildren
              ? collectSessionDescendants(remainingSessions, remainingRoots)
              : []),
          ].map(getSessionKey),
        );
        const extraFailedTargets = targets.filter(
          (session) =>
            failedKeySet.has(getSessionKey(session)) &&
            !coveredByRemainingRoots.has(getSessionKey(session)),
        );
        const nextTargets = [...remainingRoots, ...extraFailedTargets];
        setDeleteTargets(
          nextTargets.length > 0
            ? nextTargets
            : targets.filter((session) =>
                failedKeySet.has(getSessionKey(session)),
              ),
        );

        // 逐条列出失败对象，而不是只报数量 + 第一条：混合结果下用户必须能
        // 看出哪几条没删掉、为什么。
        toast.error(
          t("sessionManager.batchDeleteFailed", {
            defaultValue: "{{failed}} 个会话删除失败",
            failed: failedErrors.length,
          }),
          {
            description: failedErrors.slice(0, 5).join("\n"),
            duration: 12_000,
          },
        );
      }
    } catch (error) {
      toast.error(
        extractErrorMessage(error) ||
          t("sessionManager.batchDeleteRequestFailed", {
            defaultValue: "批量删除失败，请稍后重试",
          }),
      );
    } finally {
      await queryClient.invalidateQueries({ queryKey: ["sessions"] });
      await queryClient.invalidateQueries({ queryKey: ["sessionMessages"] });
      setIsBatchDeleting(false);
    }
  }, [
    deleteAcknowledged,
    deleteSessionMutation,
    deleteTargets,
    deleteWithChildren,
    isDeleting,
    plannedDeleteTargets,
    queryClient,
    sessions,
    setSelectedSessionKeys,
    t,
  ]);

  const dialogTitle =
    deleteTargets && deleteTargets.length > 1
      ? t("sessionManager.batchDeleteConfirmTitle", {
          defaultValue: "批量删除会话",
        })
      : t("sessionManager.deleteConfirmTitle", {
          defaultValue: "删除会话",
        });

  const dialogMessage =
    deleteTargets && deleteTargets.length > 1
      ? deleteWithChildren && deleteDescendants.length > 0
        ? t("sessionManager.batchDeleteWithChildrenMessage", {
            defaultValue:
              "将永久删除 {{count}} 个本地会话记录，其中 {{children}} 条是随父会话一并删除的子会话。\n\n不会删除项目目录。\n\n请先退出对应 Agent 并备份重要会话。此操作不可恢复。",
            count: plannedDeleteTargets.length,
            children: deleteDescendants.length,
          })
        : t("sessionManager.batchDeleteConfirmMessage", {
            defaultValue:
              "将永久删除已选中的 {{count}} 个本地会话记录。\n\n当前会话包含同 ID 历史副本与关联索引；单独选择历史副本仅删除该副本。不会删除项目目录。\n\n请先退出对应 Agent 并备份重要会话。此操作不可恢复。",
            count: plannedDeleteTargets.length,
          })
      : deleteTargets?.[0]
        ? plannedDeleteTargets.length > 1
          ? t("sessionManager.deleteConfirmWithChildrenMessage", {
              defaultValue:
                "将永久删除本地会话“{{title}}”及其 {{count}} 个子会话（子会话先于父会话删除）。\n\n不会删除项目目录。\n\n请先退出对应 Agent 并备份重要会话。此操作不可恢复。",
              title: formatSessionTitle(deleteTargets[0]),
              count: plannedDeleteTargets.length - 1,
            })
          : t("sessionManager.deleteConfirmMessage", {
              defaultValue:
                "将永久删除本地会话“{{title}}”\nSession ID: {{sessionId}}\n\n当前会话包含同 ID 历史副本与关联索引；单独选择历史副本仅删除该副本。不会删除项目目录。\n\n请先退出对应 Agent 并备份重要会话。此操作不可恢复。",
              title: formatSessionTitle(deleteTargets[0]),
              sessionId: deleteTargets[0].sessionId,
            })
        : "";

  const dialogConfirmText =
    deleteTargets && deleteTargets.length > 1
      ? t("sessionManager.batchDeleteConfirmAction", {
          defaultValue: "删除所选会话",
        })
      : plannedDeleteTargets.length > 1
        ? t("sessionManager.deleteWithChildrenAction", {
            defaultValue: "删除会话及其子会话",
          })
        : t("sessionManager.deleteConfirmAction", {
            defaultValue: "删除会话",
          });

  return {
    isDialogOpen: Boolean(deleteTargets),
    isDeleting,
    dialogTitle,
    dialogMessage,
    dialogConfirmText,
    deleteConfirmDetails,
    deleteDialogCheckboxes,
    openDeleteDialog,
    closeDeleteDialog,
    handleDeleteConfirm,
  };
}
