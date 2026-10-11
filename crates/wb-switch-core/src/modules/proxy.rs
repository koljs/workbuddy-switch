//! 账号级 HTTP 代理：归一化地址 + 连通性自检。
//!
//! 为什么需要它：上游风控会把「同一个出口 IP 短时间登录/使用大量账号」当成异常
//! 特征。给账号分别挂上不同出口的代理，就能把「一号一 IP」这件事变成可配置的，
//! 而不是只能靠整机换网络。这是与 [`crate::modules::egress`]（检测当前出口 IP）
//! 配套的另一半：egress 告诉你现在从哪出去，proxy 决定每个号从哪出去。
//!
//! 只支持 `http://` / `https://` 代理：`reqwest` 在本项目里以
//! `default-features = false, features = ["json", "rustls-tls"]` 构建，没有开
//! `socks`，`socks5://` 会在建连时报错。与其让用户对着一个含混的底层错误排查，
//! 不如在归一化阶段就明确拒绝。
//!
//! 代理凭据（`http://user:pass@host:port`）原样保留并随账号落库——它等同于该账号
//! 的出口身份，属于敏感信息，界面上按密码字段展示，不进日志。

use serde_json::{json, Value};

use crate::modules::config::http_request_with_proxy;

/// 归一化后的代理地址。空串代表「直连」。
pub type Normalized = Option<String>;

/// 校验并归一化代理地址。
///
/// 接受三种写法，统一补成带 scheme 的形式：
/// - `http://127.0.0.1:7890` → 原样
/// - `127.0.0.1:7890`        → 补成 `http://127.0.0.1:7890`
/// - 空 / 纯空白             → `Ok(None)`，表示直连（清空代理走这条）
///
/// 拒绝：未知 scheme（含 `socks5://`）、缺端口、端口非数字或越界、缺主机。
pub fn normalize(raw: &str) -> Result<Normalized, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Ok(None);
    }

    let (scheme, rest) = match text.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        // 没写 scheme 时按 http 处理：绝大多数本地代理（Clash/v2ray 等）都是 http 口。
        None => ("http".to_string(), text),
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "只支持 http:// 或 https:// 代理，收到 {scheme}://（socks5 需要另装支持 socks 的构建）"
        ));
    }

    // 先剥掉 user:pass@，再校验 host:port——否则用户名里的 `:` 会被误当端口分隔符。
    let (auth, host_port) = match rest.rsplit_once('@') {
        Some((auth, host_port)) => (Some(auth), host_port),
        None => (None, rest),
    };
    if host_port.is_empty() {
        return Err("代理地址缺少主机名".to_string());
    }

    // IPv6 字面量是 [::1]:7890 形态，端口分隔符取最后一个 `]` 之后的那个冒号。
    let (host, port_text) = if let Some(close) = host_port.rfind(']') {
        let after = &host_port[close + 1..];
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| "IPv6 代理地址缺少端口，应写成 [::1]:7890".to_string())?;
        (&host_port[..close + 1], port)
    } else {
        host_port
            .rsplit_once(':')
            .ok_or_else(|| "代理地址缺少端口，应写成 host:port".to_string())?
    };

    if host.is_empty() {
        return Err("代理地址缺少主机名".to_string());
    }
    let port: u32 = port_text
        .trim()
        .parse()
        .map_err(|_| format!("代理端口不是数字：{port_text}"))?;
    if port == 0 || port > 65535 {
        return Err(format!("代理端口超出范围：{port}"));
    }
    if let Some(auth) = auth {
        if auth.trim().is_empty() {
            return Err("代理认证信息为空（`@` 前没有内容）".to_string());
        }
    }

    Ok(Some(format!("{scheme}://{rest}")))
}

/// 掩掉代理地址里的密码，用于界面回显与日志。
///
/// `http://u:p@h:1` → `http://u:***@h:1`；无认证信息时原样返回。
pub fn mask(raw: &str) -> String {
    let text = raw.trim();
    let Some((scheme, rest)) = text.split_once("://") else {
        return mask_auth(text);
    };
    format!("{scheme}://{}", mask_auth(rest))
}

fn mask_auth(rest: &str) -> String {
    let Some((auth, host_port)) = rest.rsplit_once('@') else {
        return rest.to_string();
    };
    match auth.split_once(':') {
        Some((user, _)) => format!("{user}:***@{host_port}"),
        None => format!("{auth}:***@{host_port}"),
    }
}

/// 用哪个地址做自检：优先带认证信息的那一份（认证错了要能测出来）。
fn probe_url() -> &'static str {
    "https://api.ipify.org?format=json"
}

