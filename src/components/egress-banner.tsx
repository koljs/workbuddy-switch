import { useCallback, useEffect, useState } from "react";
import { Globe2, Loader2, RefreshCw, Route, ShieldAlert } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * 页面顶部的出口 IP 条。
 *
 * 上游风控把「同一出口 IP 短时间反复登录多个账号」当成异常特征，所以这里要同时回答
 * 两个问题：**现在从哪出去**，以及**这个出口上已经挂了多少个号**。后者才是要不要
 * 换 IP 的判断依据——单看 IP 本身没有任何可操作性。
 *
 * 「换 IP 向导」按钮把四步流程摊开，每步都写清「怎么算做完了」。换 IP 失败最典型的
 * 形态是「以为换了，其实运营商给回同一个 IP」，所以第 3 步强制走 `force` 重新探测，
 * 绕开 5 分钟缓存，避免拿旧值自我确认。
 */
export function EgressBanner() {
  const [snapshot, setSnapshot] = useState<api.EgressSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const [guideOpen, setGuideOpen] = useState(false);

  const load = useCallback(async () => {
    try {
      setSnapshot(await api.getEgressSnapshot());
    } catch {
      // 出口 IP 只是辅助信息，拿不到就不显示，不打扰用户。
      setSnapshot(null);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function detect(force: boolean) {
    if (loading) return;
    setLoading(true);
    try {
      const result = await api.detectEgress(force);
      if (api.isEgressRecord(result)) {
        toast.success(force ? "已重新检测出口 IP" : "已获取出口 IP", {
          description: result.location ? `${result.ip} · ${result.location}` : result.ip,
        });
      } else {
        toast.error("出口 IP 检测失败", { description: result.error });
      }
      await load();
    } catch (error) {
      toast.error("出口 IP 检测失败", { description: api.asError(error) });
    } finally {
      setLoading(false);
    }
  }

  const current = snapshot?.current ?? null;
  const sharing = snapshot?.sharing ?? null;
  const warn = sharing?.warn === true;

  return (
    <>
      <div
        className={cn(
          "relative mb-6 overflow-hidden rounded-2xl border px-5 py-4 shadow-[0_6px_20px_rgba(15,23,42,.025)]",
          warn ? "border-amber-400/60 bg-amber-50/60" : "border-border bg-muted/30",
        )}
      >
        <div className="relative flex flex-wrap items-center gap-x-5 gap-y-3">
          <div className="flex min-w-0 flex-1 items-center gap-3">
            <span
              className={cn(
                "inline-flex size-9 shrink-0 items-center justify-center rounded-full",
                warn ? "bg-amber-100 text-amber-700" : "bg-primary/10 text-primary",
              )}
            >
              {warn ? <ShieldAlert className="size-4" /> : <Globe2 className="size-4" />}
            </span>
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-semibold text-foreground">出口 IP</span>
                {current ? (
                  <Badge variant="secondary" className="font-mono text-xs">
                    {current.ip}
                  </Badge>
                ) : (
                  <span className="text-xs text-muted-foreground">尚未检测</span>
                )}
                {current?.cached && (
                  <span className="text-[11px] text-muted-foreground">缓存</span>
                )}
              </div>
              <p className="mt-1 truncate text-xs leading-5 text-muted-foreground">
                {current
                  ? [current.location, current.isp].filter(Boolean).join(" · ") || "无归属地信息"
                  : "点右侧按钮检测当前公网出口，换 IP 后用它确认真的换了"}
                {sharing && sharing.count > 0 && (
                  <>
                    {" · "}
                    <span className={warn ? "font-medium text-amber-700" : undefined}>
                      这个 IP 上有 {sharing.count} 个号
                    </span>
                  </>
                )}
              </p>
            </div>
          </div>

          <div className="flex shrink-0 items-center gap-2">
            <TooltipProvider delayDuration={300}>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-9"
                    disabled={loading}
                    onClick={() => void detect(false)}
                  >
                    {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                    {current ? "检测" : "检测出口 IP"}
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  {current ? "5 分钟内走缓存，不重复打探测接口" : "获取当前公网出口 IP 与归属地"}
                </TooltipContent>
              </Tooltip>
            </TooltipProvider>
            <Button variant="ghost" size="sm" className="h-9" onClick={() => setGuideOpen(true)}>
              <Route />
              换 IP 向导
            </Button>
          </div>
        </div>

        {warn && sharing && (
          <p className="relative mt-3 rounded-lg bg-amber-100/70 px-3 py-2 text-xs leading-5 text-amber-800">
            {sharing.message}
            {sharing.accounts.length > 0 && (
              <>　当前：{sharing.accounts.map((account) => account.name).join("、")}</>
            )}
          </p>
        )}
      </div>

      <Dialog open={guideOpen} onOpenChange={setGuideOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>换 IP 向导</DialogTitle>
            <DialogDescription>
              目标是让新号在一个干净的出口上登录。每一步都给出「怎么算做完了」，避免凭感觉判断。
            </DialogDescription>
          </DialogHeader>

          <ol className="space-y-3">
            {(snapshot?.guide ?? []).map((step) => (
              <li key={step.step} className="flex gap-3 rounded-xl border border-border bg-muted/20 p-3">
                <span className="mt-0.5 inline-flex size-6 shrink-0 items-center justify-center rounded-full bg-primary/10 text-xs font-semibold text-primary">
                  {step.step}
                </span>
                <div className="min-w-0">
                  <p className="text-sm font-medium text-foreground">{step.title}</p>
                  <p className="mt-1 text-xs leading-5 text-muted-foreground">{step.detail}</p>
                  <p className="mt-1.5 text-xs leading-5 text-foreground/80">
                    <span className="text-muted-foreground">完成条件：</span>
                    {step.doneWhen}
                  </p>
                </div>
              </li>
            ))}
          </ol>

          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" disabled={loading} onClick={() => void detect(true)}>
              {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
              重新检测（绕过缓存）
            </Button>
            <span className="text-xs text-muted-foreground">
              第 3 步必须用这个按钮：普通检测会读到换 IP 之前的缓存值。
            </span>
          </div>

          {(snapshot?.history?.length ?? 0) > 0 && (
            <div>
              <p className="mb-2 text-xs font-medium text-foreground">历史出口 IP</p>
              <div className="max-h-40 space-y-1 overflow-auto">
                {snapshot!.history.map((record, index) => (
                  <div
                    key={`${record.ip}-${record.at}`}
                    className="flex items-center justify-between gap-3 rounded-lg px-2 py-1.5 text-xs odd:bg-muted/40"
                  >
                    <span className="font-mono text-foreground">{record.ip}</span>
                    <span className="truncate text-muted-foreground">
                      {record.location || "—"}
                    </span>
                    <span className="shrink-0 text-muted-foreground">
                      {index === 0 ? "当前" : record.atText}
                    </span>
                  </div>
                ))}
              </div>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}
