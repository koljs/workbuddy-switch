//! workbuddy-switch CLI：npm 安装形态的入口。
//!
//! ```bash
//! workbuddy-switch              # 启动本地服务 + 打开浏览器 webui
//! workbuddy-switch serve        # 只起服务不开浏览器（--port / --no-open）
//! workbuddy-switch status       # 终端输出当前账号
//! workbuddy-switch import-share # 导入「分享包」（双击即登录的 .bat 调的就是它）
//! workbuddy-switch version      # 版本号
//! ```

mod api;
mod gateway_api;
mod share_api;

use serde_json::json;

use wb_switch_core::modules::{
    account, auth_file, auto_save, checkin, config, process, rotate, share, travel, update,
    variant::WbVariant,
};

fn default_port() -> u16 {
    57890
}

/// 后台任务：自动签到启动即核验，之后按 core 计算的下一轮延迟睡眠（未设置
/// 签到时间段时固定 30 分钟）；自动轮换按配置间隔执行；自动保存新号每 45 秒扫一次；
/// 限额 hook 信号每秒轮询一次、启动时后台默认接入。
fn spawn_background_loops() {
    // 自动保存新号：登录态里出现账号库里没有的号就补进去（只增不改）。
    // 独立 spawn，与其他循环互不阻塞。
    tokio::spawn(auto_save::run_auto_save_cycle());

    tokio::spawn(async move {
        if let Err(error) = config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::StartupVerify).await;
        loop {
            tokio::time::sleep(checkin::next_cycle_delay()).await;
            let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::PeriodicRecovery).await;
        }
    });

    tokio::spawn(async move {
        let mut last_cycle_at: i64 = 0;
        loop {
            let cfg = config::load_auto_rotate_config();
            if cfg.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
                let interval_minutes = cfg
                    .get("check_interval_minutes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(5)
                    .max(1);
                let now = config::now_ms();
                if now - last_cycle_at >= interval_minutes * 60_000 {
                    last_cycle_at = now;
                    let _ = rotate::run_rotate_cycle().await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // 派猫猫旅行：启动即派发，之后周期性补派（并重试 no-buddy / 瞬时错误）。
    tokio::spawn(async move {
        let _ = travel::run_travel_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_RETRY_INTERVAL).await;
            let _ = travel::run_travel_cycle().await;
        }
    });

    // 旅行领取：启动立刻查一轮（避免重启后空等 15 分钟漏领），之后按周期检查。
    tokio::spawn(async move {
        let _ = travel::run_travel_claim_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_CLAIM_INTERVAL).await;
            let _ = travel::run_travel_claim_cycle().await;
        }
    });

    // 限额 hook 信号：轮询 `~/.wb-switch/hook-events.jsonl`，入账后由前端下次拉取可见。
    // webui 没有 Tauri 事件通道，因此不需要推送回调（桌面端见 src-tauri/src/lib.rs）。
    wb_switch_core::modules::rate_limit_events::spawn_watcher(|| {});

    // 默认接入：后台线程自动安装 hook（幂等、非阻塞、失败静默）；
    // 装上了就作废扫描缓存——扫描范围从全量收窄到「未注册的来源」。
    std::thread::spawn(|| {
        if wb_switch_core::modules::rate_limit_hook::auto_install_on_startup() {
            wb_switch_core::modules::limits::invalidate_scan_cache();
        }
    });
}

/// CLI 档位参数：`--variant ai` / `--variant=ai`；缺省国内版。
///
/// 与 Tauri 命令的可选 `variant` 参数、HTTP 路由的 query/body 字段同义。
fn variant_arg(args: &[String]) -> WbVariant {
    let raw = args.iter().enumerate().find_map(|(index, arg)| {
        if let Some(value) = arg.strip_prefix("--variant=") {
            return Some(value.to_string());
        }
        arg.eq("--variant")
            .then(|| args.get(index + 1).cloned().unwrap_or_default())
    });
    WbVariant::parse(raw.as_deref())
}

fn print_status(variant: WbVariant) {
    let auth = auth_file::read_auth_file(variant);
    let current = auth.as_ref().map(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        json!({
            "uid": account::display_value(&acct, "uid"),
            "nickname": account::display_value(&acct, "nickname"),
            "email": account::display_value(&acct, "email"),
        })
    });
    let running = process::is_workbuddy_running(variant);
    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("WorkBuddy 运行中: {}", if running { "是" } else { "否" });
    match current {
        Some(c) => {
            let name = c
                .get("nickname")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("email").and_then(|v| v.as_str()))
                .unwrap_or("未知");
            println!("当前账号: {name}");
        }
        None => println!("当前账号: 未登录"),
    }
    println!("账号数: {}", account::load_accounts().len());
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "status" => print_status(variant_arg(&args)),
        "import-share" => std::process::exit(import_share(&args)),
        "version" | "--version" | "-V" => {
            println!("workbuddy-switch {}", env!("CARGO_PKG_VERSION"));
        }
        _ => serve(&args).await,
    }
}

