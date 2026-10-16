import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { AlertTriangle, Info } from "lucide-react";
import { useTranslation } from "react-i18next";

export interface ConfirmDialogCheckbox {
  /** 稳定标识；`onConfirm` 按它回传勾选状态 */
  id: string;
  label: string;
  /** 受控勾选状态：状态由调用方持有，对话框只负责渲染与回传 */
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  /**
   * 必须勾选该复选框才能确认。
   *
   * 用于「删除范围大于能逐条列出的数量」这类情况：清单列不全时，确认按钮保持禁用，
   * 用户必须明确确认「已核对」才能继续。这是知情确认，不是禁止——禁止会让一批会话
   * 变得无法删除。
   */
  required?: boolean;
  /** 勾选后显示的警示说明 */
  warning?: string;
}

interface ConfirmDialogProps {
  isOpen: boolean;
  title: string;
  message: string;
  confirmText?: string;
  cancelText?: string;
  variant?: "destructive" | "info";
  zIndex?: "base" | "nested" | "alert" | "top";
  /** 可选的勾选项列表；勾选状态由调用方持有 */
  checkboxes?: ConfirmDialogCheckbox[];
  /**
   * 需要逐条核对的目标清单。用于「全选筛选结果」这类选择范围大于当前可见行的
   * 破坏性操作：只给总数会让用户无法核对到底删了哪些。
   */
  details?: { title: string; meta?: string }[];
  /** details 被截断时显示的补充说明（例如「另有 N 条未列出」） */
  detailsOverflowLabel?: string;
  pending?: boolean;
  /** 回传每个勾选项的最终状态（按 id） */
  onConfirm: (checked: Record<string, boolean>) => void;
  onCancel: () => void;
}

export function ConfirmDialog({
  isOpen,
  title,
  message,
  confirmText,
  cancelText,
  variant = "destructive",
  zIndex = "alert",
  checkboxes,
  details,
  detailsOverflowLabel,
  pending = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  const IconComponent = variant === "info" ? Info : AlertTriangle;
  const iconClass =
    variant === "info" ? "h-5 w-5 text-blue-500" : "h-5 w-5 text-destructive";
  const checkboxItems = checkboxes ?? [];
  const confirmBlocked = checkboxItems.some(
    (item) => item.required && !item.checked,
  );

  return (
    <Dialog
      open={isOpen}
      onOpenChange={(open) => {
        if (!open && !pending) {
          onCancel();
        }
      }}
    >
      <DialogContent
        data-testid="confirm-dialog"
        className="max-w-sm"
        zIndex={zIndex}
      >
        <DialogHeader className="space-y-3 border-b-0 bg-transparent pb-0">
          <DialogTitle className="flex items-center gap-2 text-lg font-semibold">
            <IconComponent className={iconClass} />
            {title}
          </DialogTitle>
          <DialogDescription className="whitespace-pre-line text-sm leading-relaxed">
            {message}
          </DialogDescription>
        </DialogHeader>
        {details && details.length > 0 ? (
          <div className="px-6 pt-3" data-testid="confirm-dialog-details">
            <ul className="asm-confirm-details">
              {details.map((item, index) => (
                <li key={`${item.title}\u0000${item.meta ?? ""}\u0000${index}`}>
                  <span className="asm-confirm-details-title">
                    {item.title}
                  </span>
                  {item.meta ? (
                    <span className="asm-confirm-details-meta">
                      {item.meta}
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
            {detailsOverflowLabel ? (
              <p className="mt-2 text-xs text-muted-foreground">
                {detailsOverflowLabel}
              </p>
            ) : null}
          </div>
        ) : null}
        {checkboxItems.length > 0 ? (
          <div className="space-y-3 px-6 pt-3">
            {checkboxItems.map((item) => (
              <div key={item.id}>
                <label className="flex cursor-pointer select-none items-start gap-2">
                  <Checkbox
                    checked={item.checked}
                    disabled={pending}
                    aria-label={item.label}
                    onCheckedChange={(value) =>
                      item.onCheckedChange(value === true)
                    }
                    className="mt-0.5"
                  />
                  <span className="text-sm leading-relaxed">{item.label}</span>
                </label>
                {item.checked && item.warning ? (
                  <p className="mt-2 rounded-md bg-destructive/10 px-3 py-2 text-xs leading-relaxed text-destructive">
                    {item.warning}
                  </p>
                ) : null}
              </div>
            ))}
          </div>
        ) : null}
        <DialogFooter className="flex gap-2 border-t-0 bg-transparent pt-2 sm:justify-end">
          <Button variant="outline" onClick={onCancel} disabled={pending}>
            {cancelText || t("common.cancel")}
          </Button>
          <Button
            variant={variant === "info" ? "default" : "destructive"}
            disabled={pending || confirmBlocked}
            onClick={() =>
              onConfirm(
                Object.fromEntries(
                  checkboxItems.map((item) => [item.id, item.checked]),
                ),
              )
            }
          >
            {confirmText || t("common.confirm")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
