//! 跨进程测试：常驻生命周期（docs/protocol.md 第 1、3、7 节）——单实例锁、遗留 socket、
//! 异常退出后数据仍在、shutdown 防护、App 断开即关闭终端、scyllaui ctl。

mod common;

use common::*;
use serde_json::json;
use std::process::Command;
use std::time::{Duration, Instant};

fn wait_exit(child: &mut std::process::Child) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        assert!(Instant::now() < deadline, "进程 5 秒内未退出");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn build_id_flag_prints_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_scyllauid"))
        .arg("--build-id")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8(out.stdout).unwrap().trim().is_empty());
}

#[test]
fn second_daemon_is_refused_by_lock() {
    let d = start("lock");
    let mut second = spawn(&d.dir, &[]);
    assert_eq!(wait_exit(&mut second), 3, "锁被占用时退出码为 3");
    assert!(d.sock.exists(), "第二个实例不得删除正在使用的 socket");
    assert!(
        snapshot(&d.sock)["projects"].is_array(),
        "第一个实例照常服务"
    );
}

#[test]
fn overlong_socket_path_fails_clearly() {
    let dir = format!("/tmp/pw-{}-{}", std::process::id(), "x".repeat(100));
    let out = Command::new(env!("CARGO_BIN_EXE_scyllauid"))
        .args(["--data-dir", &dir])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("socket 路径超过"));
}

#[test]
fn kill9_leaves_stale_socket_and_restart_recovers_state() {
    let mut d = start("kill9");
    {
        let (mut app, _) = app(&d.sock, &["t1"]);
        hook(
            &d.sock,
            "kimi",
            "t1",
            &[],
            json!({"hook_event_name":"SessionStart","session_id":"a","session_title":"T"}),
        );
        app.until(|m| m["type"] == "terminal.upsert");
        let mut child = d.child.take().unwrap();
        child.kill().unwrap(); // SIGKILL
        child.wait().unwrap();
    }
    assert!(d.sock.exists(), "SIGKILL 后 socket 文件遗留");
    d.child = Some(start_in(&d.dir, &[]));
    let snap = snapshot(&d.sock);
    let t = &snap["terminals"][0];
    assert_eq!(
        (
            t["terminal_id"].as_str(),
            t["agent"]["state"].as_str(),
            t["agent"]["session_id"].as_str()
        ),
        (Some("t1"), Some("unknown"), Some("a")),
        "终端等 App 对账；绑定的对话恢复，状态未知：{snap}"
    );
    assert_eq!(snap["conversations"][0]["title"], "T");
}

/// 回归（2026-11-21）：测试后台不另起 Antigravity、Kimi、Grok 查额度。此前上面 kill9 那条测试用真实家目录起了真的
/// `kimi web`，后台被 SIGKILL 后它留在后台，36 个孤儿进程由此而来。这里把三款换成只记一行的假程序，放进后台的 HOME。
#[test]
fn test_backend_does_not_run_quota_commands() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = data_dir("noquota");
    let home = home_of(&dir);
    for exe in [".kimi-code/bin/kimi", ".grok/bin/grok", ".local/bin/agy"] {
        let path = home.join(exe);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\necho \"$0 $*\" >> \"$HOME/ran\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let d = Daemon { child: Some(start_in(&dir, &[])), sock: dir.join("scyllauid.sock"), dir };
    let (_app, _) = app(&d.sock, &["t1", "t2", "t3"]);
    hook(&d.sock, "kimi", "t1", &[], json!({"hook_event_name":"SessionStart","session_id":"k-1"}));
    let grok_env = [("GROK_SESSION_ID", "g-1"), ("GROK_HOOK_EVENT", "session_start")];
    hook(&d.sock, "grok", "t2", &grok_env,
        json!({"hook_event_name":"SessionStart","hookEventName":"session_start","sessionId":"g-1"}));
    hook(&d.sock, "antigravity", "t3", &[],
        json!({"hook_event_name":"SessionStart","conversationId":"a-1","workspacePaths":["/w"]}));
    wait_until("三款的对话都已建立", || snapshot(&d.sock)["conversations"].as_array().is_some_and(|c| c.len() == 3));
    // 一启动就读一次（core.rs `request_title`，不延迟），另起命令的线程在读取线程里立即起；等 2 秒足够。
    std::thread::sleep(Duration::from_secs(2));
    let ran = std::fs::read_to_string(home.join("ran")).unwrap_or_default();
    assert_eq!(ran, "", "测试后台另起了查额度的命令");
}

