# 从 WorkDaddy 导入账号

WorkDaddy 是另一款 WorkBuddy 多账号管理器。如果你在那边攒了一堆号，不用一个个重新扫码——
用它的「导出」功能导出一个加密 JSON，在这里导进来即可。

实现落在 `wb-switch-core` 的 `modules::workdaddy`。

## 目录

- [怎么用](#怎么用)
- [包格式](#包格式)
- [为什么 scrypt 是自己写的](#为什么-scrypt-是自己写的)
- [导入语义](#导入语义)
- [已知限制](#已知限制)

## 怎么用

1. 在 WorkDaddy 里导出账号，记下你设的密码。
2. 账号页 →「从 WorkDaddy 导入」→ 选文件 → 填密码 → 导入。

**解密在本机完成，密码不会离开这台电脑**——这一点在弹框里也写明了。

对应的接口：

| 入口 | 命令 / 请求 |
| --- | --- |
| webui | `POST /api/import-workdaddy`，body `{ "fileText": "…", "password": "…" }` |
| 桌面端 | `import_workdaddy_export` 命令 |

包体本身就是 JSON（密文在里面是 base64），所以和「导入备份」一样走文本通道，
不为它单开一条二进制通道。

## 包格式

```json
{
  "wbsExport": "WorkDaddy",
  "version": 2,
  "exportType": "accounts",
  "data": "<base64>",
  "salt": "<base64>"
}
```

校验顺序（任一条不过就拒掉，不猜）：

1. `wbsExport` 必须等于 `"WorkDaddy"`。
2. `version` ∈ `(2, 3)`。
3. `exportType` ∈ `(null, "accounts")`。
4. `salt` 解 base64 后必须是 **16 字节**；`data` 解 base64 后必须 **> 28 字节**。
5. `key = scrypt(password_utf8, salt, N=16384, r=8, p=1, dkLen=32)`。
6. `data` 的布局是 `nonce(12) ‖ ciphertext ‖ tag(16)`，AES-256-GCM 解密。
7. **`version == 3` 的明文是 gzip 压缩的**；`version == 2` 是裸 JSON。
8. 明文里要求 `exportType == "WorkDaddy-accounts"`，且 `accounts` 是数组。
9. `accounts` 的每一项是 `{ "uid": …, "info": … }`，而 **`info` 是一个 JSON 字符串**
   （不是对象），里面才是账号本体。

文件读取容忍 UTF-8 BOM（等价于 Python 的 `utf-8-sig`），上限 8 MB。

账号本体到本仓库账号记录的字段映射：

| WorkDaddy | 本仓库 |
| --- | --- |
| `uid` | `uid` |
| `access_token` / `accessToken` | `access_token` |
| `refresh_token` / `refreshToken` | `refresh_token` |
| `token_type` / `tokenType` | `token_type`（缺省 `Bearer`） |
| `email` / `nickname` | 同名 |
| `enterprise_name` / `enterpriseName` | `enterpriseName` |
| `enterprise_id` / `enterpriseId` | `enterpriseId` |
| `expires_at` / `expiresAt` | `expiresAt` |
| `domain` | `domain`，并据此判定国内版 / 国际版 |
| —（整份 info） | `auth_raw`（原样留底，映射漏了字段也还能找回来） |

## 为什么 scrypt 是自己写的

为了**不引入新依赖**。`scrypt` crate 会带进 `salsa20` + `password-hash` 两个新包；而本仓库
已经有 `pbkdf2`（默认带 hmac 特性）与 `sha2`——scrypt 就是「PBKDF2-HMAC-SHA256 + Salsa20/8
的 ROMix」，把后者补上即可，约 120 行。

这不算「自己造密码学」：scrypt 是 RFC 7914 规定的算法，有官方测试向量，而**向量 3 的参数
（N=16384, r=8, p=1）与 WorkDaddy 用的完全一致**。测试里钉的就是这条向量，所以「我们的
scrypt 与 Python `cryptography` 的 `Scrypt` 互通」是被证明的，不是猜的。

（唯一新增的直接依赖是 `flate2`，用于 v3 的 gzip。它本来就在依赖图里，只是从传递依赖提成了
直接依赖，锁文件只多一行。）

## 导入语义

返回 `{ imported, skipped, failed, total, innerLayer, accounts }`：

- **`skipped`**：uid 已在账号库里，**不动已有记录**。与「自动保存新号」同一条原则——
  库里的记录可能带着你自己的备注、显示字段、代理配置，一次导入没资格覆盖它们。
- **`failed`**：逐条的错误文案，**不中断整包**。一个坏条目不该让其余账号都进不来。
  弹框会把这些原因列出来。
- **`innerLayer`**：包里存在 WorkBuddy 5.6 的 `$wbEncrypted` 信封。

## 已知限制

- **不支持 WorkBuddy 5.6 加密信封的包。** 如果 `info` 里出现 `$wbEncrypted`，说明 token 是
  信封而不是明文，那是另一层密钥的事。本模块**不**擅自解密——猜错会把坏数据写进账号库。
  遇到这种条目会明确报错并跳过，`innerLayer` 会置真。
- **只支持 v2 / v3。** 更高版本没有样本，不做兼容猜测。
- **密码只在导入时用。** 不落盘、不缓存；想再导一次就再填一次。
- **导入的 token 可能已过期。** 与其它导入途径一样，切换后可能需要重新登录。
