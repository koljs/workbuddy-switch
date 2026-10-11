//! 分享包（双击即登录）。
//!
//! 把一个账号打包成 zip：解压后双击 `Windows电脑点这个登录.bat`，即可在**对方自己的
//! 电脑**上把该账号写进本机 WorkBuddy 并完成登录。
//!
//! 包内结构（与 WBMultiLogin 的分享包同构）：
//!
//! ```text
//! WB登录包-<账号>-001.zip
//! ├─ Windows电脑点这个登录.bat          ← UTF-8 + chcp 65001，双击即用
//! ├─ 使用说明.txt
//! └─ 此文件勿动无需操作/
//!    ├─ account.json                    ← 账号凭证（导入端只读这个）
//!    ├─ 版本.txt
//!    └─ wb-switch.exe                   ← 可选：随包携带的命令行程序
//! ```
//!
//! 两条刻意的设计约束：
//!
//! 1. **不携带生成者的网络信息**。payload 走白名单复制，`egress_ip` / `proxy` /
//!    `risk_*` 这类字段一律不进包——分享的是账号，不是分享者的网络画像。
//! 2. **zip 自己写（STORE 不压缩）**。不引入 `zip` crate：本模块只需支持「打包」与
//!    「读回自己打的包」，STORE 结构 60 行就能写完，且离线构建可用。文件名带中文，
//!    因此 local header 里必须置 UTF-8 标志位（bit 11），否则 Windows 解压会乱码。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::{account, auth_file, config};

/// 双击入口。名字带中文是刻意的——这是给普通用户看的。
pub const BAT_FILE_NAME: &str = "Windows电脑点这个登录.bat";
pub const README_FILE_NAME: &str = "使用说明.txt";
/// 数据目录。名字带「勿动」是为了劝退手改；导入端只认 `account.json`。
pub const DATA_DIR_NAME: &str = "此文件勿动无需操作";
pub const PAYLOAD_FILE_NAME: &str = "account.json";
pub const VERSION_FILE_NAME: &str = "版本.txt";
/// 包内可选携带的命令行程序名（与 npm 包里的 bin 同名）。
pub const BUNDLED_CLI_FILE_NAME: &str = "wb-switch.exe";

pub const SHARE_DIR_NAME: &str = "分享包";
pub const HISTORY_FILE_NAME: &str = "share_history.json";
pub const PACKAGE_PREFIX: &str = "WB登录包-";
pub const PACKAGE_EXT: &str = ".zip";
/// 序号上限：超过就从头找空位，避免文件名无限增长。
pub const MAX_PACKAGE_INDEX: u32 = 999;
pub const HISTORY_MAX_RECORDS: usize = 200;

/// payload 白名单。**加字段前先问一句：这会不会泄漏生成者的网络信息？**
///
/// 刻意排除：`egress_ip` / `egress_at` / `proxy` / `risk_state` / `risk_at` /
/// `last_used_at` 等——它们描述的是「生成者怎么上网」，与账号能否登录无关。
const PAYLOAD_KEYS: &[&str] = &[
    "id",
    "uid",
    "email",
    "nickname",
    "enterpriseName",
    "variant",
    "realm",
    "access_token",
    "refresh_token",
    "token_type",
    "expires_at",
    "auth_raw",
    "saved_at",
    "imported_from",
];

// ---------------------------------------------------------------- 文件名

/// 清掉 Windows 文件名非法字符与路径分隔符，并去掉首尾空白/点。
///
/// 账号昵称里出现 `公司/部门`、`a:b` 这类字符很常见，直接拿去当文件名会失败。
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let bad = matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || (ch as u32) < 0x20;
        out.push(if bad { '_' } else { ch });
    }
    let trimmed = out.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "账号".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 包名：`WB登录包-<账号>-<三位序号>.zip`。序号让同一个人反复分享不会互相覆盖。
pub fn package_file_name(account_name: &str, index: u32) -> String {
    let index = index.clamp(1, MAX_PACKAGE_INDEX);
    format!(
        "{PACKAGE_PREFIX}{}{PACKAGE_EXT}",
        format_args!("{}-{index:03}", sanitize_stem(account_name))
    )
}