/// 代理连通性自检：经该代理访问一个轻量公网接口。
///
/// 返回结构固定，界面直接渲染：
/// - `ok`       是否连通
/// - `proxy`    掩码后的代理地址（**不回显明文密码**）
/// - `latencyMs` 往返毫秒（失败时为 `null`）
/// - `ip`       该代理看到的出口 IP（连通时才有；可直接填进账号的出口 IP 字段）
/// - `message`  给人看的一句话
///
/// 三种失败分开报，因为处置方式完全不同：地址不合法（改配置）、连不上（代理没开/
/// 端口错）、连上了但对方拒绝（代理要求认证或不允许外网）。
pub async fn test(raw: &str) -> Value {
    let proxy = match normalize(raw) {
        Ok(Some(proxy)) => proxy,
        Ok(None) => {
            return json!({
                "ok": false,
                "proxy": "",
                "latencyMs": Value::Null,
                "ip": Value::Null,
                "message": "代理地址为空（空=直连，不需要测代理）",
            })
        }
        Err(message) => {
            return json!({
                "ok": false,
                "proxy": raw.trim(),
                "latencyMs": Value::Null,
                "ip": Value::Null,
                "message": format!("代理地址不合法：{message}"),
            })
        }
    };

    let started = std::time::Instant::now();
    let resp = http_request_with_proxy(probe_url(), "GET", None, None, Some(&proxy)).await;
    let elapsed = started.elapsed().as_millis() as i64;
    let masked = mask(&proxy);

    // http_request_with_proxy 用 `code = -1` 表示网络层失败（连不上代理、TLS 失败等）。
    let code = resp.get("code").and_then(|v| v.as_i64());
    if code == Some(-1) {
        let reason = resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知原因");
        return json!({
            "ok": false,
            "proxy": masked,
            "latencyMs": Value::Null,
            "ip": Value::Null,
            "message": format!("连不上代理（确认代理已启动、端口正确）：{reason}"),
        });
    }

    let ip = resp
        .get("ip")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match ip {
        Some(ip) => json!({
            "ok": true,
            "proxy": masked,
            "latencyMs": elapsed,
            "ip": ip,
            "message": format!("代理可用，出口 IP {ip}"),
        }),
        // 连上了但拿不到 IP：多半是代理返回了自己的错误页（认证失败 / 禁止外网）。
        None => {
            let detail = resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("响应里没有 ip 字段");
            json!({
                "ok": false,
                "proxy": masked,
                "latencyMs": elapsed,
                "ip": Value::Null,
                "message": format!("代理有响应但请求未成功（可能需要认证或不允许访问外网）：{detail}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_direct() {
        assert_eq!(normalize("").unwrap(), None);
        assert_eq!(normalize("   ").unwrap(), None);
    }

    #[test]
    fn bare_host_port_gets_http_scheme() {
        assert_eq!(
            normalize("127.0.0.1:7890").unwrap(),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            normalize(" proxy.example.com:8080 ").unwrap(),
            Some("http://proxy.example.com:8080".to_string())
        );
    }

    #[test]
    fn explicit_scheme_is_kept() {
        assert_eq!(
            normalize("https://proxy.example.com:443").unwrap(),
            Some("https://proxy.example.com:443".to_string())
        );
        // scheme 大小写不敏感，但归一化后统一小写。
        assert_eq!(
            normalize("HTTP://127.0.0.1:7890").unwrap(),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn socks_is_rejected_with_readable_reason() {
        let err = normalize("socks5://127.0.0.1:1080").unwrap_err();
        assert!(err.contains("只支持 http"), "实际: {err}");
    }

    #[test]
    fn missing_port_is_rejected() {
        let err = normalize("127.0.0.1").unwrap_err();
        assert!(err.contains("缺少端口"), "实际: {err}");
    }

    #[test]
    fn port_must_be_numeric_and_in_range() {
        assert!(normalize("127.0.0.1:abc").unwrap_err().contains("不是数字"));
        assert!(normalize("127.0.0.1:0").unwrap_err().contains("超出范围"));
        assert!(normalize("127.0.0.1:70000")
            .unwrap_err()
            .contains("超出范围"));
    }

    #[test]
    fn credentials_survive_normalization() {
        assert_eq!(
            normalize("http://user:p%40ss@proxy.example.com:8080").unwrap(),
            Some("http://user:p%40ss@proxy.example.com:8080".to_string())
        );
    }

    #[test]
    fn ipv6_literal_keeps_brackets() {
        assert_eq!(
            normalize("[::1]:7890").unwrap(),
            Some("http://[::1]:7890".to_string())
        );
        assert!(normalize("[::1]").unwrap_err().contains("缺少端口"));
    }

    #[test]
    fn mask_hides_password_only() {
        assert_eq!(mask("http://u:secret@h:1"), "http://u:***@h:1");
        assert_eq!(mask("http://h:1"), "http://h:1");
        // 只有用户名没有密码时，补一个 ***，避免把用户名当密码泄露的错觉。
        assert_eq!(mask("http://u@h:1"), "http://u:***@h:1");
        // 没有 scheme 也要能掩。
        assert_eq!(mask("u:p@h:1"), "u:***@h:1");
    }

    #[test]
    fn empty_auth_is_rejected() {
        assert!(normalize("http://@h:1")
            .unwrap_err()
            .contains("认证信息为空"));
    }
}
