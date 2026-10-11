import { useEffect, useRef, useState } from "react";
import { FileUp, Loader2 } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
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

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 导入完成后的回调（含逐条失败原因与内层信封标记）。 */
  onImported?: (result: api.WorkDaddyImportResult) => void;
}

/**
 * 从 WorkDaddy 导入账号：选它「导出」出来的加密 JSON + 填当时设的密码。
 *
 * 解密在本机做，密码不出本机——这条要写在界面上，否则用户不敢填。
 */
export function ImportWorkDaddyDialog({ open, onOpenChange, onImported }: Props) {
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [fileName, setFileName] = useState("");
  const [fileText, setFileText] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    if (open) {
      setFileName("");
      setFileText("");
      setPassword("");
      setBusy(false);
      setError("");
    }
  }, [open]);

  function onFileChange(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    // 清空 value，允许再次选择同一文件
    e.target.value = "";
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      setFileName(file.name);
      setFileText(String(reader.result ?? ""));
      setError("");
    };
    reader.readAsText(file);
  }

  const ready = fileText.length > 0 && password.trim().length > 0;

  async function doImport() {
    if (busy || !ready) return;
    setBusy(true);
    setError("");
    try {
      const res = await api.importWorkDaddyExport(fileText, password);
      onImported?.(res.result);
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="min-w-0 overflow-x-hidden">
        <DialogHeader>
          <DialogTitle>从 WorkDaddy 导入</DialogTitle>
          <DialogDescription>
            选择 WorkDaddy 导出的加密 JSON，并填入当时设置的密码。解密在本机完成，密码不会离开这台电脑。
          </DialogDescription>
        </DialogHeader>

        <input
          ref={fileInputRef}
          type="file"
          accept=".json,application/json"
          className="hidden"
          onChange={onFileChange}
        />

        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <Button variant="outline" onClick={() => fileInputRef.current?.click()} disabled={busy}>
              <FileUp />
              选择文件
            </Button>
            {fileName && <span className="truncate text-xs text-muted-foreground">{fileName}</span>}
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="workdaddy-password">导出密码</Label>
            <Input
              id="workdaddy-password"
              type="password"
              autoComplete="off"
              value={password}
              disabled={busy}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="WorkDaddy 导出时设置的密码"
            />
          </div>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={() => void doImport()} disabled={busy || !ready}>
            {busy ? (
              <>
                <Loader2 className="animate-spin" />
                解密中…
              </>
            ) : (
              "导入"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
