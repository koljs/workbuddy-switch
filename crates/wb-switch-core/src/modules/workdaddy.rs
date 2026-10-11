//! WorkDaddy 加密导出的导入。
//!
//! WorkDaddy 是另一款 WorkBuddy 多账号管理器，它的「导出」是一个加密 JSON 信封，
//! 密码由用户自己设。本模块让用户能把那边的账号搬过来，不用一个个重新扫码。
//!
//! ## 包格式
//!
//! 逆向自 WBMultiLogin 的 `_workdaddy_open_export` / `_workdaddy_decrypt_binary`：
//!
//! ```json
//! {
//!   "wbsExport": "WorkDaddy",
//!   "version": 2,
//!   "exportType": "accounts",
//!   "data": "<base64>",
//!   "salt": "<base64>"
//! }
//! ```
//!
//! - `salt` 必须 16 字节；`data` 解 base64 后是 `nonce(12) ‖ ciphertext ‖ tag(16)`。
//! - `key = scrypt(password_utf8, salt, N=16384, r=8, p=1, dkLen=32)`，AES-256-GCM 解密。
//! - `version == 3` 的明文是 **gzip** 压缩的，`version == 2` 是裸 JSON。
//! - 明文里要求 `exportType == "WorkDaddy-accounts"`、`accounts` 是数组。
//! - `accounts` 的每一项是 `{ uid, info }`，**`info` 是一个 JSON 字符串**，
//!   里面才是账号本体（`uid` / `access_token` / `refresh_token` / `email` /
//!   `nickname` / `domain` / `enterprise_name` …）。
//!
//! ## scrypt 为什么是自己写的
//!
//! 为了不引入新依赖。`scrypt` crate 会带进 `salsa20` + `password-hash` 两个新包，而本仓库
//! 已经有 `pbkdf2`（默认带 hmac 特性）与 `sha2`——scrypt 就是「PBKDF2-HMAC-SHA256 +
//! Salsa20/8 的 ROMix」，把后者补上即可，约 120 行。
//!
//! 这不算「自己造密码学」：scrypt 是 RFC 7914 规定的算法，有官方测试向量，而
//! **向量 3 的参数（N=16384, r=8, p=1）与 WorkDaddy 用的完全一致**——测试里钉的就是它。
//! 与 Python `cryptography` 的 `Scrypt` 是否互通，由那条向量保证，不是靠猜。

use std::collections::HashSet;
use std::io::Read;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose, Engine as _};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::modules::{account, variant::WbVariant};

/// WorkDaddy 用的 scrypt 参数（= RFC 7914 测试向量 3 的参数）。
const SCRYPT_N: usize = 16384;
const SCRYPT_R: usize = 8;
const SCRYPT_P: usize = 1;

const ENVELOPE_TAG: &str = "WorkDaddy";
const PAYLOAD_EXPORT_TYPE: &str = "WorkDaddy-accounts";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
/// 与参考实现一致：导入文件上限 8 MB。
pub const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------- scrypt

