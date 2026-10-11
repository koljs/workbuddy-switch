import { useEffect, useState } from "react";
import { Check, Loader2, Plug, Trash2 } from "lucide-react";
import { toast } from "sonner";

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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import * as api from "@/lib/api";
import { accountIdentity } from "@/lib/account-display";
import type { AccountMeta } from "@/lib/types";

/**
 * 账号级出口代理配置。
 *
 * 为什么按号配而不是全局配：上游风控看的是「同一个出口 IP 上出现了多少个账号」，
 * 所以真正有用的是**让不同账号走不同出口**。全局代理只能整体换一次 IP，做不到这件事。
 *
 * 保存前先跑一次自检：代理地址写错了（端口写反、代理没启动、socks5 而不是 http）
 * 如果留到下一次发对话才暴露，排查成本会高得多——那时候报错是「上游 403」，看起来
 * 跟代理毫无关系。
 */
interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标账号；为空时不渲染内容。 */
  account: AccountMeta | null;
  /** 保存成功后回调（用于刷新账号列表里的代理标记）。 */
  onSaved?: () => void;
}

export function AccountProxyDialog({ open, onOpenChange, account, onSaved }: Props) {
  const [value, setValue] = useState("");
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [testResult, setTestResult] = useState<api.ProxyTestResult | null>(null);

  // 每次打开都从账号当前值回填：关掉弹窗后用户可能在别处改过（例如换 IP 向导）。
  useEffect(() => {
    if (open) {
      setValue(account?.proxy ?? "");
      setTestResult(null);
    }
  }, [open, account?.proxy, account?.id]);

  async function runTest() {
    if (testing) return;
    setTesting(true);
    setTestResult(null);
    try {
      const result = await api.testAccountProxy(value);
      setTestResult(result);
      if (result.ok) {
        toast.success("代理可用", { description: result.message });
      } else {
        toast.error("代理不可用", { description: result.message });
      }
    } catch (error) {
      toast.error("代理自检失败", { description: api.asError(error) });
    } finally {
      setTesting(false);
    }
  }

  async function save() {
    if (!account || saving) return;
    setSaving(true);
    try {
      const result = await api.saveAccountProxy(account.id, value);
      toast.success(result.proxy ? "已保存出口代理" : "已改回直连", {
        description: result.proxy ? result.masked : "该账号的请求将不再经过代理",
      });
      onSaved?.();
      onOpenChange(false);
    } catch (error) {
      toast.error("保存失败", { description: api.asError(error) });
    } finally {
      setSaving(false);
    }
  }

  const changed = (account?.proxy ?? "") !== value.trim();

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Plug className="size-4" />
            出口代理
          </DialogTitle>
          <DialogDescription>
            {account ? accountIdentity(account) : ""}　·　配好之后，该账号的对话、token 续期与风控体检都会走这个出口。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="account-proxy-input">代理地址</Label>
            <Input
              id="account-proxy-input"
              value={value}
              onChange={(event) => {
                setValue(event.target.value);
                setTestResult(null);
              }}
              placeholder="http://127.0.0.1:7890　（留空 = 直连）"
              spellCheck={false}
              autoComplete="off"
            />
            <p className="text-xs leading-5 text-muted-foreground">
              只支持 <code className="font-mono">http://</code> 与{" "}
              <code className="font-mono">https://</code>。可以带认证：
              <code className="font-mono">http://user:pass@host:port</code>。不写 scheme 时按 http
              处理；socks5 不支持。
            </p>
          </div>

          {testResult && (
            <div
              className={
                testResult.ok
                  ? "rounded-lg border border-emerald-300/60 bg-emerald-50/60 px-3 py-2 text-xs leading-5 text-emerald-800"
                  : "rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2 text-xs leading-5 text-destructive"
              }
            >
              <p>{testResult.message}</p>
              {testResult.latencyMs !== null && (
                <p className="mt-0.5 text-muted-foreground">往返 {testResult.latencyMs} ms</p>
              )}
              {testResult.ip && (
                <p className="mt-1 flex items-center gap-2">
                  该代理出口 IP：
                  <Badge variant="secondary" className="font-mono text-[11px]">
                    {testResult.ip}
                  </Badge>
                </p>
              )}
            </div>
          )}

          {account?.egressIp && (
            <p className="text-xs text-muted-foreground">
              这个号记录的登录出口是{" "}
              <span className="font-mono text-foreground">{account.egressIp}</span>。
            </p>
          )}
        </div>

        <DialogFooter className="flex-wrap gap-2 sm:justify-between">
          <Button
            variant="outline"
            size="sm"
            disabled={testing || !value.trim()}
            onClick={() => void runTest()}
          >
            {testing ? <Loader2 className="animate-spin" /> : <Check />}
            测试连通性
          </Button>
          <div className="flex items-center gap-2">
            {account?.proxy ? (
              <Button
                variant="ghost"
                size="sm"
                className="text-destructive hover:text-destructive"
                disabled={saving}
                onClick={() => {
                  setValue("");
                  setTestResult(null);
                }}
              >
                <Trash2 />
                改回直连
              </Button>
            ) : null}
            <Button size="sm" disabled={saving || !changed} onClick={() => void save()}>
              {saving ? <Loader2 className="animate-spin" /> : null}
              保存
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
