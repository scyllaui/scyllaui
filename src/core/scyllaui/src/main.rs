//! `scyllaui` 命令行：`scyllaui hook --config <claude|codex|kimi|grok|opencode|copilot|antigravity> [--event <事件名>]`（Hook 桥）、
//! `scyllaui ctl`（调试，docs/protocol.md 第 9 节）、`scyllaui hooks`（四款 CLI 的 Hook 配置：状态、改动、安装与一键撤销，
//! M1 第 7 块）、`scyllaui statusline` 与 `scyllaui statusline-bridge`（Claude 的 statusline 桥，M2 第 3 块）
//! 与 `scyllaui attach --terminal`：保活终端的窗格启动器（M3，见 pane_attach.rs）。
//! Hook 桥约束（docs/design.md 4.4 节）：发送超时 100 ms，永远 exit 0，不往 stdout 写任何东西。

mod pane_attach;

use scyllaui_proto::adapters::{adapter, ANCESTOR_LEVELS};
use scyllaui_proto::hookcfg;
use scyllaui_proto::statusline::{Bridge, State as BridgeState};
use scyllaui_proto::{default_data_dir, hook_env_report, is_grok_env, now_us, parse_procargs};
use scyllaui_proto::{
    resolve_source, socket_path, Tool, BUILD_ID, MAX_PAYLOAD_BYTES, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

const STDIN_TIMEOUT: Duration = Duration::from_millis(1000);
const SEND_TIMEOUT: Duration = Duration::from_millis(100);

fn main() {
    let t0 = now_us();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => {
            // 出错一律静默放行，不影响 CLI 自己的流程。
            let _ = hook(t0, &args[1..]);
        }
        Some("ctl") => std::process::exit(ctl(&args[1..])),
        Some("hooks") => std::process::exit(hooks(&args[1..])),
        Some("attach") => std::process::exit(pane_attach::run(&args[1..])),
        Some("statusline") => statusline(),
        Some("statusline-bridge") => std::process::exit(statusline_bridge(&args[1..])),
        _ => {
            eprintln!("用法：scyllaui hook --config <claude|codex|kimi|grok|opencode|copilot|antigravity> [--event <事件名>] | scyllaui ctl <snapshot|recent|shutdown|close> | scyllaui hooks <status|diff|install|uninstall> [--home <目录>] | scyllaui statusline | scyllaui statusline-bridge <status|diff|install|uninstall> [--home <目录>] | scyllaui attach --terminal <ID>");
            std::process::exit(2);
        }
    }
}