/// Salsa20/8 core（RFC 7914 §3）。原地处理 64 字节。
fn salsa20_8(block: &mut [u8; 64]) {
    let mut orig = [0u32; 16];
    // 不用 `chunks_exact(4)`：新版 clippy 会拦「常量大小的 chunks_exact」，
    // 而 `array_chunks` 在 `&mut [u8; N]` 上的方法解析又不通。直接按 4 字节步进，
    // 既没有 lint 问题，也无需多想版本差异。
    for (i, slot) in orig.iter_mut().enumerate() {
        let at = i * 4;
        *slot = u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
    }
    let mut x = orig;
    // 8 轮 = 4 个「列轮 + 行轮」。
    for _ in 0..4 {
        // 列轮
        x[4] ^= x[0].wrapping_add(x[12]).rotate_left(7);
        x[8] ^= x[4].wrapping_add(x[0]).rotate_left(9);
        x[12] ^= x[8].wrapping_add(x[4]).rotate_left(13);
        x[0] ^= x[12].wrapping_add(x[8]).rotate_left(18);

        x[9] ^= x[5].wrapping_add(x[1]).rotate_left(7);
        x[13] ^= x[9].wrapping_add(x[5]).rotate_left(9);
        x[1] ^= x[13].wrapping_add(x[9]).rotate_left(13);
        x[5] ^= x[1].wrapping_add(x[13]).rotate_left(18);

        x[14] ^= x[10].wrapping_add(x[6]).rotate_left(7);
        x[2] ^= x[14].wrapping_add(x[10]).rotate_left(9);
        x[6] ^= x[2].wrapping_add(x[14]).rotate_left(13);
        x[10] ^= x[6].wrapping_add(x[2]).rotate_left(18);

        x[3] ^= x[15].wrapping_add(x[11]).rotate_left(7);
        x[7] ^= x[3].wrapping_add(x[15]).rotate_left(9);
        x[11] ^= x[7].wrapping_add(x[3]).rotate_left(13);
        x[15] ^= x[11].wrapping_add(x[7]).rotate_left(18);

        // 行轮
        x[1] ^= x[0].wrapping_add(x[3]).rotate_left(7);
        x[2] ^= x[1].wrapping_add(x[0]).rotate_left(9);
        x[3] ^= x[2].wrapping_add(x[1]).rotate_left(13);
        x[0] ^= x[3].wrapping_add(x[2]).rotate_left(18);

        x[6] ^= x[5].wrapping_add(x[4]).rotate_left(7);
        x[7] ^= x[6].wrapping_add(x[5]).rotate_left(9);
        x[4] ^= x[7].wrapping_add(x[6]).rotate_left(13);
        x[5] ^= x[4].wrapping_add(x[7]).rotate_left(18);

        x[11] ^= x[10].wrapping_add(x[9]).rotate_left(7);
        x[8] ^= x[11].wrapping_add(x[10]).rotate_left(9);
        x[9] ^= x[8].wrapping_add(x[11]).rotate_left(13);
        x[10] ^= x[9].wrapping_add(x[8]).rotate_left(18);

        x[12] ^= x[15].wrapping_add(x[14]).rotate_left(7);
        x[13] ^= x[12].wrapping_add(x[15]).rotate_left(9);
        x[14] ^= x[13].wrapping_add(x[12]).rotate_left(13);
        x[15] ^= x[14].wrapping_add(x[13]).rotate_left(18);
    }
    for i in 0..16 {
        let v = x[i].wrapping_add(orig[i]);
        block[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// BlockMix（RFC 7914 §4）。输入输出都是 `128*r` 字节。
///
/// 输出不是简单的顺序排列：偶数下标的结果写前半、奇数下标写后半。
fn block_mix(input: &[u8], output: &mut [u8], r: usize) {
    const BLOCK: usize = 64;
    let mut x = [0u8; BLOCK];
    x.copy_from_slice(&input[(2 * r - 1) * BLOCK..2 * r * BLOCK]);
    let mut y = [0u8; BLOCK];
    for i in 0..2 * r {
        for j in 0..BLOCK {
            y[j] = x[j] ^ input[i * BLOCK + j];
        }
        salsa20_8(&mut y);
        x = y;
        let dst = if i % 2 == 0 {
            (i / 2) * BLOCK
        } else {
            (r + i / 2) * BLOCK
        };
        output[dst..dst + BLOCK].copy_from_slice(&y);
    }
}

/// ROMix（RFC 7914 §5）。
fn ro_mix(block: &mut [u8], n: usize, r: usize) {
    let len = 128 * r;
    let mut x = block.to_vec();
    let mut tmp = vec![0u8; len];
    let mut v = vec![0u8; n * len];

    for i in 0..n {
        v[i * len..(i + 1) * len].copy_from_slice(&x);
        block_mix(&x, &mut tmp, r);
        x.copy_from_slice(&tmp);
    }
    for _ in 0..n {
        // Integerify：最后一个 64 字节块的前 4 字节，小端。
        let last = (2 * r - 1) * 64;
        let j = u32::from_le_bytes([x[last], x[last + 1], x[last + 2], x[last + 3]]) as usize % n;
        for k in 0..len {
            x[k] ^= v[j * len + k];
        }
        block_mix(&x, &mut tmp, r);
        x.copy_from_slice(&tmp);
    }
    block.copy_from_slice(&x);
}

/// scrypt（RFC 7914）。`out` 是派生密钥，长度即 dkLen。
pub fn scrypt(
    password: &[u8],
    salt: &[u8],
    n: usize,
    r: usize,
    p: usize,
    out: &mut [u8],
) -> Result<(), String> {
    if !n.is_power_of_two() || n < 2 {
        return Err("scrypt 的 N 必须是 2 的幂且不小于 2".to_string());
    }
    if r == 0 || p == 0 {
        return Err("scrypt 的 r / p 不能为 0".to_string());
    }
    let len = 128 * r;
    let mut b = vec![0u8; p * len];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, 1, &mut b);
    for i in 0..p {
        ro_mix(&mut b[i * len..(i + 1) * len], n, r);
    }
    pbkdf2::pbkdf2_hmac::<Sha256>(password, &b, 1, out);
    Ok(())
}

// ---------------------------------------------------------------- 包解析

fn decode_utf8_sig(bytes: &[u8]) -> Result<String, String> {
    // Python 侧是 `decode('utf-8-sig')`：容忍 UTF-8 BOM。
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(body.to_vec()).map_err(|_| "文件不是有效的 WorkDaddy 导出 JSON".to_string())
}

fn decode_b64(value: Option<&Value>) -> Result<Vec<u8>, String> {
    let raw = value.and_then(Value::as_str).unwrap_or("");
    general_purpose::STANDARD
        .decode(raw)
        .map_err(|_| "导出文件中的加密数据格式无效".to_string())
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|_| "导出文件解压失败，文件可能已损坏".to_string())?;
    Ok(out)
}

/// 解开 WorkDaddy 加密信封，返回 `accounts` 数组。
pub fn open_export(bytes: &[u8], password: &str) -> Result<Vec<Value>, String> {
    let password = password.trim();
    if password.is_empty() {
        return Err("密码不能为空".to_string());
    }
    if password.chars().count() > 1024 {
        return Err("密码不能超过 1024 个字符".to_string());
    }
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err("导入文件不能超过 8 MB".to_string());
    }

    let text = decode_utf8_sig(bytes)?;
    let envelope: Value = serde_json::from_str(&text)
        .map_err(|_| "文件不是有效的 WorkDaddy 导出 JSON".to_string())?;
    let Some(obj) = envelope.as_object() else {
        return Err("不是有效的 WorkDaddy 加密导出文件".to_string());
    };
    if obj.get("wbsExport").and_then(Value::as_str) != Some(ENVELOPE_TAG) {
        return Err("不是有效的 WorkDaddy 加密导出文件".to_string());
    }
    let version = obj.get("version").and_then(Value::as_i64).unwrap_or(0);
    if version != 2 && version != 3 {
        return Err("只支持 WorkDaddy v2 / v3 账号导出文件".to_string());
    }
    match obj.get("exportType") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s == "accounts" => {}
        _ => return Err("这不是 WorkDaddy 账号导出文件".to_string()),
    }

    let packed = decode_b64(obj.get("data"))?;
    let salt = decode_b64(obj.get("salt"))?;
    // `> 28`：至少要放下 12 字节 nonce + 16 字节 tag，外加 1 字节密文。
    if packed.len() <= 28 || salt.len() != SALT_LEN {
        return Err("导出文件缺少有效的加密数据".to_string());
    }

    let mut key = [0u8; 32];
    scrypt(
        password.as_bytes(),
        &salt,
        SCRYPT_N,
        SCRYPT_R,
        SCRYPT_P,
        &mut key,
    )?;

    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    let nonce = Nonce::from_slice(&packed[..NONCE_LEN]);
    let plain = cipher
        .decrypt(nonce, &packed[NONCE_LEN..])
        .map_err(|_| "解密失败：密码不对，或导出文件已损坏".to_string())?;

    let plain = if version == 3 { gunzip(&plain)? } else { plain };

    let payload: Value = serde_json::from_slice(&plain)
        .map_err(|_| "解密后的内容不是合法的账号 JSON".to_string())?;
    if payload.get("exportType").and_then(Value::as_str) != Some(PAYLOAD_EXPORT_TYPE) {
        return Err("导出文件类型不匹配".to_string());
    }
    let Some(accounts) = payload.get("accounts").and_then(Value::as_array) else {
        return Err("导入文件中没有账号数据".to_string());
    };
    Ok(accounts.clone())
}

