import { useCallback, useEffect, useState } from "react";
import { Gift, Loader2, RefreshCw, Sparkles } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import type { AccountMeta } from "@/lib/types";

interface Props {
  account: AccountMeta | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function GrowthWelfareDialog({ account, open, onOpenChange }: Props) {
  const [tasks, setTasks] = useState<api.GrowthTask[]>([]);
  const [taskMessage, setTaskMessage] = useState("");
  const [welfareMessages, setWelfareMessages] = useState<string[]>([]);
  const [busy, setBusy] = useState<"tasks" | "claim" | "welfare" | null>(null);

  const refreshTasks = useCallback(async () => {
    if (!account) return;
    setBusy("tasks");
    setTaskMessage("");
    try {
      const result = await api.getGrowthTasks(account.id);
      setTasks(result.tasks ?? []);
      setTaskMessage(result.message ?? (result.ok ? "任务进度已更新" : "任务读取失败"));
    } catch (error) {
      setTaskMessage(api.asError(error));
    } finally {
      setBusy(null);
    }
  }, [account]);

  useEffect(() => {
    if (!open || !account) return;
    setTasks([]);
    setWelfareMessages([]);
    void refreshTasks();
  }, [open, account, refreshTasks]);

  async function claimReady() {
    if (!account) return;
    setBusy("claim");
    setTaskMessage("");
    try {
      const result = await api.claimGrowthTasks(account.id);
      const claimed = result.results.filter((item) => item.ok && !item.already_claimed).length;
      setTaskMessage(
        result.message ??
          (result.unsupported
            ? "该账号暂不支持此成长任务列表"
            : claimed > 0
              ? `已领取 ${claimed} 项奖励；积分 +${result.credit}，能量 +${result.energy}`
              : "没有已达成且待领取的任务"),
      );
      await refreshTasks();
    } catch (error) {
      setTaskMessage(api.asError(error));
    } finally {
      setBusy(null);
    }
  }

  async function runWelfare() {
    if (!account) return;
    setBusy("welfare");
    setWelfareMessages([]);
    try {
      const result = await api.runDailyWelfare(account.id);
      setWelfareMessages(result.results.map((item) => `${item.ok ? "完成" : "未领取"} · ${item.step}：${item.message}`));
    } catch (error) {
      setWelfareMessages([api.asError(error)]);
    } finally {
      setBusy(null);
    }
  }

  const loading = busy !== null;
  const claimableCount = tasks.filter((task) => task.claimable && !task.claimed).length;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Sparkles className="size-4 text-primary" />成长任务与每日福利
          </DialogTitle>
          <DialogDescription>
            {account ? `账号：${displayName(account)}` : "查看任务进度、领取已达成奖励，或执行真实福利领取。"}
          </DialogDescription>
        </DialogHeader>

        <div className="rounded-lg border bg-muted/30 px-3 py-2 text-xs leading-5 text-muted-foreground">
          仅领取服务端已确认达成的任务，不会代发对话、分享或浏览等行为事件。每日福利会使用该账号实际可用的领取资格。
        </div>

        <div className="flex flex-wrap gap-2">
          <Button size="sm" variant="outline" onClick={() => void refreshTasks()} disabled={!account || loading}>
            {busy === "tasks" ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            刷新任务
          </Button>
          <Button size="sm" variant="outline" onClick={() => void claimReady()} disabled={!account || loading || claimableCount === 0}>
            {busy === "claim" ? <Loader2 className="animate-spin" /> : <Gift />}
            领取已达成奖励{claimableCount > 0 ? ` (${claimableCount})` : ""}
          </Button>
          <Button size="sm" onClick={() => void runWelfare()} disabled={!account || loading}>
            {busy === "welfare" ? <Loader2 className="animate-spin" /> : <Gift />}
            执行每日福利
          </Button>
        </div>

        {taskMessage && <p role="status" className="text-xs text-muted-foreground">{taskMessage}</p>}
        <div className="max-h-64 space-y-2 overflow-y-auto rounded-lg border p-2">
          {tasks.length === 0 ? (
            <p className="p-3 text-center text-xs text-muted-foreground">尚无任务数据；点击“刷新任务”读取。</p>
          ) : tasks.map((task) => (
            <div key={task.task_code} className="flex items-center gap-3 rounded-md px-2 py-1.5">
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium">{task.title || task.task_code}</p>
                <p className="text-xs text-muted-foreground">
                  {task.target > 0 ? `${task.current}/${task.target}` : task.accept_status || "进度未知"}
                  {task.credit > 0 ? ` · ${task.credit} 积分` : ""}
                </p>
              </div>
              <Badge variant={task.claimed ? "success" : task.claimable ? "warning" : "secondary"}>
                {task.claimed ? "已领取" : task.claimable ? "可领取" : task.locked ? "未解锁" : "进行中"}
              </Badge>
            </div>
          ))}
        </div>

        {welfareMessages.length > 0 && (
          <div className="max-h-36 space-y-1 overflow-y-auto rounded-lg border p-3" role="status">
            {welfareMessages.map((message, index) => <p key={`${index}-${message}`} className="text-xs text-muted-foreground">{message}</p>)}
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