/// 以 cli 角色连接 scyllauid，发一条请求并打印应答。socket 优先取 `SCYLLAUI_SOCK`，否则用默认路径。
fn ctl(args: &[String]) -> i32 {
    const USAGE: &str = "用法：scyllaui ctl snapshot | recent [--project <ID>] [--limit <N>] | shutdown [--force] | close <终端 ID> [--confirmed]";
    let (mut req, rest) = match (args.first().map(String::as_str), args.get(1)) {
        (Some("snapshot"), _) => (json!({"type": "snapshot.get"}), &args[1..]),
        (Some("recent"), _) => (json!({"type": "recent.query"}), &args[1..]),
        (Some("shutdown"), _) => (json!({"type": "shutdown"}), &args[1..]),
        // M3：关闭终端（保活终端在光标不在提示符时要带 --confirmed），供测试收尾。
        (Some("close"), Some(id)) => (json!({"type": "terminal.close", "terminal_id": id}), &args[2..]),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--force" if req["type"] == "shutdown" => req["force"] = json!(true),
            "--confirmed" if req["type"] == "terminal.close" => req["confirmed"] = json!(true),
            "--project" | "--limit" if req["type"] == "recent.query" => {
                let Some(v) = it.next().and_then(|v| v.parse::<i64>().ok()) else {
                    eprintln!("{USAGE}");
                    return 2;
                };
                req[if flag == "--project" { "project_id" } else { "limit" }] = json!(v);
            }
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    req["id"] = json!(1);
    let sock = match std::env::var("SCYLLAUI_SOCK").ok().filter(|s| !s.is_empty()) {
        Some(s) => s.into(),
        None => match default_data_dir() {
            Some(dir) => socket_path(&dir),
            None => {
                eprintln!("scyllaui ctl：无法确定 socket 路径（HOME 未设置）");
                return 1;
            }
        },
    };
    let hello = json!({"type": "hello", "role": "cli", "protocol_version": PROTOCOL_VERSION, "build_id": BUILD_ID});
    let result = (|| -> std::io::Result<Value> {
        let mut stream = UnixStream::connect(&sock)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        writeln!(stream, "{hello}\n{req}")?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let greeting: Value = serde_json::from_str(&line).map_err(std::io::Error::other)?;
        if greeting["type"] != "hello_ok" {
            return Ok(greeting);
        }
        line.clear();
        reader.read_line(&mut line)?;
        serde_json::from_str(&line).map_err(std::io::Error::other)
    })();
    match result {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            if v["ok"] == true {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("scyllaui ctl：连接 {} 失败：{e}", sock.display());
            1
        }
    }
}

/// 直接读写四款 CLI 的配置文件，不经后台（docs/protocol.md 第 9 节）。规则在 `scyllaui_proto::hookcfg`，
/// 与侧栏的一键修复相同；`uninstall` 只删本项目的条目，是一键撤销。写入前备份 `<文件>.scyllaui-bak-<时间>`。
fn hooks(args: &[String]) -> i32 {
    const USAGE: &str = "用法：scyllaui hooks <status|diff|install|uninstall> [--home <目录>]";
    let action = args.first().map(String::as_str).unwrap_or("");
    let home = match &args[1.min(args.len())..] {
        [] => std::env::var("HOME").ok(),
        [flag, dir] if flag == "--home" => Some(dir.clone()),
        _ => None,
    };
    let (Some(home), true) = (home, matches!(action, "status" | "diff" | "install" | "uninstall")) else {
        eprintln!("{USAGE}");
        return 2;
    };
    if action == "status" {
        for tool in hookcfg::TOOLS {
            let s = hookcfg::analyze(&hookcfg::read(tool, &home));
            println!("{:8} {:10} {}  {}", tool.as_str(), s.state.as_str(), s.path, s.problems.join("、"));
        }
        return 0;
    }
    let kind = if action == "uninstall" { hookcfg::Action::Uninstall } else { hookcfg::Action::Repair };
    let mut plans = vec![];
    let mut failed = false;
    for tool in hookcfg::TOOLS {
        match hookcfg::plan(&hookcfg::read(tool, &home), kind) {
            Ok(Some(p)) => plans.push(p),
            Ok(None) => println!("== {}：无改动", tool.as_str()),
            Err(e) => {
                eprintln!("== {}：不处理（{e}）", tool.as_str());
                failed = true;
            }
        }
    }
    for p in &plans {
        println!("== {}：{}", p.tool.as_str(), p.actions.join("；"));
        print!("{}", p.diff());
    }
    if action == "diff" || plans.is_empty() {
        return i32::from(failed);
    }
    match hookcfg::apply(&plans, &hookcfg::local_stamp()) {
        Ok(backups) => {
            for p in plans.iter().flat_map(|p| &p.files) {
                println!("{action}: {}", p.path.display());
            }
            for b in backups {
                println!("备份: {}", b.display());
            }
            i32::from(failed)
        }
        Err(e) => {
            eprintln!("scyllaui hooks {action} 失败：{e}");
            1
        }
    }
}

/// 安装、查看与撤销 Claude 的 statusline 桥（docs/protocol.md 第 9 节）：直接读写 `<home>/.claude/settings.json`，
/// 规则在 `scyllaui_proto::statusline`，写入同 `scyllaui hooks`（备份、核对指纹、临时文件改名）。
fn statusline_bridge(args: &[String]) -> i32 {
    const USAGE: &str = "用法：scyllaui statusline-bridge <status|diff|install|uninstall> [--home <目录>]";
    let action = args.first().map(String::as_str).unwrap_or("");
    let home = match &args[1.min(args.len())..] {
        [] => std::env::var("HOME").ok(),
        [flag, dir] if flag == "--home" => Some(dir.clone()),
        _ => None,
    };
    let (Some(home), true) = (home, matches!(action, "status" | "diff" | "install" | "uninstall")) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let bridge = match Bridge::read(std::path::Path::new(&home)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("scyllaui statusline-bridge：{e}");
            return 1;
        }
    };
    if action == "status" {
        return match bridge.state() {
            Ok(BridgeState::Installed { orig }) => {
                println!("已安装  {}
原命令：{orig}", bridge.path.display());
                0
            }
            Ok(BridgeState::Plain { command }) => {
                println!("未安装  {}
当前命令：{command}", bridge.path.display());
                0
            }
            Ok(BridgeState::None) => {
                println!("未安装  {}（没有配置 statusLine）", bridge.path.display());
                0
            }
            Err(e) => {
                println!("无法处理  {}  {e}", bridge.path.display());
                1
            }
        };
    }
    let plan = match bridge.plan(action != "uninstall") {
        Ok(Some(p)) => p,
        Ok(None) => {
            println!("无改动");
            return 0;
        }
        Err(e) => {
            eprintln!("不处理（{e}）");
            return 1;
        }
    };
    println!("== {}", plan.actions.join("；"));
    print!("{}", plan.diff());
    if action == "diff" {
        return 0;
    }
    match hookcfg::apply(std::slice::from_ref(&plan), &hookcfg::local_stamp()) {
        Ok(backups) => {
            println!("{action}: {}", bridge.path.display());
            for b in backups {
                println!("备份: {}", b.display());
            }
            0
        }
        Err(e) => {
            eprintln!("scyllaui statusline-bridge {action} 失败：{e}");
            1
        }
    }
}

/// Claude 的 statusline 桥运行时（docs/design.md 第 5 节「实现（M2 第 3 块）」）：stdin 先原样写到 stdout 并关闭，
/// 交给管道后面的原命令；不超过 1 MiB 时再把 `session_id`、`model`、`rate_limits` 发给后台（超时 100 ms）。永远 exit 0。
fn statusline() {
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let _ = (&mut stdin).take(MAX_PAYLOAD_BYTES as u64 + 1).read_to_end(&mut buf);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&buf);
    let whole = buf.len() <= MAX_PAYLOAD_BYTES;
    if !whole {
        let _ = std::io::copy(&mut stdin, &mut out);
    }
    let _ = out.flush();
    drop(out);
    // SAFETY: 关闭标准输出，让原命令读到 EOF；之后不再写 stdout。
    unsafe { libc::close(1) };
    if whole {
        let _ = send_statusline(&buf);
    }
}

fn send_statusline(buf: &[u8]) -> Option<()> {
    let sock = std::env::var("SCYLLAUI_SOCK").ok().filter(|s| !s.is_empty())?;
    let v: Value = serde_json::from_slice(buf).ok()?;
    let mut payload = serde_json::Map::new();
    // `context_window`：右侧栏「状态」的上下文占用（2026-11-23）。
    for k in ["session_id", "model", "rate_limits", "context_window"] {
        if let Some(x) = v.get(k) {
            payload.insert(k.to_owned(), x.clone());
        }
    }
    let frame = json!({"type": "statusline", "source": "claude", "payload": payload});
    let hello = json!({"type": "hello", "role": "hook", "protocol_version": PROTOCOL_VERSION, "build_id": BUILD_ID});
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_write_timeout(Some(SEND_TIMEOUT)).ok()?;
    stream.write_all(format!("{hello}\n{frame}\n").as_bytes()).ok()
}

fn hook(t0: u64, args: &[String]) -> Option<()> {
    // `--event <名>`：载荷里没有事件名的 CLI（Copilot、Antigravity，2026-10-30 实测）由 Hook 命令带上，写进载荷的
    // `hook_event_name`（`set_event`）。
    let (config, event) = match args {
        [flag, name] if flag == "--config" => (Tool::parse(name)?, None),
        [flag, name, ev_flag, ev] if flag == "--config" && ev_flag == "--event" => (Tool::parse(name)?, Some(ev.clone())),
        _ => return None,
    };
    let sock = std::env::var("SCYLLAUI_SOCK")
        .ok()
        .filter(|s| !s.is_empty())?;
    let payload = read_stdin()?;
    // 解析不了也照样发帧（payload 为 null、不带原文），后台据此提示「状态可能不准」（M4b 块 6，docs/protocol.md 第 6 节）。
    let mut payload: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
    if let Some(ev) = event {
        set_event(&mut payload, ev);
    }
    let (source, dropped) = resolve_source(config, is_grok_env(|k| std::env::var(k).ok()));
    // 是否由 CLI 的守护进程执行（docs/design.md 4.4 节）：只上报判断结果，不上报进程参数。
    let hosted = adapter(source).hosted.is_some().then(|| {
        adapter(source).hosted_in(|k| std::env::var_os(k).is_some(), &ancestor_args(ANCESTOR_LEVELS))
    });

    let frame = json!({
        "type": "hook",
        "config": config.as_str(),
        "source": source.as_str(),
        "dropped": dropped,
        "pane_id": std::env::var("SCYLLAUI_PANE_ID").ok(),
        "hook_t0_us": t0,
        "hook_send_us": now_us(),
        // 由 scyllauid 按 source 选用适配器规范化（docs/protocol.md 第 6 节）；只在指定采集目录时落盘。
        "payload": payload,
        "env": hook_env_report(std::env::vars()),
        "hosted": hosted,
    });
    let hello = json!({"type": "hello", "role": "hook", "protocol_version": PROTOCOL_VERSION, "build_id": BUILD_ID});

    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_write_timeout(Some(SEND_TIMEOUT)).ok()?;
    let mut buf = Vec::with_capacity(4096);
    serde_json::to_writer(&mut buf, &hello).ok()?;
    buf.push(b'\n');
    serde_json::to_writer(&mut buf, &frame).ok()?;
    buf.push(b'\n');
    stream.write_all(&buf).ok()
}

/// 最多 `levels` 层上级进程的参数（从父进程起）。macOS：`proc_pidinfo` 取父进程号，`KERN_PROCARGS2` 取参数。
fn ancestor_args(levels: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    // SAFETY: getppid 无参数、总是成功。
    let mut pid = unsafe { libc::getppid() };
    while out.len() < levels && pid > 1 {
        out.push(proc_args(pid));
        match parent_of(pid) {
            Some(p) => pid = p,
            None => break,
        }
    }
    out
}

fn parent_of(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: 缓冲区为 proc_bsdinfo 大小，与传入的长度一致。
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
    };
    (n == size).then_some(info.pbi_ppid as i32)
}