/// 这条记录（或它的任意子节点）是否带 WorkBuddy 5.6 加密信封。
///
/// 对照 WBMultiLogin 的 `_contains_workbuddy_encrypted`：递归找 `$wbEncrypted`。
/// 这种包里的 token 是信封而不是明文，本模块**不**擅自解密——那是另一层密钥的事，
/// 猜错会把坏数据写进账号库。遇到就明确报错，让用户走别的路子。
pub fn contains_encrypted_envelope(node: &Value) -> bool {
    match node {
        Value::Object(map) => {
            if map.get("$wbEncrypted").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            map.values().any(contains_encrypted_envelope)
        }
        Value::Array(items) => items.iter().any(contains_encrypted_envelope),
        _ => false,
    }
}

fn pick_str(info: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| account::get_str(info, key))
}

/// 把 WorkDaddy 的 `info` 映射成本仓库的账号记录。
///
/// 字段名按本仓库 `auth_file::import_from_auth_file` 产出的形状对齐，这样后续
/// 切换 / 签到 / 积分等所有功能不需要知道这条记录是从 WorkDaddy 来的。
pub fn map_entry(info: &Value) -> Value {
    let domain = pick_str(info, &["domain"]);
    let variant = WbVariant::from_account(&json!({ "domain": domain }));
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "uid": pick_str(info, &["uid"]),
        "nickname": pick_str(info, &["nickname", "name"]),
        "email": pick_str(info, &["email"]),
        "variant": variant.as_str(),
        "enterpriseName": pick_str(info, &["enterprise_name", "enterpriseName"]),
        "enterpriseId": pick_str(info, &["enterprise_id", "enterpriseId"]),
        "access_token": pick_str(info, &["access_token", "accessToken"]),
        "refresh_token": pick_str(info, &["refresh_token", "refreshToken"]),
        "token_type": pick_str(info, &["token_type", "tokenType"])
            .unwrap_or_else(|| "Bearer".to_string()),
        "domain": domain,
        "expiresAt": info
            .get("expires_at")
            .or_else(|| info.get("expiresAt"))
            .cloned()
            .unwrap_or(Value::Null),
        "refreshExpiresAt": info
            .get("refresh_expires_at")
            .or_else(|| info.get("refreshExpiresAt"))
            .cloned()
            .unwrap_or(Value::Null),
        // 整份 info 原样留底：映射漏掉哪个字段都还能从 auth_raw 里找回来。
        "auth_raw": info,
        "profile_raw": info.get("profile").cloned().unwrap_or(Value::Null),
        "createdAt": crate::modules::config::now_ms(),
        "imported_from": "WorkDaddy",
    })
}

