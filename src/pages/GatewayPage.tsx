import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Check,
  Copy,
  Eye,
  EyeOff,
  Loader2,
  RefreshCw,
  Stethoscope,
  Trash2,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  checkAccountRisk,
  clearGatewayUsage,
  fetchGatewayModels,
  getAccounts,
  getGatewaySnapshot,
  saveGatewayConfig,
  type GatewayModel,
  type GatewayRealm,
  type GatewayRealmSnapshot,
  type GatewaySnapshot,
  type RiskReport,
  type RiskState,
} from "@/lib/api";
import type { AccountMeta } from "@/lib/types";

/** 档位顺序：国内版在前，与账号页一致。 */
const REALMS: GatewayRealm[] = ["cn", "global"];

/** 前端档位（"ai"）→ 网关档位（"global"）。 */
function realmOfVariant(variant: AccountMeta["variant"]): GatewayRealm {
  return variant === "ai" ? "global" : "cn";
}

const RISK_TONE: Record<RiskState, string> = {
  ok: "text-emerald-600 dark:text-emerald-400",
  risk: "text-red-600 dark:text-red-400",
  auth: "text-amber-600 dark:text-amber-400",
  network: "text-amber-600 dark:text-amber-400",
  other: "text-muted-foreground",
};

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/**
 * OpenAI 兼容网关页面。
 *
 * 国内版与国际版是**两套完全独立的网关**：接口路径、密钥、号池、每日上限、
 * 用量统计各算各的。填了哪个地址就一定在打哪一边的号，不会串。
 */
