# 分享包：把账号连凭证打成一个「双击即登录」的包

分享包解决一个很具体的场景：你想把某个 WorkBuddy 账号交给同事用，但不想把
「先装工具 → 打开 → 扫码/粘贴 token → 切换」这套流程讲一遍。

生成一个 `.zip` 发过去，对方**解压后双击一个 .bat** 就完事。

实现落在 `wb-switch-core` 的 `modules::share`，桌面端（Tauri commands）与
webui（HTTP server）共用同一份实现，两边行为一致。

## 目录

- [为什么需要分享包](#为什么需要分享包)
- [包里有什么](#包里有什么)
- [生成分享包](#生成分享包)
- [对方怎么用](#对方怎么用)
- [导入的三种入口](#导入的三种入口)
- [安全边界](#安全边界)
- [文件与配置](#文件与配置)
- [已知限制](#已知限制)

## 为什么需要分享包

账号凭证本身很好搬——一个 JSON。真正麻烦的是**对方那边**：

- 对方的机器上有没有 `workbuddy-switch`？没有就得先教他装。
- 装完之后用什么命令导入？路径里有中文、有空格怎么办？
- 双击一个 `.bat` 时，Windows 用哪个代码页解释它？脚本里既有中文路径又有中文提示，
  存成 GBK 在开了「Beta: 使用 UTF-8 提供全球语言支持」的机器上乱码，存 UTF-8 而
  不切码页则在默认中文机器上乱码。

分享包把这些都封在包里：一个自解释的 `.bat`、一份说明书、一份凭证，外加一个
自检——生成时会把包读回来验一遍，确认包内 uid 与账号一致。**写坏的包发给同事才最尴尬**。

## 包里有什么

```
WB登录包-<账号名>-001.zip
├── Windows电脑点这个登录.bat        双击入口
├── 使用说明.txt                    给对方的说明书（含账号名与生成时间）
└── 此文件勿动无需操作/
    ├── account.json                凭证（白名单裁剪后的账号记录）
    └── 版本.txt                     生成器版本 / 时间 / 格式版本 / 隐私声明
```

四个文件全部由本仓库生成，**没有任何 Python 依赖**。

`.bat` 的行为（`share::bat_content()`）：

1. `chcp 65001 >nul` 切到 UTF-8 代码页——这是中文不乱码的前提。
2. `%~dp0` 定位脚本自己所在的目录，于是「解压到哪都能跑」，不依赖当前工作目录。
3. 找命令行程序，顺序是：包内 `此文件勿动无需操作/wb-switch.exe` → `PATH` 上的
   `wb-switch` → `PATH` 上的 `workbuddy-switch`。三个都找不到就打印该装什么，`exit /b 1`。
4. 执行 `"%CLI%" import-share "%PAYLOAD%" --switch`。
5. **按 `%ERRORLEVEL%` 如实报成败**：`已完成。` / `执行失败，错误码 N。`，然后 `pause`
   让窗口停住，最后 `exit /b %STATUS%` 把退出码透传出去。

第 5 点不是装饰：`.bat` 是对方判断「到底成没成」的唯一依据，`import_share` 里吞掉错误
会让对方看到「已完成」却什么都没发生。

## 生成分享包

三个入口，都调同一个 `share::build_package()`：

| 入口 | 操作 |
| --- | --- |
| 桌面端 | 账号卡片 →「⋯」菜单 →「生成分享包」 |
| webui | `POST /api/share`，body `{"id": "<账号 id>"}` |
| CLI | 暂无（生成需要读账号库，交给上面两个入口更合适） |

生成结果写到 `<store>/分享包/WB登录包-<账号名>-<NNN>.zip`，并追加一条历史记录。

文件名细节：

- `<账号名>` 会先过 `sanitize_stem()`：Windows 非法字符 `\ / : * ? " < > |` 与控制字符
  一律换成 `_`，再去掉首尾空白和点号；清洗后为空则回落到 `账号`。
- `<NNN>` 是三位序号，从 **已有最大序号 + 1** 开始（不是「文件个数 + 1」——中间删掉几个
  包之后再生成不会撞名），到 999 后回到 1。

## 对方怎么用

1. 解压到任意文件夹（右键 → 全部解压）。
2. 双击 `Windows电脑点这个登录.bat`。
3. 看到「已完成。」就成了，打开 WorkBuddy 即是该账号。

前提是对方机器上能找到 `wb-switch`：要么装了 npm 全局包（`npm i -g workbuddy-switch`），
要么你生成时把 `wb-switch.exe` 放进包里（见「已知限制」）。

**对话记录不会跟着走**。登录完成后想迁移，用 workbuddy-switch 的「会话迁移」显式迁过去。
这是刻意的：自动把分享者的会话写进对方账号，属于会改对方数据的动作，不能顺手做掉。

## 导入的三种入口

| 入口 | 命令 / 请求 |
| --- | --- |
| 双击 `.bat` | 内部就是调下面这条 CLI |
| CLI | `workbuddy-switch import-share <分享包目录\|zip> [--switch]` |
| webui | `POST /api/share/import`，body `{"path": "...", "switch": true}` |
| 桌面端 | `import_share_package` 命令（供后续 UI 使用） |

`import_package()` 做的事，按顺序：

1. `read_payload()` 取凭证。目录形态会依次找 `<dir>/此文件勿动无需操作/account.json`、
   `<dir>/account.json`、以及各子目录里的数据目录；zip 形态直接按条目名读。
2. `validate_payload()` 要求 `uid` 与 `access_token`/`accessToken` 都在——缺一个就拒掉，
   不写半条脏记录进账号库。
3. `account::save_collected_account()` 写进账号库。
4. `auth_file::write_account_to_auth_file()` 写进本机 WorkBuddy 的 auth 文件。
5. 只有传了 `--switch` 才真正切过去；切换失败不算导入失败，会在结果里标 `switched: false`
   并提示手动切。

退出码：`0` 成功，`1` 失败（读包 / 写盘 / 校验），`2` 参数用错。`.bat` 依赖这个区分。

## 安全边界

**包里是账号凭证，等同于账号本身。** 生成后的 `.zip` 请勿转发给不信任的人，
也不要放到公网可访问的位置。

为此做了两件事：

**一、凭证白名单。** `payload_of()` 只复制这些字段：

```
id, uid, email, nickname, enterpriseName, variant, realm,
access_token, refresh_token, token_type, expires_at, auth_raw,
saved_at, imported_from
```

**刻意不含** `egress_ip` / `egress_at` / `proxy` / `risk_state` / `risk_at` / `last_used_at`。
理由：分享的是账号，不是分享者的网络画像。出口 IP 与代理配置属于「这个人怎么上网」，
跟对方能不能用这个账号无关，漏出去反而给对方带来无谓的信息暴露。包内 `版本.txt` 里也
写明了这一点。

**二、导入侧防目录穿越。** `GET /api/share/download?file=` 只接受**纯文件名**：带 `/`、
`\`、`..`，或路径分量不等于 1 的，一律 400；且必须以 `WB登录包-` 开头、`.zip` 结尾。
否则这就成了一个「读服务器任意文件」的接口。

和 `/api/gateway*` 一样，这些是管理接口，**只在回环上跑、不做鉴权**。别把它们暴露到公网。

## 文件与配置

| 路径 | 说明 |
| --- | --- |
| `<store>/分享包/` | 生成的包都放这里 |
| `<store>/share_history.json` | 历史记录，最多保留 200 条 |

历史记录放在 store 根目录而不是 `分享包/` 里，与 `config.rs` 里其它状态文件
（`accounts.json`、`auto_checkin_logs.json`…）保持一致；`分享包/` 只放包本身。
好处是 `next_index()` 扫目录时不必额外排除非包文件。

`<store>` 与其他模块一致（见 `config::store_dir()`），即 workbuddy-switch 的数据目录。

历史记录每条含：`file` / `path` / `account_id` / `account_name` / `uid` / `bytes` / `created_at`。

## 已知限制

- **只有 Windows 端能「双击即登录」。** `.bat` 是 Windows 的东西；macOS / Linux 对方拿到包
  只能自己跑 `wb-switch import-share <包> --switch`。上游那份参考实现里还带了一套 Python
  运行时安装脚本（`install_macos_runtime.sh` / `install_windows_runtime.ps1`）来兜住
  「对方没装 Python」，本仓库没走这条路——我们的导入逻辑本来就在 `wb-switch` 里，不需要
  再塞一份 Python 实现进包，少一层就少一处会坏的地方。
- **默认不打包 `wb-switch.exe`。** 包里给 `此文件勿动无需操作/wb-switch.exe` 留了位置
  （`.bat` 会优先用它），但默认不生成——二进制会让包体积从几 KB 涨到几十 MB。
  需要「对方零安装」时，把 `wb-switch.exe` 手动放进那个目录再压缩即可。
- **zip 只写 STORE（不压缩）。** 凭证是几 KB 的文本，压不压没区别，而自写 STORE 编码器
  换掉了整个 `zip` 依赖。读的时候遇到非 STORE 条目会明确报错，不会静默出错。
- **包名里的序号只认本目录。** 换了 `分享包` 目录，序号从头算。
- **导入不合并会话。** 见「对方怎么用」。