/// 扫目录里已有的同前缀包，返回下一个可用序号（1..=999）。
///
/// 取「最大序号 + 1」而不是「文件数 + 1」：删掉中间某个包后不会撞名。
pub fn next_index(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 1;
    };
    let mut max_index = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(PACKAGE_PREFIX) || !name.ends_with(PACKAGE_EXT) {
            continue;
        }
        let stem = &name[PACKAGE_PREFIX.len()..name.len() - PACKAGE_EXT.len()];
        if let Some((_, tail)) = stem.rsplit_once('-') {
            if let Ok(index) = tail.parse::<u32>() {
                max_index = max_index.max(index);
            }
        }
    }
    if max_index >= MAX_PACKAGE_INDEX {
        1
    } else {
        max_index + 1
    }
}

// ---------------------------------------------------------------- payload

/// 按白名单裁剪账号记录，得到可以放进分享包的凭证。
pub fn payload_of(acc: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in PAYLOAD_KEYS {
        if let Some(value) = acc.get(*key) {
            if !value.is_null() {
                out.insert((*key).to_string(), value.clone());
            }
        }
    }
    out.insert(
        "shared_by".to_string(),
        Value::String("workbuddy-switch".to_string()),
    );
    Value::Object(out)
}

/// 校验包内凭证是否「够用」：缺 uid 或缺 token 就没法登录，早报错好过双击后才失败。
pub fn validate_payload(payload: &Value) -> Result<(), String> {
    if !payload.is_object() {
        return Err("分享包里的 account.json 不是一个对象".to_string());
    }
    let uid = account::get_str(payload, "uid").unwrap_or_default();
    if uid.trim().is_empty() {
        return Err("分享包缺少 uid，无法确认要登录哪个账号".to_string());
    }
    let has_token = account::get_str(payload, "access_token").is_some()
        || account::get_str(payload, "accessToken").is_some();
    if !has_token {
        return Err("分享包缺少 access_token，凭证不完整".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------- 文本模板

/// 双击入口脚本。
///
/// 编码必须是 **UTF-8**，且第一段就要 `chcp 65001`：脚本里既有中文路径又有中文提示，
/// 存成 GBK 会在开了「Beta: 使用 UTF-8 提供全球语言支持」的机器上乱码，存 UTF-8 而不
/// 切码页则会在默认中文机器上乱码。两者都做才两边都对。
pub fn bat_content() -> String {
    let mut s = String::new();
    s.push_str("@echo off\r\n");
    s.push_str("setlocal EnableExtensions\r\n");
    s.push_str("chcp 65001 >nul\r\n");
    s.push_str("\r\n");
    s.push_str("set \"SCRIPT_DIR=%~dp0\"\r\n");
    s.push_str(&format!("set \"DATA_DIR=%SCRIPT_DIR%{DATA_DIR_NAME}\"\r\n"));
    s.push_str(&format!(
        "set \"PAYLOAD=%DATA_DIR%\\{PAYLOAD_FILE_NAME}\"\r\n"
    ));
    s.push_str(&format!(
        "set \"BUNDLED=%DATA_DIR%\\{BUNDLED_CLI_FILE_NAME}\"\r\n"
    ));
    s.push_str("set \"CLI=\"\r\n");
    s.push_str("\r\n");
    s.push_str("rem 1) 包内自带的命令行程序优先，双击即用、不依赖对方装了什么\r\n");
    s.push_str("if exist \"%BUNDLED%\" set \"CLI=%BUNDLED%\"\r\n");
    s.push_str("rem 2) 其次找 PATH 上的 wb-switch / workbuddy-switch\r\n");
    s.push_str("if not defined CLI (\r\n");
    s.push_str("  where wb-switch >nul 2>nul\r\n");
    s.push_str("  if not errorlevel 1 set \"CLI=wb-switch\"\r\n");
    s.push_str(")\r\n");
    s.push_str("if not defined CLI (\r\n");
    s.push_str("  where workbuddy-switch >nul 2>nul\r\n");
    s.push_str("  if not errorlevel 1 set \"CLI=workbuddy-switch\"\r\n");
    s.push_str(")\r\n");
    s.push_str("\r\n");
    s.push_str("if not defined CLI (\r\n");
    s.push_str("  echo [失败] 没有找到 workbuddy-switch 命令行程序。\r\n");
    s.push_str("  echo        请任选其一后重试：\r\n");
    s.push_str("  echo        1. 安装 npm 全局包：npm i -g workbuddy-switch\r\n");
    s.push_str(&format!(
        "  echo        2. 把 wb-switch.exe 放进“{DATA_DIR_NAME}”文件夹\r\n"
    ));
    s.push_str("  pause\r\n");
    s.push_str("  exit /b 1\r\n");
    s.push_str(")\r\n");
    s.push_str("\r\n");
    s.push_str("if not exist \"%PAYLOAD%\" (\r\n");
    s.push_str(&format!(
        "  echo [失败] 子文件夹“{DATA_DIR_NAME}”中缺少 {PAYLOAD_FILE_NAME}。\r\n"
    ));
    s.push_str("  pause\r\n");
    s.push_str("  exit /b 1\r\n");
    s.push_str(")\r\n");
    s.push_str("\r\n");
    s.push_str("echo [WorkBuddy] 正在登录分享包内账号……\r\n");
    s.push_str("echo.\r\n");
    s.push_str("rem --switch：写入凭证后直接把 WorkBuddy 切到该账号（含会话迁移）\r\n");
    s.push_str("\"%CLI%\" import-share \"%PAYLOAD%\" --switch\r\n");
    s.push_str("set \"STATUS=%ERRORLEVEL%\"\r\n");
    s.push_str("echo.\r\n");
    s.push_str(
        "if \"%STATUS%\"==\"0\" (echo 已完成。) else (echo 执行失败，错误码 %STATUS%。)\r\n",
    );
    s.push_str("pause\r\n");
    s.push_str("exit /b %STATUS%\r\n");
    s
}

pub fn readme_content(account_name: &str, created_at: &str) -> String {
    format!(
        "【WorkBuddy 账号登录包】\n\n\
         账号：{account_name}\n\
         生成时间：{created_at}\n\
         由 workbuddy-switch 生成\n\n\
         使用方法（Windows 电脑）：\n\
         1. 把本压缩包解压到任意文件夹（右键 → 全部解压）\n\
         2. 双击「{BAT_FILE_NAME}」\n\
         3. 程序会把包内账号写入你本机的 WorkBuddy 并登录\n\n\
         想把原账号的对话记录也带过去：登录完成后在 workbuddy-switch 里用\n\
         「会话迁移」把它迁到该账号下即可（不会自动执行，避免误改你的会话）。\n\n\
         注意：\n\
         - 「{DATA_DIR_NAME}」文件夹里的内容请勿修改\n\
         - 本包含账号登录凭证，请勿再转发给其他人\n\
         - 若提示找不到命令行程序：安装 npm 全局包 `npm i -g workbuddy-switch`，\n\
           或把 wb-switch.exe 放进「{DATA_DIR_NAME}」文件夹\n"
    )
}

pub fn version_file_content(version: &str, created_at: &str) -> String {
    format!(
        "workbuddy-switch 分享包\n\
         生成器版本：{version}\n\
         生成时间：{created_at}\n\
         格式版本：1\n\
         本包只包含账号凭证，不包含生成者的出口 IP、代理等网络信息。\n"
    )
}

// ---------------------------------------------------------------- zip（STORE）

fn crc32(data: &[u8]) -> u32 {
    // 逐位实现，不建表：单包总数据量在百 KB 量级，这点开销换「无依赖」是划算的。
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 毫秒时间戳 → MS-DOS 的 (time, date) 对（zip 头部用）。
pub fn dos_datetime(now_ms: i64) -> (u16, u16) {
    let secs = now_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 起算的天数 → 民用日期（Howard Hinnant 的 days_from_civil 逆运算）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let year = (year - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | ((m as u16) << 5) | d as u16;
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16 / 2);
    (time, date)
}

/// 写一个 STORE（不压缩）zip。
///
/// 只实现「不压缩」是有意的：压缩要么引依赖，要么自己实现 deflate，而分享包的体积
/// （凭证 JSON 几 KB + 说明文本）本来就没什么可压的。
pub fn build_zip(entries: &[(String, Vec<u8>)], now_ms: i64) -> Vec<u8> {
    let (time, date) = dos_datetime(now_ms);
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;

        out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // bit 11 = 文件名是 UTF-8
        out.extend_from_slice(&0u16.to_le_bytes()); // store
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(&date.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }

    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// 从 STORE 版 zip 里按文件名取出内容。
///
/// 只认自己打的包（不压缩）。对方若用资源管理器重新压缩成 deflate，这里会明确报错，
/// 而不是给出半截数据。
pub fn read_zip_entry(bytes: &[u8], want: &str) -> Result<Vec<u8>, String> {
    let mut pos = 0usize;
    while pos + 30 <= bytes.len() {
        let sig = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        if sig == 0x0201_4B50 || sig == 0x0605_4B50 {
            break; // 到中央目录了，说明前面没命中
        }
        if sig != 0x0403_4B50 {
            return Err("分享包不是有效的 zip（未找到本地文件头）".to_string());
        }
        let method = u16::from_le_bytes([bytes[pos + 8], bytes[pos + 9]]);
        let size = u32::from_le_bytes([
            bytes[pos + 18],
            bytes[pos + 19],
            bytes[pos + 20],
            bytes[pos + 21],
        ]) as usize;
        let name_len = u16::from_le_bytes([bytes[pos + 26], bytes[pos + 27]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[pos + 28], bytes[pos + 29]]) as usize;
        let name_start = pos + 30;
        if name_start + name_len > bytes.len() {
            return Err("分享包已损坏（文件头越界）".to_string());
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_start + name_len]).to_string();
        let data_start = name_start + name_len + extra_len;
        if data_start + size > bytes.len() {
            return Err("分享包已损坏（数据越界）".to_string());
        }
        if name == want || name.ends_with(&format!("/{want}")) {
            if method != 0 {
                return Err(format!(
                    "分享包里的 {want} 是压缩存储的（method={method}），请解压后再导入"
                ));
            }
            return Ok(bytes[data_start..data_start + size].to_vec());
        }
        pos = data_start + size;
    }
    Err(format!("分享包里没有找到 {want}"))
}

// ---------------------------------------------------------------- 路径

pub fn share_dir() -> PathBuf {
    config::store_dir().join(SHARE_DIR_NAME)
}

pub fn history_file() -> PathBuf {
    config::store_dir().join(HISTORY_FILE_NAME)
}

pub fn load_history() -> Vec<Value> {
    std::fs::read_to_string(history_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

pub fn append_history(entry: &Value) -> Result<(), String> {
    let mut history = load_history();
    history.insert(0, entry.clone());
    history.truncate(HISTORY_MAX_RECORDS);
    let path = history_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(&Value::Array(history))
        .map_err(|e| format!("序列化分享历史失败：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入分享历史失败：{e}"))
}

// ---------------------------------------------------------------- 生成

/// 生成一个分享包，返回可直接给前端展示的元数据。
pub fn build_package(account_id: &str, out_dir: Option<&Path>) -> Result<Value, String> {
    let acc = account::find_account(account_id)
        .ok_or_else(|| format!("未找到账号（id={account_id}）"))?;
    let payload = payload_of(&acc);
    validate_payload(&payload)?;

    let dir = out_dir.map(Path::to_path_buf).unwrap_or_else(share_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建分享包目录失败：{e}"))?;

    let display_name = account::account_display_name(&acc);
    let index = next_index(&dir);
    let file_name = package_file_name(&display_name, index);
    let path = dir.join(&file_name);

    let now_ms = config::now_ms();
    let created_at = format_time(now_ms);
    let version = env!("CARGO_PKG_VERSION");
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    entries.push((BAT_FILE_NAME.to_string(), bat_content().into_bytes()));
    entries.push((
        README_FILE_NAME.to_string(),
        readme_content(&display_name, &created_at).into_bytes(),
    ));
    entries.push((
        format!("{DATA_DIR_NAME}/{PAYLOAD_FILE_NAME}"),
        serde_json::to_vec_pretty(&payload).map_err(|e| format!("序列化凭证失败：{e}"))?,
    ));
    entries.push((
        format!("{DATA_DIR_NAME}/{VERSION_FILE_NAME}"),
        version_file_content(version, &created_at).into_bytes(),
    ));

    let zip = build_zip(&entries, now_ms);
    std::fs::write(&path, &zip).map_err(|e| format!("写入分享包失败：{e}"))?;

    // 自检：打完立刻读回，确认包内凭证与预期一致——写坏的包发给同事才最尴尬。
    let read_back = read_zip_entry(&zip, PAYLOAD_FILE_NAME)?;
    let parsed: Value =
        serde_json::from_slice(&read_back).map_err(|e| format!("生成的分享包校验失败：{e}"))?;
    validate_payload(&parsed).map_err(|e| format!("生成的分享包校验失败：{e}"))?;
    if account::get_str(&parsed, "uid") != account::get_str(&payload, "uid") {
        return Err("生成的分享包校验失败：包内 uid 与账号不一致".to_string());
    }

    let entry = json!({
        "file": file_name,
        "path": path.to_string_lossy(),
        "account_id": get_id(&acc, account_id),
        "account_name": display_name,
        "uid": account::get_str(&payload, "uid").unwrap_or_default(),
        "bytes": zip.len(),
        "created_at": created_at,
    });
    append_history(&entry)?;
    Ok(entry)
}

fn get_id(acc: &Value, fallback: &str) -> String {
    account::get_str(acc, "id").unwrap_or_else(|| fallback.to_string())
}

pub fn format_time(now_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| now_ms.to_string())
}

// ---------------------------------------------------------------- 导入

/// 从「已解压的目录」或「分享包 zip」里读出凭证 JSON。
pub fn read_payload(path: &Path) -> Result<Value, String> {
    let bytes = if path.is_dir() {
        let mut candidates = vec![
            path.join(DATA_DIR_NAME).join(PAYLOAD_FILE_NAME),
            path.join(PAYLOAD_FILE_NAME),
        ];
        candidates.extend(
            std::fs::read_dir(path)
                .map_err(|e| format!("读取分享包目录失败：{e}"))?
                .flatten()
                .map(|entry| entry.path().join(DATA_DIR_NAME).join(PAYLOAD_FILE_NAME)),
        );
        let found = candidates
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| format!("分享包里没有找到 {DATA_DIR_NAME}/{PAYLOAD_FILE_NAME}"))?;
        std::fs::read(&found).map_err(|e| format!("读取 {PAYLOAD_FILE_NAME} 失败：{e}"))?
    } else {
        let raw = std::fs::read(path).map_err(|e| format!("读取分享包失败：{e}"))?;
        read_zip_entry(&raw, PAYLOAD_FILE_NAME)?
    };

    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("分享包凭证不是合法 JSON：{e}"))?;
    validate_payload(&payload)?;
    Ok(payload)
}

/// 把包内账号写进本机：入保险柜 + 写 auth 文件。
///
/// `switch` 为真时再走一次完整切换（重启 WorkBuddy），否则只写凭证。
/// 会话迁移**不**在这里做：那会把「对方当前账号」的会话复制到目标账号下，属于一次
/// 明确的操作，应由用户在界面上自己点，而不是双击一个包就悄悄发生。
pub fn import_package(path: &Path, switch: bool) -> Result<Value, String> {
    let payload = read_payload(path)?;
    let variant = account::variant_of(&payload);

    let saved = account::save_collected_account(payload.clone())
        .map_err(|e| format!("写入账号库失败：{e}"))?;
    auth_file::write_account_to_auth_file(&payload, variant)?;

    let uid = account::get_str(&payload, "uid").unwrap_or_default();
    let account_id = account::get_str(&saved, "id").unwrap_or_default();
    let switched = if switch {
        crate::modules::switch::switch_account(None, &account_id, true, false, &[], &[])
            .map(|_| true)
            .unwrap_or(false)
    } else {
        false
    };

    Ok(json!({
        "uid": uid,
        "variant": variant.as_str(),
        "account": account::account_meta(&saved),
        "switched": switched,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-switch-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_account() -> Value {
        json!({
            "id": "acc-1",
            "uid": "uid-123",
            "email": "a@example.com",
            "nickname": "公司/部门:测试",
            "variant": "cn",
            "access_token": "tok",
            "refresh_token": "rtok",
            "egress_ip": "203.0.113.9",
            "egress_at": 1,
            "proxy": "http://127.0.0.1:1080",
            "risk_state": "blocked",
            "last_used_at": 123
        })
    }

    #[test]
    fn sanitize_stem_strips_windows_illegal_chars() {
        assert_eq!(sanitize_stem("公司/部门:测试"), "公司_部门_测试");
        assert_eq!(sanitize_stem("a\\b*c?d\"e<f>g|h"), "a_b_c_d_e_f_g_h");
        assert_eq!(
            sanitize_stem("   "),
            "账号",
            "空名字要有兜底，否则包名会变成 -001.zip"
        );
        assert_eq!(sanitize_stem("..."), "账号");
        assert_eq!(sanitize_stem("正常名字"), "正常名字");
    }

    #[test]
    fn package_file_name_uses_three_digit_index() {
        assert_eq!(package_file_name("张三", 1), "WB登录包-张三-001.zip");
        assert_eq!(package_file_name("张三", 42), "WB登录包-张三-042.zip");
        // 序号越界要夹住，别造出 0000 / 1000 这种怪名字
        assert_eq!(package_file_name("张三", 0), "WB登录包-张三-001.zip");
        assert_eq!(package_file_name("张三", 5000), "WB登录包-张三-999.zip");
    }

    #[test]
    fn next_index_takes_max_plus_one_not_count_plus_one() {
        let dir = temp_dir("share-next-index");
        assert_eq!(next_index(&dir), 1, "空目录从 001 开始");
        std::fs::write(dir.join(package_file_name("a", 1)), b"x").unwrap();
        std::fs::write(dir.join(package_file_name("b", 3)), b"x").unwrap();
        // 删掉 002 之后按「文件数 + 1」会撞上已存在的 003
        assert_eq!(next_index(&dir), 4, "应取最大序号 + 1");
        std::fs::write(dir.join("无关文件.zip"), b"x").unwrap();
        assert_eq!(next_index(&dir), 4, "不同前缀的文件不该影响序号");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_whitelist_drops_network_fingerprint() {
        let payload = payload_of(&sample_account());
        assert_eq!(payload["uid"], "uid-123");
        assert_eq!(payload["access_token"], "tok");
        assert_eq!(payload["shared_by"], "workbuddy-switch");
        for leaked in [
            "egress_ip",
            "egress_at",
            "proxy",
            "risk_state",
            "last_used_at",
        ] {
            assert!(
                payload.get(leaked).is_none(),
                "分享包是分享账号，不是分享网络画像：{leaked} 不该进包"
            );
        }
    }

    #[test]
    fn validate_payload_rejects_incomplete_credentials() {
        assert!(validate_payload(&json!({"uid": "u", "access_token": "t"})).is_ok());
        assert!(
            validate_payload(&json!({"access_token": "t"})).is_err(),
            "缺 uid"
        );
        assert!(validate_payload(&json!({"uid": "u"})).is_err(), "缺 token");
        assert!(validate_payload(&json!("not an object")).is_err());
    }

    #[test]
    fn build_package_rejects_unknown_account() {
        let err = build_package("no-such-account-id", None).unwrap_err();
        assert!(err.contains("未找到账号"), "实际错误：{err}");
    }

    #[test]
    fn zip_round_trips_chinese_names_and_nested_paths() {
        let entries = vec![
            (BAT_FILE_NAME.to_string(), b"@echo off\r\n".to_vec()),
            (
                format!("{DATA_DIR_NAME}/{PAYLOAD_FILE_NAME}"),
                br#"{"uid":"u"}"#.to_vec(),
            ),
        ];
        let zip = build_zip(&entries, 1_760_000_000_000);
        assert_eq!(&zip[0..4], &[0x50, 0x4b, 0x03, 0x04], "缺本地文件头签名");
        assert!(
            zip.windows(4).any(|w| w == [0x50, 0x4b, 0x01, 0x02]),
            "缺中央目录"
        );
        assert!(
            zip.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]),
            "缺 EOCD"
        );
        assert_eq!(
            read_zip_entry(&zip, PAYLOAD_FILE_NAME).unwrap(),
            br#"{"uid":"u"}"#
        );
        assert_eq!(
            read_zip_entry(&zip, BAT_FILE_NAME).unwrap(),
            b"@echo off\r\n"
        );
    }

    #[test]
    fn zip_declares_utf8_file_names() {
        let zip = build_zip(
            &[(BAT_FILE_NAME.to_string(), b"x".to_vec())],
            1_760_000_000_000,
        );
        let flags = u16::from_le_bytes([zip[6], zip[7]]);
        assert_eq!(
            flags & 0x0800,
            0x0800,
            "中文文件名必须置 UTF-8 标志位，否则资源管理器解压会乱码"
        );
    }

    #[test]
    fn crc32_matches_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn read_zip_entry_reports_missing_and_corrupt() {
        let zip = build_zip(&[("a.txt".to_string(), b"a".to_vec())], 0);
        assert!(read_zip_entry(&zip, "b.txt")
            .unwrap_err()
            .contains("没有找到"));
        assert!(read_zip_entry(b"not a zip at all", "a.txt").is_err());
    }

    #[test]
    fn dos_datetime_encodes_epoch_and_stays_in_range() {
        let (time, date) = dos_datetime(0);
        assert_eq!(time, 0, "1970-01-01 00:00:00 的时间字段应为 0");
        assert_eq!(date, 33, "1970-01-01 → (0<<9)|(1<<5)|1");
        for ms in [1_000_000_000_000i64, 1_792_000_000_000, 2_000_000_000_000] {
            let (t, d) = dos_datetime(ms);
            let month = (d >> 5) & 0x0F;
            let day = d & 0x1F;
            assert!((1..=12).contains(&month), "月份越界：{month}");
            assert!((1..=31).contains(&day), "日期越界：{day}");
            let seconds = (t & 0x1F) * 2;
            assert!(
                seconds < 60,
                "DOS 时间字段把秒按 2 秒粒度存，不该溢出：{seconds}"
            );
        }
    }

    #[test]
    fn bat_is_utf8_ready_and_anchored_to_script_dir() {
        let bat = bat_content();
        assert!(bat.starts_with("@echo off\r\n"), "bat 必须是 CRLF 行尾");
        assert!(
            bat.contains("chcp 65001 >nul"),
            "必须切到 UTF-8 码页，否则中文路径与提示乱码"
        );
        assert!(bat.contains("%~dp0"), "必须按脚本自身位置定位数据目录");
        assert!(bat.contains(DATA_DIR_NAME));
        assert!(bat.contains("import-share"));
        assert_eq!(
            bat.matches('\n').count(),
            bat.matches("\r\n").count(),
            "每一行都必须是 CRLF，混进裸 LF 会让 cmd 解析出意外结果"
        );
    }

    #[test]
    fn read_payload_finds_json_in_data_dir_or_root() {
        let dir = temp_dir("share-read-data");
        let data = dir.join(DATA_DIR_NAME);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(
            data.join(PAYLOAD_FILE_NAME),
            br#"{"uid":"u1","access_token":"t1"}"#,
        )
        .unwrap();
        assert_eq!(read_payload(&dir).unwrap()["uid"], "u1");

        // 对方手工把 JSON 挪到根目录也要认
        let dir2 = temp_dir("share-read-root");
        std::fs::write(
            dir2.join(PAYLOAD_FILE_NAME),
            br#"{"uid":"u2","access_token":"t2"}"#,
        )
        .unwrap();
        assert_eq!(read_payload(&dir2).unwrap()["uid"], "u2");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn read_payload_reads_from_zip_and_validates() {
        let dir = temp_dir("share-read-zip");
        let path = dir.join("pkg.zip");
        let zip = build_zip(
            &[(
                format!("{DATA_DIR_NAME}/{PAYLOAD_FILE_NAME}"),
                br#"{"uid":"zu","access_token":"zt"}"#.to_vec(),
            )],
            0,
        );
        std::fs::write(&path, &zip).unwrap();
        assert_eq!(read_payload(&path).unwrap()["uid"], "zu");

        // 凭证不完整的包要在导入前就被拦住
        let bad = dir.join("bad.zip");
        let bad_zip = build_zip(
            &[(
                format!("{DATA_DIR_NAME}/{PAYLOAD_FILE_NAME}"),
                br#"{"uid":"zu"}"#.to_vec(),
            )],
            0,
        );
        std::fs::write(&bad, &bad_zip).unwrap();
        assert!(read_payload(&bad).unwrap_err().contains("access_token"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readme_and_version_mention_credentials_warning() {
        let readme = readme_content("张三", "2026-10-11 18:00:00");
        assert!(readme.contains(BAT_FILE_NAME), "说明里要写清双击哪个文件");
        assert!(readme.contains("请勿再转发"), "必须提醒不要把凭证再转出去");
        let version = version_file_content("0.1.60", "2026-10-11 18:00:00");
        assert!(version.contains("不包含生成者的出口 IP"));
    }
}
