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