fn proc_args(pid: i32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: 先取所需大小，再按该大小分配缓冲区读取。
    let ok = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
    if ok != 0 || size < 4 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    let ok = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0)
    };
    if ok != 0 {
        return Vec::new();
    }
    parse_procargs(&buf[..size])
}

/// 读完 stdin；超时或超过上限则放弃。读取放在线程里，主线程超时后直接退出进程。
fn read_stdin() -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let ok = std::io::stdin()
            .take(MAX_PAYLOAD_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .is_ok();
        let _ = tx.send(if ok && buf.len() <= MAX_PAYLOAD_BYTES {
            Some(buf)
        } else {
            None
        });
    });
    rx.recv_timeout(STDIN_TIMEOUT).ok().flatten()
}

/// 以 Hook 配置里登记的事件名为准，覆盖载荷自带的：Copilot 1.0.105 只有 notification 的载荷自带
/// `"hook_event_name": "Notification"`（与 Claude 同名的大写写法），其余事件都不带（2026-10-30 并行回归发现）。
fn set_event(payload: &mut Value, ev: String) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("hook_event_name".into(), Value::String(ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_flag_overrides_payload_name() {
        let mut p = serde_json::json!({"sessionId": "s1", "hook_event_name": "Notification", "notification_type": "permission_prompt"});
        set_event(&mut p, "notification".into());
        assert_eq!(p["hook_event_name"], "notification");
        let mut p = serde_json::json!({"sessionId": "s1"});
        set_event(&mut p, "sessionStart".into());
        assert_eq!(p["hook_event_name"], "sessionStart");
        let mut p = Value::Null;
        set_event(&mut p, "sessionStart".into());
        assert_eq!(p, Value::Null, "解析不了的载荷照样为 null");
    }

    /// 读上级进程的系统调用可用：测试进程至少能读到父进程（cargo 测试运行器）的参数，层数不超过上限。
    #[test]
    fn reads_ancestor_args() {
        let a = ancestor_args(ANCESTOR_LEVELS);
        assert!(!a.is_empty() && a.len() <= ANCESTOR_LEVELS);
        assert!(!a[0].is_empty(), "父进程参数为空：{a:?}");
    }
}
