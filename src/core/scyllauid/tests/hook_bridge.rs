//! 跨进程测试：Hook 桥。真实启动 scyllauid，用 scyllaui hook 喂入载荷，以 App 身份订阅并核对推送。

mod common;

use common::*;
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn hook_reaches_app_and_never_writes_stdout() {
    assert!(
        scyllaui().exists(),
        "先构建 scyllaui：cargo build --workspace"
    );
    let dir = data_dir("hook");
    let cap = dir.join("cap");
    let child = start_in(&dir, &["--capture", cap.to_str().unwrap()]);
    let d = Daemon {
        child: Some(child),
        sock: dir.join("scyllauid.sock"),
        dir,
    };
    let (mut app, _) = app(&d.sock, &["pane-1", "pane-2"]);

    let out = hook(
        &d.sock,
        "claude",
        "pane-1",
        &[],
        json!({"hook_event_name":"SessionStart","session_id":"c-1","transcript_path":"/t.jsonl","cwd":"/w/p"}),
    );
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "Hook 不得写 stdout（Claude 会把 SessionStart 的 stdout 当上下文）"
    );
    let conv = app.until(|m| m["type"] == "conversation.upsert");
    assert_eq!(
        (
            conv["conversation"]["native_id"].as_str(),
            conv["conversation"]["cwd"].as_str()
        ),
        (Some("c-1"), Some("/w/p"))
    );
    let m = app.until(|m| m["type"] == "terminal.upsert");
    let t = &m["terminal"];
    assert_eq!(
        (
            t["terminal_id"].as_str(),
            t["agent"]["tool"].as_str(),
            t["agent"]["state"].as_str()
        ),
        (Some("pane-1"), Some("claude"), Some("ready"))
    );
    assert_eq!(
        t["conversation_id"],
        conv["conversation"]["conversation_id"]
    );
    let trace = &m["trace"];
    let (t0, recv, send) = (
        trace["hook_t0_us"].as_u64().unwrap(),
        trace["daemon_recv_us"].as_u64().unwrap(),
        trace["daemon_send_us"].as_u64().unwrap(),
    );
    assert!(t0 <= recv && recv <= send, "{trace}");
    assert!(m["rev"].as_u64().unwrap() > conv["rev"].as_u64().unwrap());

    hook(
        &d.sock,
        "claude",
        "pane-1",
        &[],
        json!({"hook_event_name":"PermissionRequest","session_id":"c-1"}),
    );
    let m = app.until(|m| m["type"] == "terminal.upsert");
    assert_eq!(m["terminal"]["agent"]["state"], "waiting");

    // Grok 会话里由 Claude 配置触发的那一份丢弃，只认 Grok 配置那一份。
    let grok_env = [
        ("GROK_SESSION_ID", "g-1"),
        ("GROK_HOOK_EVENT", "session_start"),
    ];
    let gp =
        json!({"hook_event_name":"SessionStart","hookEventName":"session_start","sessionId":"g-1"});
    hook(&d.sock, "claude", "pane-2", &grok_env, gp.clone());
    hook(&d.sock, "grok", "pane-2", &grok_env, gp);
    let m = app.until(|m| m["type"] == "terminal.upsert");
    assert_eq!(
        (
            m["terminal"]["terminal_id"].as_str(),
            m["terminal"]["agent"]["tool"].as_str()
        ),
        (Some("pane-2"), Some("grok"))
    );
    let snap = snapshot(&d.sock);
    let grok: Vec<_> = snap["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["tool"] == "grok")
        .collect();
    assert_eq!(grok.len(), 1, "重复来源不得产生第二条对话");

    // 采集文件：丢弃的那一份也记录，便于核对去重；不在白名单内的变量值不落盘。
    let text = std::fs::read_to_string(cap.join("grok.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("\"dropped\":true"));
    assert!(!std::fs::read_dir(&cap).unwrap().any(|e| {
        std::fs::read_to_string(e.unwrap().path())
            .unwrap()
            .contains("must-not-leak")
    }));
}

#[test]
fn hook_for_unregistered_terminal_is_dropped() {
    let d = start("unreg");
    let (mut app, _) = app(&d.sock, &["pane-1"]);
    let out = hook(
        &d.sock,
        "codex",
        "pane-x",
        &[],
        json!({"hook_event_name":"SessionStart","session_id":"x"}),
    );
    assert!(out.status.success() && out.stdout.is_empty());
    let reply = app.req(json!({"type":"snapshot.get"}));
    assert!(reply["snapshot"]["conversations"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn hook_exits_zero_without_daemon_or_socket() {
    let out = hook(
        Path::new("/tmp/pw-none/d.sock"),
        "codex",
        "p",
        &[],
        json!({"hook_event_name":"Stop","session_id":"x"}),
    );
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
    let out = Command::new(scyllaui())
        .args(["hook", "--config", "codex"])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
}