export default function GatewayPage() {
  const [snapshot, setSnapshot] = useState<GatewaySnapshot | null>(null);
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const [models, setModels] = useState<Record<string, GatewayModel[] | string>>({});
  const [risk, setRisk] = useState<RiskReport | null>(null);
  const [origin, setOrigin] = useState("");

  const refresh = useCallback(async () => {
    try {
      const [next, accountList] = await Promise.all([getGatewaySnapshot(), getAccounts()]);
      setSnapshot(next);
      setAccounts(accountList.accounts ?? []);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (typeof window !== "undefined") setOrigin(window.location.origin);
    void refresh();
  }, [refresh]);

  const accountsByRealm = useMemo(() => {
    const grouped: Record<GatewayRealm, AccountMeta[]> = { cn: [], global: [] };
    for (const account of accounts) grouped[realmOfVariant(account.variant)].push(account);
    return grouped;
  }, [accounts]);

  const run = useCallback(
    async (tag: string, action: () => Promise<void>) => {
      setBusy(tag);
      setError(null);
      try {
        await action();
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const patchRealm = useCallback(
    (realm: GatewayRealm, patch: Parameters<typeof saveGatewayConfig>[1]) =>
      run(`save:${realm}`, async () => {
        setSnapshot(await saveGatewayConfig(realm, patch));
      }),
    [run],
  );

  if (loading) {
    return (
      <div className="flex items-center gap-2 p-8 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" />
        正在读取网关状态…
      </div>
    );
  }

  return (
    <div className="mx-auto flex w-full max-w-5xl flex-col gap-5 p-6">
      <header className="flex flex-col gap-1">
        <h1 className="text-lg font-medium tracking-[-0.01em]">OpenAI 兼容网关</h1>
        <p className="text-sm text-muted-foreground">
          把账号库变成一个标准 OpenAI 接口：Cherry Studio、ChatBox、Cline、Continue、Dify
          以及各种 OpenAI SDK 填上地址与密钥即可直接用。请求按「最久没用过的号优先」自动换号，
          某个号撞限额或掉线时自动切下一个号重试。
        </p>
      </header>

      {error && (
        <div className="rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
          {error}
        </div>
      )}

      {snapshot &&
        REALMS.map((realm) => (
          <RealmCard
            key={realm}
            snapshot={snapshot[realm]}
            origin={origin}
            accounts={accountsByRealm[realm]}
            busy={busy}
            revealed={Boolean(revealed[realm])}
            models={models[realm]}
            onToggleReveal={() =>
              setRevealed((prev) => ({ ...prev, [realm]: !prev[realm] }))
            }
            onPatch={(patch) => patchRealm(realm, patch)}
            onClearUsage={() =>
              run(`clear:${realm}`, async () => {
                setSnapshot(await clearGatewayUsage(realm));
              })
            }
            onLoadModels={() =>
              run(`models:${realm}`, async () => {
                const result = await fetchGatewayModels(realm);
                setModels((prev) => ({
                  ...prev,
                  [realm]: "error" in result ? result.error : result.data,
                }));
              })
            }
          />
        ))}

      <RiskSection
        report={risk}
        busy={busy}
        onRun={(realm) =>
          run(`risk:${realm ?? "all"}`, async () => {
            setRisk(await checkAccountRisk(realm));
            // 体检结论会写回账号记录，网关挑号时会立刻用上。
            await refresh();
          })
        }
      />
    </div>
  );
}

function RealmCard({
  snapshot,
  origin,
  accounts,
  busy,
  revealed,
  models,
  onToggleReveal,
  onPatch,
  onClearUsage,
  onLoadModels,
}: {
  snapshot: GatewayRealmSnapshot;
  origin: string;
  accounts: AccountMeta[];
  busy: string | null;
  revealed: boolean;
  models?: GatewayModel[] | string;
  onToggleReveal: () => void;
  onPatch: (patch: Parameters<typeof saveGatewayConfig>[1]) => void;
  onClearUsage: () => void;
  onLoadModels: () => void;
}) {
  const realm = snapshot.realm;
  const [limitDraft, setLimitDraft] = useState(String(snapshot.dailyLimit));
  const [copied, setCopied] = useState<string | null>(null);

  useEffect(() => {
    setLimitDraft(String(snapshot.dailyLimit));
  }, [snapshot.dailyLimit]);

  const baseUrl = `${origin}${snapshot.path}`;
  const selected = new Set(snapshot.pool);

  const doCopy = async (tag: string, text: string) => {
    if (await copyText(text)) {
      setCopied(tag);
      window.setTimeout(() => setCopied((prev) => (prev === tag ? null : prev)), 1500);
    }
  };

  return (
    <Card>
      <CardHeader className="flex-row items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <CardTitle className="flex items-center gap-2 text-base">
            {snapshot.label}
            <Badge variant="secondary" className="h-5 border-0 px-1.5 text-[10px]">
              {snapshot.path}
            </Badge>
            {snapshot.enabled ? (
              <Badge className="h-5 border-0 bg-emerald-600/15 px-1.5 text-[10px] text-emerald-700 dark:text-emerald-400">
                已启用
              </Badge>
            ) : (
              <Badge variant="secondary" className="h-5 border-0 px-1.5 text-[10px]">
                已关闭
              </Badge>
            )}
          </CardTitle>
          <CardDescription>
            {snapshot.label}账号独立成一套：密钥、号池、每日上限、用量都与另一版本分开计算。
          </CardDescription>
        </div>
        <Switch
          checked={snapshot.enabled}
          disabled={busy === `save:${realm}`}
          onCheckedChange={(checked) => onPatch({ enabled: checked })}
          aria-label={`启用${snapshot.label}网关`}
        />
      </CardHeader>

      <CardContent className="flex flex-col gap-5">
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1.5">
            <Label className="text-xs text-muted-foreground">接口地址（Base URL）</Label>
            <div className="flex items-center gap-2">
              <Input readOnly value={baseUrl} className="font-mono text-xs" />
              <Button
                type="button"
                variant="outline"
                size="icon"
                aria-label="复制接口地址"
                onClick={() => void doCopy("url", baseUrl)}
              >
                {copied === "url" ? <Check className="size-4" /> : <Copy className="size-4" />}
              </Button>
            </div>
          </div>

          <div className="flex flex-col gap-1.5">
            <Label className="text-xs text-muted-foreground">密钥（API Key）</Label>
            <div className="flex items-center gap-2">
              <Input
                readOnly
                value={revealed ? snapshot.key : "•".repeat(Math.min(snapshot.key.length, 28))}
                className="font-mono text-xs"
              />
              <Button
                type="button"
                variant="outline"
                size="icon"
                aria-label={revealed ? "隐藏密钥" : "显示密钥"}
                onClick={onToggleReveal}
              >
                {revealed ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
              </Button>
              <Button
                type="button"
                variant="outline"
                size="icon"
                aria-label="复制密钥"
                onClick={() => void doCopy("key", snapshot.key)}
              >
                {copied === "key" ? <Check className="size-4" /> : <Copy className="size-4" />}
              </Button>
            </div>
            <div className="flex items-center gap-2 pt-1">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-xs"
                disabled={busy === `save:${realm}`}
                onClick={() => onPatch({ key: "regenerate" })}
              >
                重新生成密钥
              </Button>
              <span className="text-[11px] text-muted-foreground">
                国内版与国际版密钥不通用，混填会直接 401。
              </span>
            </div>
          </div>
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1.5">
            <Label className="text-xs text-muted-foreground">每日上限（0 = 不限）</Label>
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                value={limitDraft}
                onChange={(event) => setLimitDraft(event.target.value)}
                className="w-32"
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={busy === `save:${realm}`}
                onClick={() => onPatch({ dailyLimit: Number(limitDraft) || 0 })}
              >
                保存
              </Button>
              <span className="text-[11px] text-muted-foreground">
                今日已用 {snapshot.today}
                {snapshot.dailyLimit > 0 ? ` / ${snapshot.dailyLimit}` : ""}
              </span>
            </div>
          </div>

          <div className="flex flex-col gap-1.5">
            <Label className="text-xs text-muted-foreground">用量</Label>
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
              <span>累计 {snapshot.total} 次请求</span>
              <span>{snapshot.tokens.toLocaleString()} tokens</span>
              <span>失败 {snapshot.failures}</span>
              <span>冷却中 {snapshot.cooling}</span>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-xs"
                disabled={busy === `clear:${realm}`}
                onClick={onClearUsage}
              >
                <Trash2 className="size-3.5" />
                清空用量
              </Button>
            </div>
          </div>
        </div>

        <div className="flex flex-col gap-2">
          <div className="flex items-center justify-between">
            <Label className="text-xs text-muted-foreground">
              参与轮转的号（不勾选 = 该版本全部账号参与）
            </Label>
            <div className="flex items-center gap-2">
              <span className="text-[11px] text-muted-foreground">
                共 {snapshot.accountCount} 个号
              </span>
              {snapshot.pool.length > 0 && (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-7 px-2 text-xs"
                  disabled={busy === `save:${realm}`}
                  onClick={() => onPatch({ pool: [] })}
                >
                  全选
                </Button>
              )}
            </div>
          </div>
          {accounts.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              还没有{snapshot.label}账号，先在账号页保存一个。
            </p>
          ) : (
            <div className="flex max-h-40 flex-col gap-1 overflow-y-auto rounded-lg border border-border/60 p-2">
              {accounts.map((account) => {
                const label = account.note || account.nickname || account.email || account.uid || account.id;
                const checked = selected.size === 0 || selected.has(account.id);
                return (
                  <label key={account.id} className="flex items-center gap-2 text-xs">
                    <Checkbox
                      checked={checked}
                      disabled={busy === `save:${realm}`}
                      onCheckedChange={(next) => {
                        const base = new Set(selected.size === 0 ? accounts.map((a) => a.id) : selected);
                        if (next) base.add(account.id);
                        else base.delete(account.id);
                        onPatch({ pool: Array.from(base) });
                      }}
                    />
                    <span className="truncate">{label}</span>
                  </label>
                );
              })}
            </div>
          )}
        </div>

        <div className="flex flex-col gap-2">
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={busy === `models:${realm}`}
              onClick={onLoadModels}
            >
              {busy === `models:${realm}` ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              查可用模型
            </Button>
            <span className="text-[11px] text-muted-foreground">
              官方模型多为推理型，客户端里 max_tokens 建议填 512 以上，太小会只思考不出话。
            </span>
          </div>
          {typeof models === "string" && (
            <p className="text-xs text-destructive">{models}</p>
          )}
          {Array.isArray(models) && (
            <div className="flex flex-wrap gap-1">
              {models.map((model) => (
                <Badge key={model.id} variant="secondary" className="border-0 text-[10px] font-normal">
                  {model.id}
                </Badge>
              ))}
            </div>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function RiskSection({
  report,
  busy,
  onRun,
}: {
  report: RiskReport | null;
  busy: string | null;
  onRun: (realm?: GatewayRealm | "all") => void;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Stethoscope className="size-4" />
          风控体检
        </CardTitle>
        <CardDescription>
          上游风控是**按账号**下手的：被风控的号 token 照样能续期、积分照样能查，但一发对话就被拒。
          所以只有真发一条最小对话才知道还能不能用。体检只读不改号，被判风控的号 6
          小时内不再参与网关轮转。
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <div className="flex flex-wrap items-center gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={busy?.startsWith("risk") === true}
            onClick={() => onRun("all")}
          >
            {busy === "risk:all" ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <Stethoscope className="size-3.5" />
            )}
            全部号体检
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={busy?.startsWith("risk") === true}
            onClick={() => onRun("cn")}
          >
            只体检国内版
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={busy?.startsWith("risk") === true}
            onClick={() => onRun("global")}
          >
            只体检国际版
          </Button>
          {report && (
            <span className="text-[11px] text-muted-foreground">
              {report.checkedAt} · 可用 {report.counts.ok} / 风控 {report.counts.risk} / 掉线{" "}
              {report.counts.auth} / 网络 {report.counts.network} / 其他 {report.counts.other}
            </span>
          )}
        </div>

        {report && report.rows.length > 0 && (
          <div className="max-h-80 overflow-y-auto rounded-lg border border-border/60">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-52">账号</TableHead>
                  <TableHead className="w-32">结论</TableHead>
                  <TableHead>说明</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {report.rows.map((row) => (
                  <TableRow key={`${row.realm}:${row.id}`}>
                    <TableCell className="truncate text-xs">
                      {row.name}
                      <span className="ml-1 text-[10px] text-muted-foreground">
                        {row.realm === "global" ? "国际版" : "国内版"}
                      </span>
                    </TableCell>
                    <TableCell className={`text-xs ${RISK_TONE[row.state]}`}>{row.label}</TableCell>
                    <TableCell className="text-xs text-muted-foreground">{row.message}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