/// `workbuddy-switch import-share <分享包目录|zip> [--switch]`
///
/// 分享包里的 `Windows电脑点这个登录.bat` 就是调这条命令：把包内账号写进本机
/// WorkBuddy 的 auth 文件并（可选）切换过去。
///
/// 退出码必须如实反映成败——`.bat` 是靠 `%ERRORLEVEL%` 判断并打印「已完成 / 执行失败」
/// 的，这里吞掉错误会让对方看到「已完成」却什么都没发生。
fn import_share(args: &[String]) -> i32 {
    // 锚在子命令名上，而不是写死 `skip(2)`：`main` 传的是完整 argv（`args[0]` 是程序名，
    // 子命令在 `args[1]`），而本文件的测试按惯例只传「从子命令开始」的切片。
    // 写死下标会让后者把目标路径当成子命令吃掉，于是「路径不存在」被误报成「参数用错」。
    let target = args
        .iter()
        .skip_while(|arg| arg.as_str() != "import-share")
        .skip(1)
        .find(|arg| !arg.starts_with("--"))
        .cloned();
    let Some(target) = target else {
        eprintln!("用法：workbuddy-switch import-share <分享包目录|zip> [--switch]");
        return 2;
    };
    let switch = args.iter().any(|arg| arg == "--switch");
    match share::import_package(std::path::Path::new(&target), switch) {
        Ok(result) => {
            let uid = result.get("uid").and_then(|v| v.as_str()).unwrap_or("");
            let name = result
                .get("account")
                .and_then(|meta| meta.get("nickname").or_else(|| meta.get("email")))
                .and_then(|v| v.as_str())
                .unwrap_or("未知");
            println!("[WorkBuddy] 已导入账号：{name}（uid {uid}）");
            if result.get("switched").and_then(|v| v.as_bool()) == Some(true) {
                println!("[WorkBuddy] 已切换到该账号，WorkBuddy 正在重启。");
            } else if switch {
                println!(
                    "[WorkBuddy] 凭证已写入，但自动切换未完成，请打开 workbuddy-switch 手动切换。"
                );
            } else {
                println!("[WorkBuddy] 凭证已写入。需要立即登录请加 --switch。");
            }
            0
        }
        Err(error) => {
            eprintln!("[失败] {error}");
            1
        }
    }
}

async fn serve(args: &[String]) {
    let mut port = default_port();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            port = p;
        }
    }

    let app = api::router();
    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("启动失败: 端口 {port} 被占用或不可用（{e}）。可用 --port 指定其他端口。");
            std::process::exit(1);
        }
    };

    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("webui: http://{addr}");
    println!("按 Ctrl+C 停止服务。");

    let no_open = args.iter().any(|a| a == "--no-open");
    if !no_open {
        open_browser(&addr);
    }

    spawn_background_loops();

    axum::serve(listener, app).await.unwrap();
}

fn open_browser(addr: &str) {
    let url = format!("http://{addr}");
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：开浏览器不闪 cmd 窗
        }
        let _ = c.args(["/C", "start", &url]).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::{import_share, variant_arg};
    use wb_switch_core::modules::variant::WbVariant;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 分享包的 `.bat` 靠退出码判断成败，缺参数必须是失败码而不是 panic。
    #[test]
    fn import_share_without_target_reports_usage() {
        assert_eq!(import_share(&args(&["import-share"])), 2);
        assert_eq!(import_share(&args(&["import-share", "--switch"])), 2);
    }

    /// 目标不存在时给失败码（1），不能是 2——2 专指「参数用错了」。
    #[test]
    fn import_share_with_missing_package_fails_cleanly() {
        let missing = std::env::temp_dir().join(format!("wb-share-missing-{}", std::process::id()));
        assert_eq!(
            import_share(&args(&["import-share", &missing.to_string_lossy()])),
            1
        );
    }

    /// 解析必须对两种 argv 形态都成立：`main` 传完整 argv（含程序名），
    /// 测试传从子命令开始的切片。二者混淆过一次（`skip(2)`），
    /// 结果是把目标路径当子命令吃掉，把「路径不存在」误报成「参数用错」。
    #[test]
    fn import_share_parses_target_with_or_without_program_name() {
        let missing = std::env::temp_dir().join(format!("wb-share-argv-{}", std::process::id()));
        let path = missing.to_string_lossy().to_string();
        // 带程序名（真实调用形态）
        assert_eq!(
            import_share(&args(&["wb-switch.exe", "import-share", &path])),
            1
        );
        // 不带程序名（测试切片形态）
        assert_eq!(import_share(&args(&["import-share", &path])), 1);
        // 两种形态下漏参数都要落到「参数用错」
        assert_eq!(import_share(&args(&["wb-switch.exe", "import-share"])), 2);
    }

    /// 不传档位时必须仍是国内版（改造前行为）。
    #[test]
    fn cli_variant_defaults_to_cn() {
        assert_eq!(variant_arg(&args(&["status"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "--debug"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "--variant"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "cn"])), WbVariant::Cn);
        assert_eq!(
            variant_arg(&args(&["status", "--variant=cn"])),
            WbVariant::Cn
        );
    }

    #[test]
    fn cli_variant_reads_space_and_equals_forms() {
        assert_eq!(
            variant_arg(&args(&["status", "--variant", "ai"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["status", "--variant=ai"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["status", "--variant", "AI", "--no-open"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["--variant=ai", "status"])),
            WbVariant::Ai
        );
    }
}