/// 进程的启动时间（Unix 毫秒），与后台 procinfo.rs `start_ms` 同一口径。
fn start_ms(pid: u32) -> i64 {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    assert_eq!(n, size);
    info.pbi_start_tvsec as i64 * 1000 + info.pbi_start_tvusec as i64 / 1000
}

/// 回归（2026-11-21）：后台查额度到一半被强制结束（或换版本接管）时另起的进程没人结束，`kimi web` 不会自己退出。启动时按
/// `quota-children/` 的记录结束它们：认 SIGTERM 的、不认 SIGTERM 的（5 秒后 SIGKILL）都结束；启动时间对不上的（进程号已被
/// 别的进程复用）不动；记录全部删掉。
#[test]
fn startup_ends_quota_processes_left_by_previous_backend() {
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Stdio;
    let dir = data_dir("reap");
    let ledger = dir.join("quota-children");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&ledger).unwrap();
    let sh = |script: &str| Command::new("/bin/sh").args(["-c", script]).stdin(Stdio::null()).spawn().unwrap();
    let mut polite = sh("exec /bin/sleep 60");
    // 都会在 60 秒内自己退出：测试中途失败时不留下长期运行的进程。
    // 打印 ready 时 trap 已生效；读到这一行再起后台，否则 SIGTERM 可能先于 trap 到达（2026-11-23 偶发失败）。
    // 读端留到测试结束：循环里不再写标准输出，不会卡住。
    let mut stubborn = Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; echo ready; i=0; while [ $i -lt 60 ]; do /bin/sleep 1; i=$((i+1)); done"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = std::io::BufReader::new(stubborn.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut ready, &mut line).unwrap();
    assert_eq!(line, "ready\n", "不认 SIGTERM 的进程没有准备好");
    let mut stranger = sh("exec /bin/sleep 60");
    for (c, start) in [(&polite, start_ms(polite.id())), (&stubborn, start_ms(stubborn.id())), (&stranger, start_ms(stranger.id()) - 1000)] {
        std::fs::write(ledger.join(c.id().to_string()), start.to_string()).unwrap();
    }
    std::fs::write(ledger.join("999999"), "1").unwrap();
    let d = Daemon { child: Some(start_in(&dir, &[])), sock: dir.join("scyllauid.sock"), dir };
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut a, mut b) = (None, None);
    while (a.is_none() || b.is_none()) && Instant::now() < deadline {
        a = a.or_else(|| polite.try_wait().unwrap());
        b = b.or_else(|| stubborn.try_wait().unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(a.and_then(|s| s.signal()), Some(libc::SIGTERM), "认 SIGTERM 的没有被结束");
    assert_eq!(b.and_then(|s| s.signal()), Some(libc::SIGKILL), "不认 SIGTERM 的没有被强制结束");
    assert!(stranger.try_wait().unwrap().is_none(), "启动时间对不上的进程被结束了");
    let _ = stranger.kill();
    let _ = stranger.wait();
    wait_until("记录全部删掉", || std::fs::read_dir(&ledger).unwrap().count() == 0);
    drop(d);
}

#[test]
fn shutdown_is_guarded_while_app_connected_and_cleans_up() {
    let mut d = start("shutdown");
    let (mut app, _) = app(&d.sock, &["t1"]);
    let mut cli = connect(&d.sock, "cli", PROTOCOL);
    let r = cli.req(json!({"type":"shutdown"}));
    assert_eq!(
        (r["ok"].as_bool(), r["error"]["code"].as_str()),
        (Some(false), Some("app_connected"))
    );
    // 旧版本的 App（协议不符）也不能在别的 App 连接时关掉后台。
    let mut old = connect(&d.sock, "app", 1);
    assert_eq!(old.greeting["code"], "protocol_mismatch");
    assert_eq!(
        old.req(json!({"type":"shutdown"}))["error"]["code"],
        "app_connected"
    );
    let r = app.req(json!({"type":"shutdown"}));
    assert_eq!(r["ok"], true);
    assert_eq!(wait_exit(d.child.as_mut().unwrap()), 0);
    assert!(!d.sock.exists(), "shutdown 删除 socket");
    // 锁已释放：立即可以启动新实例，数据还在。
    d.child = Some(start_in(&d.dir, &[]));
    assert_eq!(snapshot(&d.sock)["projects"][0]["dir"], "/w/p");
}

#[test]
fn protocol_mismatch_can_shutdown_when_no_app() {
    let mut d = start("mismatch");
    let mut old = connect(&d.sock, "app", 1);
    assert_eq!(
        (
            old.greeting["type"].as_str(),
            old.greeting["protocol_version"].as_u64()
        ),
        (Some("hello_err"), Some(3))
    );
    assert_eq!(old.req(json!({"type":"shutdown"}))["ok"], true);
    assert_eq!(wait_exit(d.child.as_mut().unwrap()), 0);
}

#[test]
fn app_disconnect_closes_terminals_and_ends_conversations() {
    let d = start("disconnect");
    {
        let (mut app, _) = app(&d.sock, &["t1"]);
        hook(
            &d.sock,
            "codex",
            "t1",
            &[],
            json!({"hook_event_name":"SessionStart","session_id":"c"}),
        );
        // 提问过才算可恢复的对话（从没提问过的不进「最近任务」）。
        hook(
            &d.sock,
            "codex",
            "t1",
            &[],
            json!({"hook_event_name":"UserPromptSubmit","session_id":"c","prompt":"你好"}),
        );
        app.until(|m| m["type"] == "terminal.upsert");
        let second = connect(&d.sock, "app", PROTOCOL);
        assert_eq!(second.greeting["code"], "app_already_connected");
    }
    wait_until("App 断开后终端关闭", || {
        snapshot(&d.sock)["terminals"]
            .as_array()
            .unwrap()
            .is_empty()
    });
    let mut cli = connect(&d.sock, "cli", PROTOCOL);
    let r = cli.req(json!({"type":"recent.query","limit":10}));
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        (
            items[0]["native_id"].as_str(),
            items[0]["end_reason"].as_str()
        ),
        (Some("c"), Some("terminal_closed"))
    );
    // 名额已释放，新的 App 可以连接。
    let (_app, snap) = app(&d.sock, &[]);
    assert!(snap["conversations"].as_array().unwrap().is_empty());
}

#[test]
fn scyllaui_ctl_queries_and_shuts_down() {
    let mut d = start("ctl");
    let ctl = |args: &[&str]| {
        Command::new(scyllaui())
            .arg("ctl")
            .args(args)
            .env_clear()
            .env("SCYLLAUI_SOCK", &d.sock)
            .output()
            .unwrap()
    };
    let out = ctl(&["snapshot"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    let out = ctl(&["recent", "--limit", "5"]);
    assert!(out.status.success());
    assert_eq!(ctl(&["bogus"]).status.code(), Some(2));
    assert!(ctl(&["shutdown"]).status.success());
    assert_eq!(wait_exit(d.child.as_mut().unwrap()), 0);
    assert_eq!(
        ctl(&["snapshot"]).status.code(),
        Some(1),
        "后台已退出时明确失败"
    );
}