/// 导入 WorkDaddy 加密导出。
///
/// 返回 `{ imported, skipped, failed, total, innerLayer, accounts }`：
/// - `skipped`：uid 已在账号库里，**不动已有记录**（与「自动保存新号」同一条原则）。
/// - `failed`：逐条的错误文案，不中断整包——一个坏条目不该让其余账号都进不来。
/// - `innerLayer`：包里存在 WorkBuddy 5.6 信封，本模块不支持。
///
/// 键名用驼峰：与 `account_meta` 等下发到前端的结构保持同一套命名。
pub fn import_export(bytes: &[u8], password: &str) -> Result<Value, String> {
    let entries = open_export(bytes, password)?;

    let known: HashSet<String> = account::load_accounts()
        .iter()
        .filter_map(|existing| account::get_str(existing, "uid"))
        .collect();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut inner_layer = false;
    let mut added: Vec<Value> = Vec::new();

    for item in &entries {
        let uid = account::get_str(item, "uid").unwrap_or_default();
        let info_text = item.get("info").and_then(Value::as_str).unwrap_or("");
        if uid.is_empty() || info_text.trim().is_empty() {
            failed.push("导出条目缺少 uid 或 info".to_string());
            continue;
        }

        let info: Value = match serde_json::from_str(info_text) {
            Ok(value) => value,
            Err(_) => {
                failed.push(format!("{uid}: 条目里的 info 不是合法 JSON"));
                continue;
            }
        };
        if !info.is_object() {
            failed.push(format!("{uid}: 账号数据不是对象"));
            continue;
        }
        if contains_encrypted_envelope(&info) {
            inner_layer = true;
            failed.push(format!("{uid}: 账号数据是 WorkBuddy 加密信封，暂不支持"));
            continue;
        }

        let record = map_entry(&info);
        let record_uid = account::get_str(&record, "uid").unwrap_or_default();
        if record_uid != uid || account::get_str(&record, "access_token").is_none() {
            failed.push(format!("{uid}: uid 与 access token 校验失败"));
            continue;
        }
        if known.contains(&uid) {
            skipped += 1;
            continue;
        }

        match account::save_collected_account(record) {
            Ok(saved) => {
                added.push(account::account_meta(&saved));
                imported += 1;
            }
            Err(error) => failed.push(format!("{uid}: 写入账号库失败：{error}")),
        }
    }

    Ok(json!({
        "imported": imported,
        "skipped": skipped,
        "failed": failed,
        "total": entries.len(),
        "innerLayer": inner_layer,
        "accounts": added,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::generic_array::GenericArray as Ga;
    use aes_gcm::aead::Aead;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 7914 §12 的三条官方测试向量。
    ///
    /// **向量 3 的参数（N=16384, r=8, p=1）与 WorkDaddy 用的完全一致**，所以它同时
    /// 证明了「我们的 scrypt 与 Python `cryptography` 的 Scrypt 互通」——这是本模块
    /// 敢自己实现 scrypt 的全部底气。
    #[test]
    fn scrypt_matches_rfc7914_vectors() {
        // 向量 1：空口令、空盐
        let mut out = [0u8; 64];
        scrypt(b"", b"", 16, 1, 1, &mut out).unwrap();
        assert_eq!(
            hex(&out),
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442\
             fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906"
        );

        // 向量 2：N=1024, r=8, p=16
        let mut out = [0u8; 64];
        scrypt(b"password", b"NaCl", 1024, 8, 16, &mut out).unwrap();
        assert_eq!(
            hex(&out),
            "fdbabe1c9d3472007856e7190d01e9fe7c6ad7cbc8237830e77376634b373162\
             2eaf30d92e22a3886ff109279d9830dac727afb94a83ee6d8360cbdfa2cc0640"
        );

        // 向量 3：与 WorkDaddy 同参数
        let mut out = [0u8; 64];
        scrypt(b"pleaseletmein", b"SodiumChloride", 16384, 8, 1, &mut out).unwrap();
        assert_eq!(
            hex(&out),
            "7023bdcb3afd7348461c06cd81fd38ebfda8fbba904f8e3ea9b543f6545da1f2\
             d5432955613f0fcf62d49705242a9af9e61e85dc0d651e40dfcf017b45575887"
        );
    }

    #[test]
    fn scrypt_rejects_invalid_params() {
        let mut out = [0u8; 32];
        assert!(
            scrypt(b"p", b"s", 3, 8, 1, &mut out).is_err(),
            "N 非 2 的幂"
        );
        assert!(scrypt(b"p", b"s", 16, 0, 1, &mut out).is_err(), "r=0");
        assert!(scrypt(b"p", b"s", 16, 8, 0, &mut out).is_err(), "p=0");
    }

    /// 用同一套参数把明文打包成 WorkDaddy 信封（测试自己的夹具）。
    fn build_envelope(plain: &[u8], password: &str, version: i64, salt: &[u8; 16]) -> Vec<u8> {
        let mut key = [0u8; 32];
        scrypt(
            password.as_bytes(),
            salt,
            SCRYPT_N,
            SCRYPT_R,
            SCRYPT_P,
            &mut key,
        )
        .unwrap();
        let cipher = Aes256Gcm::new(Ga::from_slice(&key));
        let nonce = [7u8; 12];
        let body = if version == 3 {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut enc, plain).unwrap();
            enc.finish().unwrap()
        } else {
            plain.to_vec()
        };
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), body.as_slice())
            .unwrap();
        let mut packed = nonce.to_vec();
        packed.extend_from_slice(&ct);
        json!({
            "wbsExport": "WorkDaddy",
            "version": version,
            "exportType": "accounts",
            "data": general_purpose::STANDARD.encode(&packed),
            "salt": general_purpose::STANDARD.encode(salt),
        })
        .to_string()
        .into_bytes()
    }

    fn sample_payload() -> Vec<u8> {
        let info = json!({
            "uid": "u-1",
            "access_token": "TOKEN-1",
            "refresh_token": "REFRESH-1",
            "email": "a@example.com",
            "nickname": "阿一",
            "domain": "www.workbuddy.ai",
            "enterprise_name": "某公司",
            "expires_at": 1_700_000_000_000i64,
        });
        json!({
            "exportType": "WorkDaddy-accounts",
            "accounts": [
                { "uid": "u-1", "info": info.to_string() },
            ],
        })
        .to_string()
        .into_bytes()
    }

    /// v2（裸 JSON）与 v3（gzip）都能解开，且结果一致。
    #[test]
    fn open_export_round_trips_v2_and_v3() {
        let salt = [3u8; 16];
        for version in [2, 3] {
            let env = build_envelope(&sample_payload(), "pw-123", version, &salt);
            let accounts = open_export(&env, "pw-123").expect("应能解开 v{version}");
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0]["uid"], "u-1");
        }
    }

    /// 带 BOM 的文件也要能读（Python 侧是 `utf-8-sig`）。
    #[test]
    fn open_export_tolerates_utf8_bom() {
        let mut env = vec![0xEF, 0xBB, 0xBF];
        env.extend_from_slice(&build_envelope(&sample_payload(), "pw", 2, &[1u8; 16]));
        assert!(open_export(&env, "pw").is_ok());
    }

    /// 密码错必须报「密码不对」，而不是别的含糊错误。
    #[test]
    fn open_export_reports_wrong_password() {
        let env = build_envelope(&sample_payload(), "right", 2, &[2u8; 16]);
        let err = open_export(&env, "wrong").unwrap_err();
        assert!(err.contains("解密失败"), "实际：{err}");
    }

    /// 信封校验：来源标记 / 版本 / 导出类型。
    #[test]
    fn open_export_validates_envelope() {
        let salt = general_purpose::STANDARD.encode([0u8; 16]);
        let base = json!({
            "wbsExport": "WorkDaddy", "version": 2, "exportType": "accounts",
            "data": "AAAA", "salt": salt,
        });
        let with = |patch: Value| {
            let mut v = base.clone();
            for (k, val) in patch.as_object().unwrap() {
                v[k] = val.clone();
            }
            v.to_string().into_bytes()
        };

        assert!(open_export(&with(json!({"wbsExport": "Other"})), "p")
            .unwrap_err()
            .contains("不是有效的 WorkDaddy"));
        assert!(open_export(&with(json!({"version": 1})), "p")
            .unwrap_err()
            .contains("只支持"));
        assert!(open_export(&with(json!({"exportType": "settings"})), "p")
            .unwrap_err()
            .contains("不是 WorkDaddy 账号导出"));
        assert!(open_export(&with(json!({"salt": "AAAA"})), "p")
            .unwrap_err()
            .contains("缺少有效的加密数据"));
    }

    #[test]
    fn open_export_rejects_empty_password_and_oversize() {
        let env = build_envelope(&sample_payload(), "pw", 2, &[0u8; 16]);
        assert!(open_export(&env, "   ")
            .unwrap_err()
            .contains("密码不能为空"));
        assert!(open_export(&vec![0u8; MAX_IMPORT_BYTES + 1], "pw")
            .unwrap_err()
            .contains("不能超过 8 MB"));
    }

    /// 信封探测：`$wbEncrypted` 藏在多深都要能找到。
    #[test]
    fn detects_nested_encrypted_envelope() {
        assert!(!contains_encrypted_envelope(&json!({"a": 1})));
        assert!(contains_encrypted_envelope(
            &json!({"$wbEncrypted": true, "envelope": "x"})
        ));
        assert!(contains_encrypted_envelope(
            &json!({"a": [{"b": {"$wbEncrypted": true}}]})
        ));
        // 只有 true 才算
        assert!(!contains_encrypted_envelope(
            &json!({"$wbEncrypted": false})
        ));
    }

    /// 字段映射：WorkDaddy 的下划线命名要落到本仓库的驼峰字段上，
    /// 且 domain 决定档位。
    #[test]
    fn maps_workdaddy_fields_onto_our_record_shape() {
        let record = map_entry(&json!({
            "uid": "u-9",
            "access_token": "T",
            "refresh_token": "R",
            "token_type": "Bearer",
            "email": "b@example.com",
            "nickname": "阿九",
            "domain": "www.workbuddy.ai",
            "enterprise_name": "公司",
            "enterprise_id": "e-1",
            "expires_at": 123i64,
        }));
        assert_eq!(record["uid"], "u-9");
        assert_eq!(record["access_token"], "T");
        assert_eq!(record["refresh_token"], "R");
        assert_eq!(record["enterpriseName"], "公司");
        assert_eq!(record["enterpriseId"], "e-1");
        assert_eq!(record["expiresAt"], 123);
        assert_eq!(record["imported_from"], "WorkDaddy");
        // .workbuddy.ai 后缀 → 国际版
        assert_eq!(record["variant"], WbVariant::Ai.as_str());
        // 原样留底
        assert_eq!(record["auth_raw"]["nickname"], "阿九");

        // 国内域名 → 国内版；缺 token_type 时补 Bearer
        let cn =
            map_entry(&json!({"uid": "u-8", "access_token": "T", "domain": "www.codebuddy.cn"}));
        assert_eq!(cn["variant"], WbVariant::Cn.as_str());
        assert_eq!(cn["token_type"], "Bearer");
    }

    /// 坏条目要逐条记进 `failed`，不能中断整包。
    #[test]
    fn import_export_reports_per_entry_failures() {
        let payload = json!({
            "exportType": "WorkDaddy-accounts",
            "accounts": [
                { "uid": "ok-1", "info": json!({"uid": "ok-1", "access_token": "T"}).to_string() },
                { "uid": "", "info": "{}" },
                { "uid": "bad-json", "info": "{不是 JSON" },
                { "uid": "mismatch", "info": json!({"uid": "other", "access_token": "T"}).to_string() },
                { "uid": "envelope", "info": json!({"uid": "envelope", "access_token": {"$wbEncrypted": true}}).to_string() },
            ],
        })
        .to_string()
        .into_bytes();
        let env = build_envelope(&payload, "pw", 3, &[9u8; 16]);

        // 只验「解析 + 逐条分类」这一段，不落盘（落盘会写真实账号库）。
        let accounts = open_export(&env, "pw").unwrap();
        assert_eq!(accounts.len(), 5);

        let mut reasons: Vec<String> = Vec::new();
        let mut inner = false;
        for item in &accounts {
            let uid = account::get_str(item, "uid").unwrap_or_default();
            let info_text = item.get("info").and_then(Value::as_str).unwrap_or("");
            if uid.is_empty() || info_text.trim().is_empty() {
                reasons.push("缺少 uid 或 info".to_string());
                continue;
            }
            let Ok(info) = serde_json::from_str::<Value>(info_text) else {
                reasons.push("info 不是合法 JSON".to_string());
                continue;
            };
            if contains_encrypted_envelope(&info) {
                inner = true;
                reasons.push("加密信封".to_string());
                continue;
            }
            let record = map_entry(&info);
            if account::get_str(&record, "uid").unwrap_or_default() != uid
                || account::get_str(&record, "access_token").is_none()
            {
                reasons.push("uid 与 access token 校验失败".to_string());
            }
        }
        assert!(reasons.contains(&"缺少 uid 或 info".to_string()));
        assert!(reasons.contains(&"info 不是合法 JSON".to_string()));
        assert!(reasons.contains(&"uid 与 access token 校验失败".to_string()));
        assert!(reasons.contains(&"加密信封".to_string()));
        assert!(inner);
    }
}
