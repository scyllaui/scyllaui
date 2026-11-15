//! 跨进程测试（M3 第 1 块，docs/design.md 10.3 节）：真实启动 scyllauid，测试扮演 attach（`pty` 角色），
//! 会话里跑 /bin/sh 脚本（测试后台允许 `spawn.command`，不启动用户的登录 Shell）。
//! 只是分层证据；兼容结论以 ScyllaUI 真实窗口与 Ghostty.app 对照为准。

mod common;

// vt.rs 用到后台的日志宏；测试里直接打印。
macro_rules! log {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}
#[allow(dead_code)]
#[path = "../src/pty/vt.rs"]
mod vt;

use common::{app, connect, snapshot, start, wait_until, Client, PROTOCOL};
use scyllaui_proto::pty::{self as frame, read_frame};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

struct Pty {
    _s: UnixStream,
    r: BufReader<UnixStream>,
    hello: Value,
}

fn pty_hello(sock: &Path, terminal: &str, spawn: Value) -> Pty {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let h = json!({"type":"hello","role":"pty","protocol_version":PROTOCOL,"build_id":"test",
        "terminal_id": terminal, "cols": 60, "rows": 10, "cell_w": 8, "cell_h": 16, "spawn": spawn});
    writeln!(s, "{h}").unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let hello = serde_json::from_str(&line).unwrap_or(Value::Null);
    Pty { _s: s, r, hello }
}

fn sh(sock: &Path, terminal: &str, script: &str) -> Pty {
    pty_hello(sock, terminal, json!({"cwd": "/", "command": ["/bin/sh", "-c", script], "env": {"PATH": "/usr/bin:/bin"}}))
}

/// 读帧直到累计输出含 `until`（为空则读到 EXIT 或断开），返回（快照，后续输出，EXIT 的退出码）。
fn read_until(p: &mut Pty, until: &str) -> (Vec<u8>, Vec<u8>, Option<i64>) {
    let (mut snap, mut out, mut exit) = (Vec::new(), Vec::new(), None);
    while let Ok((kind, data)) = read_frame(&mut p.r) {
        match kind {
            frame::SNAPSHOT => snap = data,
            frame::OUTPUT => out.extend_from_slice(&data),
            frame::EXIT => {
                exit = serde_json::from_slice::<Value>(&data).ok().and_then(|v| v["code"].as_i64());
                break;
            }
            _ => {}
        }
        if !until.is_empty() && String::from_utf8_lossy(&out).contains(until) {
            break;
        }
    }
    (snap, out, exit)
}

/// 以 App 身份连接，登记一个保活终端（项目目录 /w/p）。
fn held(app: &mut Client, terminal: &str, input: Option<&str>) {
    let mut msg = json!({"type":"terminal.open","terminal_id":terminal,"project_dir":"/w/p","cwd":"/","held":true});
    if let Some(i) = input {
        msg["initial_input"] = json!(i);
    }
    assert_eq!(app.req(msg)["ok"], true);
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pids(text: &str, key: &str) -> Vec<i32> {
    text.split(|c: char| c.is_whitespace())
        .filter_map(|w| w.strip_prefix(key)?.parse().ok())
        .collect()
}

fn terminal<'a>(snap: &'a Value, id: &str) -> Option<&'a Value> {
    snap["terminals"].as_array()?.iter().find(|t| t["terminal_id"] == id)
}

#[test]
fn answers_queries_while_detached_and_restores_modes() {
    let d = start("pq");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "q", None);
    let out = d.dir.join("replies");
    let script = format!(
        "printf '\\033[?2004h\\033[?1000h\\033[?1006h\\033[>1u\\033[2;8r\\033[31mready\\033[0m\\n'; sleep 1; \
         stty raw -echo; dd bs=1 count=400 of={} 2>/dev/null </dev/tty & DD=$!; \
         printf '\\033[c\\033[>c\\033[>q\\033]10;?\\033\\\\\\033]11;?\\033\\\\\\033[18t'; sleep 1; kill $DD; sleep 30",
        out.display()
    );
    let mut c1 = sh(&d.sock, "q", &script);
    assert_eq!(c1.hello["type"], "hello_ok", "{}", c1.hello);
    assert_eq!(c1.hello["created"], true);
    read_until(&mut c1, "ready");
    drop(c1); // 断开：之后的查询由后台代答
    std::thread::sleep(Duration::from_millis(2600));
    let replies = std::fs::read(&out).unwrap_or_default();
    let r = String::from_utf8_lossy(&replies).into_owned();
    let e = vt::escape(&replies);
    assert!(r.starts_with("\x1b[?62;22;52c"), "DA1：{e}");
    assert!(r.contains("\x1b[>1;10;0c"), "DA2：{e}");
    assert!(r.contains("\x1bP>|ghostty 1.3.2-HEAD+34f3900\x1b\\"), "XTVERSION：{e}");
    assert!(r.contains("\x1b]10;rgb:ff"), "OSC 10：{e}");
    assert!(r.contains("\x1b]11;rgb:28"), "OSC 11：{e}");
    assert!(r.contains("\x1b[8;10;60t"), "尺寸：{e}");

    // 接回：快照恢复模式、滚动区与 Kitty 键盘标志。
    let mut c2 = sh(&d.sock, "q", "");
    assert_eq!(c2.hello["created"], false);
    let (kind, snap) = read_frame(&mut c2.r).unwrap();
    assert_eq!(kind, frame::SNAPSHOT);
    let s = vt::escape(&snap);
    for m in ["\\e[?2004h", "\\e[?1000h", "\\e[?1006h", "\\e[2;8r"] {
        assert!(s.contains(m), "快照缺少 {m}：{s}");
    }
    assert!(s.contains("ready"));
    // App 用 appearance.set 改配色后，代答随之变化（新的断开期间）。
    assert_eq!(a.req(json!({"type":"appearance.set","foreground":"#102030","background":"#a0b0c0","color_scheme":"light"}))["ok"], true);
    assert_eq!(a.req(json!({"type":"appearance.set","foreground":"red"}))["ok"], false);
}

#[test]
fn alternate_screen_keeps_primary_across_reconnect() {
    let d = start("pa");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "alt", None);
    let script = "i=0; while [ $i -lt 20 ]; do i=$((i+1)); echo \"shell $i\"; done; \
                  printf '\\033[?1049h\\033[H\\033[2J全屏界面\\n'; sleep 30";
    let mut c1 = sh(&d.sock, "alt", script);
    read_until(&mut c1, "全屏界面");
    drop(c1);
    let mut c2 = sh(&d.sock, "alt", "");
    let (_, snap) = read_frame(&mut c2.r).unwrap();
    let sink = Box::new(vt::Sink::new(-1));
    let mut t = vt::Vt::new(60, 10, &*sink);
    t.write(&snap);
    assert!(t.alt_screen(), "接回后应在备用屏");
    assert!(String::from_utf8_lossy(&t.plain()).contains("全屏界面"));
    t.write(b"\x1b[?1049l");
    assert!(String::from_utf8_lossy(&t.plain()).contains("shell 1\n"), "主屏内容应保留");
}

#[test]
fn no_output_lost_and_shell_exit_closes_terminal() {
    let d = start("pn");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "n", None);
    let script = "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo \"n $i\"; sleep 0.004; done; echo done; sleep 0.3; exit 3";
    let mut c1 = sh(&d.sock, "n", script);
    read_until(&mut c1, "n 40\r\n");
    drop(c1);
    std::thread::sleep(Duration::from_millis(300));
    let mut c2 = sh(&d.sock, "n", "");
    let (snap, out, exit) = read_until(&mut c2, "");
    assert_eq!(exit, Some(3), "attach 应收到 EXIT 3");

    let sink = Box::new(vt::Sink::new(-1));
    let mut t = vt::Vt::new(60, 10, &*sink);
    t.write(&snap);
    t.write(&out);
    let text = String::from_utf8_lossy(&t.plain()).into_owned();
    let nums: Vec<u32> = text.lines().filter_map(|l| l.strip_prefix("n ")?.trim().parse().ok()).collect();
    assert_eq!(nums, (1..=300).collect::<Vec<_>>(), "行号不连续：{text}");
    assert!(text.contains("done"));

    let up = a.until(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "n" && m["terminal"]["status"] == "closed");
    assert_eq!(up["terminal"]["exit_code"], 3);
    assert_eq!(up["terminal"]["close_reason"], "exit");
    // 已关闭的终端不能再接回。
    let c3 = sh(&d.sock, "n", "");
    assert_eq!(c3.hello["code"], "not_found");
}

#[test]
fn close_asks_for_confirmation_only_off_prompt() {
    let d = start("pc");
    let (mut a, _) = app(&d.sock, &[]);
    let prompt = "printf '\\033]133;A\\007$ \\033]133;B\\007'";
    let cases = [
        ("idle", format!("echo pid=$$; {prompt}; echo ok; sleep 30"), false),
        ("busy", format!("sleep 30 & echo pid=$$ kid=$!; {prompt}; printf 'sleep\\r\\n\\033]133;C\\007'; echo ok; wait"), true),
        ("alt", format!("echo pid=$$; {prompt}; printf '\\033[?1049h'; echo ok; sleep 30"), true),
    ];
    for (id, script, confirm) in cases {
        held(&mut a, id, None);
        let mut c = sh(&d.sock, id, &script);
        let (_, out, _) = read_until(&mut c, "ok");
        let text = String::from_utf8_lossy(&out).into_owned();
        let mut procs = pids(&text, "pid=");
        procs.extend(pids(&text, "kid="));
        assert!(!procs.is_empty() && procs.iter().all(|&p| alive(p)), "{id}：{text}");
        let r = a.req(json!({"type":"terminal.close","terminal_id":id}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["needs_confirm"], confirm, "{id}：{r}");
        assert_eq!(r["closed"], !confirm, "{id}：{r}");
        if confirm {
            let r = a.req(json!({"type":"terminal.close","terminal_id":id,"confirmed":true}));
            assert_eq!(r["closed"], true, "{r}");
        }
        wait_until(&format!("{id} 的进程组结束"), || procs.iter().all(|&p| !alive(p)));
        let (_, _, exit) = read_until(&mut c, "");
        assert!(exit.is_some(), "{id}：attach 应收到 EXIT");
        assert!(terminal(&snapshot(&d.sock), id).is_none(), "{id} 应已关闭");
    }
}

#[test]
fn app_disconnect_keeps_held_terminals_and_sync_reconciles() {
    let d = start("ps");
    let (mut a, _) = app(&d.sock, &["plain"]);
    held(&mut a, "keep", None);
    let mut c = sh(&d.sock, "keep", "echo up; sleep 60");
    read_until(&mut c, "up");
    assert_eq!(a.req(json!({"type":"terminal.focus","terminal_id":"keep"}))["ok"], true);
    drop(a);
    wait_until("App 断开后关闭非保活终端", || terminal(&snapshot(&d.sock), "plain").is_none());
    let s = snapshot(&d.sock);
    assert_eq!(terminal(&s, "keep").unwrap()["held"], true, "{s}");

    // App 重启：sync 不列出保活终端也保留，快照带 last_focus。
    let (_a2, snap) = app(&d.sock, &[]);
    assert!(terminal(&snap, "keep").is_some(), "{snap}");
    assert_eq!(snap["last_focus"], "keep");
    drop(c);
}

#[test]
fn sync_after_daemon_crash_closes_lost_pty_terminals() {
    let mut d = start("pl");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "lost", None);
    let mut c = sh(&d.sock, "lost", "echo pid=$$; sleep 60");
    let (_, out, _) = read_until(&mut c, "pid=");
    let (_, more, _) = read_until(&mut c, "\n");
    let pid = pids(&format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&more)), "pid=")[0];
    // 后台崩溃：内核关闭主端，Shell 随之结束（风险 17）。
    let mut child = d.child.take().unwrap();
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    wait_until("后台崩溃后 Shell 结束", || !alive(pid));
    d.child = Some(common::start_in(&d.dir, &[]));
    let mut a = connect(&d.sock, "app", PROTOCOL);
    let r = a.req(json!({"type":"sync","terminals":[
        {"terminal_id":"lost","project_dir":"/w/p","cwd":"/","held":true},
        {"terminal_id":"fixture","project_dir":"/w/p","cwd":"/"}]}));
    assert_eq!(r["ok"], true, "{r}");
    let snap = &r["snapshot"];
    assert!(terminal(snap, "lost").is_none(), "没有 PTY 的保活终端应关闭：{snap}");
    assert!(terminal(snap, "fixture").is_some(), "夹具终端照旧");
    assert_eq!(connect(&d.sock, "cli", PROTOCOL).greeting["pty_host"], true);
}

#[test]
fn shell_keeps_only_standard_descriptors_and_env_comes_from_attach() {
    let d = start("pf");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "fd", None);
    let script = "echo BEGIN; /usr/sbin/lsof -a -p $$ -d 0-65535 -Ff 2>/dev/null; echo END; \
                  /usr/bin/env | /usr/bin/sort; echo ENVEND; sleep 30";
    let mut c = pty_hello(&d.sock, "fd", json!({"cwd": "/", "command": ["/bin/sh", "-c", script],
        "env": {"PATH": "/usr/bin:/bin", "FOO": "bar", "SHLVL": "7", "GHOSTTY_SURFACE_ID": "9"}}));
    let (_, out, _) = read_until(&mut c, "ENVEND");
    let text = String::from_utf8_lossy(&out).replace('\r', "");
    let fds = text.split("BEGIN\n").nth(1).and_then(|t| t.split("END\n").next()).unwrap_or("");
    let open: Vec<&str> = fds.lines().filter(|l| l.starts_with('f')).collect();
    assert_eq!(open, ["f0", "f1", "f2"], "Shell 只应有 0/1/2 号描述符：{fds}");
    assert!(text.contains("FOO=bar\n"), "{text}");
    assert!(text.contains("TERM=xterm-256color\n"), "{text}");
    assert!(!text.contains("SHLVL=7") && !text.contains("GHOSTTY_SURFACE_ID"), "{text}");
}

#[test]
fn initial_input_is_written_only_after_entering_the_directory() {
    let d = start("pi");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "in", Some("abc\n"));
    let mut c = sh(&d.sock, "in", "read line; echo \"got:[$line]\"; sleep 30");
    let (_, out, _) = read_until(&mut c, "got:[");
    let (_, more, _) = read_until(&mut c, "]");
    let text = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&more));
    assert!(text.contains("got:[abc]"), "{text}");

    // 启动目录不存在：不代输，Shell 退回主目录，终端带 cwd_missing（「目录不存在」，2026-11-13 长跑：曾落到「/」并显示
    // 「目录无权限」）。启动目录以 App 登记的为准，attach 报的目录不用。
    let open = json!({"type":"terminal.open","terminal_id":"deny","project_dir":"/w/p","cwd":"/nonexistent-scyllaui-dir",
        "held":true,"initial_input":"abc\n"});
    assert_eq!(a.req(open)["ok"], true);
    let mut c = pty_hello(&d.sock, "deny", json!({"cwd": "/",
        "command": ["/bin/sh", "-c", "read -t 1 line; echo \"got:[$line] pwd:[$(pwd -P)]\"; sleep 30"], "env": {}}));
    let (_, out, _) = read_until(&mut c, "pwd:[");
    let (_, more, _) = read_until(&mut c, "]");
    let text = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&more));
    let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
    assert!(text.contains(&format!("got:[] pwd:[{}]", home.display())), "{text}");
    let up = a.until(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "deny" && m["terminal"]["notice"] == "cwd_missing");
    assert_eq!(up["terminal"]["held"], true);

    // 没有权限进入（风险 14）：照旧 cwd_denied，不代输。
    let locked = d.dir.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let open = json!({"type":"terminal.open","terminal_id":"lock","project_dir":"/w/p","cwd":locked,"held":true,"initial_input":"abc\n"});
    assert_eq!(a.req(open)["ok"], true);
    let mut c = pty_hello(&d.sock, "lock", json!({"cwd": "/",
        "command": ["/bin/sh", "-c", "read -t 1 line; echo \"got:[$line]\"; sleep 30"], "env": {}}));
    let (_, out, _) = read_until(&mut c, "]");
    assert!(String::from_utf8_lossy(&out).contains("got:[]"), "{}", String::from_utf8_lossy(&out));
    a.until(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "lock" && m["terminal"]["notice"] == "cwd_denied");
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// 回归（2026-11-13 长跑缺陷 C）：`kill -9` 后台后，App 在重起的 Shell 里用 Ghostty 的输入文字接口代输恢复命令，它按粘贴
/// 处理：zsh 已画出提示符、开了括号粘贴（默认开）时整段连同回车被包成粘贴，停在输入行上不执行；提示符出现前到的才执行。
/// 这里用真实的 zsh（`-f`，HOME、ZDOTDIR 指向测试目录，不读用户配置）先复现：括号粘贴包着的「命令 + 回车」不执行；
/// 再验证修复：`conversation.resume` 带 `type_into` 时后台把 `cd <目录> && <命令>` 与回车当键盘输入写进 PTY，照常执行。
#[test]
fn respawn_resume_is_typed_not_pasted() {
    let d = start("pr");
    let work = std::fs::canonicalize(&d.dir).unwrap().join("work dir");
    std::fs::create_dir_all(work.join("bin")).unwrap();
    let fake = work.join("bin/claude");
    std::fs::write(&fake, "#!/bin/sh\necho \"RESUMED-$1-$2 in $(pwd -P)\"\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let w = work.to_str().unwrap();
    let (mut a, _) = app(&d.sock, &[]);
    let open = json!({"type":"terminal.open","terminal_id":"z","project_dir":w,"cwd":"/","held":true});
    assert_eq!(a.req(open)["ok"], true);
    let env = json!({"HOME": w, "ZDOTDIR": w, "TERM": "xterm-256color", "PS1": "%# ",
        "PATH": format!("{w}/bin:/usr/bin:/bin")});
    let mut c = pty_hello(&d.sock, "z", json!({"cwd": "/", "command": ["/bin/zsh", "-f", "-i"], "env": env}));
    read_until(&mut c, "\x1b[?2004h");
    let input = |c: &mut Pty, bytes: &[u8]| {
        let mut b = Vec::new();
        scyllaui_proto::pty::write_frame(&mut b, frame::INPUT, bytes).unwrap();
        c._s.write_all(&b).unwrap();
    };
    // 复现：与 Ghostty 在括号粘贴模式下的输入文字相同的字节。
    input(&mut c, b"\x1b[200~echo pasted-$((40+2))\n\x1b[201~");
    std::thread::sleep(Duration::from_millis(800));
    input(&mut c, b"\x03");
    input(&mut c, b"echo marker-$((1+2))\n");
    let (_, out, _) = read_until(&mut c, "marker-3\r\n");
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.contains("pasted-$((40+2))") && !text.contains("pasted-42"), "括号粘贴里的回车不执行：{text:?}");

    // 修复：对话结束后按 `type_into` 恢复，后台当键盘输入写进去。
    let hook = |e: &str| common::hook(&d.sock, "claude", "z", &[], json!({"hook_event_name": e, "session_id": "s1", "cwd": w}));
    assert!(hook("SessionStart").status.success() && hook("SessionEnd").status.success());
    let conv = a.until(|m| m["type"] == "conversation.upsert" && !m["conversation"]["ended_at"].is_null());
    let cid = conv["conversation"]["conversation_id"].clone();
    let r = a.req(json!({"type": "conversation.resume", "conversation_id": cid, "type_into": "z"}));
    assert_eq!((r["action"].as_str(), r["command"].as_str()), (Some("typed"), Some("claude --resume s1")), "{r}");
    let (_, out, _) = read_until(&mut c, " in ");
    let (_, more, _) = read_until(&mut c, "\r\n");
    let text = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&more));
    assert!(text.contains(&format!("RESUMED---resume-s1 in {w}")), "{text:?}");
    // 没有这个终端时报 not_found。
    let r = a.req(json!({"type": "conversation.resume", "conversation_id": cid, "type_into": "nope"}));
    assert_eq!(r["error"]["code"], "not_found", "{r}");
}

/// 终端提示与当前目录只在后台内存里：App 断开重连后、换版本接管后都还在（2026-10-23 修正：接管后两者都丢失，
/// 「目录无权限」不再显示、副标题退回启动目录）。
#[test]
fn notice_and_current_dir_survive_reconnect_and_upgrade() {
    let d = start("pk");
    let (mut a, _) = app(&d.sock, &[]);
    let open = json!({"type":"terminal.open","terminal_id":"deny","project_dir":"/w/p","cwd":"/nonexistent-scyllaui-dir","held":true});
    assert_eq!(a.req(open)["ok"], true);
    // Shell 回退到其他目录启动，用 OSC 7 报当前目录后停住。
    let mut c = pty_hello(&d.sock, "deny", json!({"cwd": "/",
        "command": ["/bin/sh", "-c", "printf '\\033]7;file://localhost/usr\\007'; echo ready; sleep 60"], "env": {}}));
    read_until(&mut c, "ready");
    a.until(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "deny"
        && m["terminal"]["notice"] == "cwd_missing" && m["terminal"]["current_dir"] == "/usr");
    let check = |snap: &Value, when: &str| {
        let t = terminal(snap, "deny").unwrap_or_else(|| panic!("{when}：终端应存活：{snap}"));
        assert_eq!((t["notice"].as_str(), t["current_dir"].as_str()), (Some("cwd_missing"), Some("/usr")), "{when}：{t}");
    };
    // App 断开重连（后台不变）：后台处理完断开之前新的 App 握手会被拒，重试到成功为止。
    drop(a);
    let mut again = None;
    wait_until("App 重连", || {
        again = std::panic::catch_unwind(|| app(&d.sock, &[])).ok();
        again.is_some()
    });
    let (mut a2, snap) = again.unwrap();
    check(&snap, "重连后");
    // 换版本接管（exec 同一程序）。
    let r = a2.req(json!({"type":"upgrade","path": env!("CARGO_BIN_EXE_scyllauid")}));
    assert_eq!(r["ok"], true, "{r}");
    let mut a3 = None;
    wait_until("接管后重新监听", || {
        a3 = std::panic::catch_unwind(|| connect(&d.sock, "app", PROTOCOL)).ok();
        a3.is_some()
    });
    let r = a3.unwrap().req(json!({"type":"sync","terminals":[{"terminal_id":"deny","project_dir":"/w/p","cwd":"/nonexistent-scyllaui-dir","held":true}]}));
    check(&r["snapshot"], "换版本后");
    drop(c);
}

#[test]
fn stalled_attach_does_not_block_the_core() {
    let d = start("pb");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "flood", None);
    // 这个 attach 握手后不再读：读线程写它会卡住（最多 5 秒后断开），核心线程不受影响。
    let c = sh(&d.sock, "flood", "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done");
    std::thread::sleep(Duration::from_millis(800));
    let mut cli = connect(&d.sock, "cli", PROTOCOL);
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let t0 = Instant::now();
        assert_eq!(cli.req(json!({"type":"snapshot.get"}))["ok"], true);
        worst = worst.max(t0.elapsed());
    }
    assert!(worst <= Duration::from_millis(50), "snapshot.get 最慢 {worst:?}");
    let r = a.req(json!({"type":"terminal.close","terminal_id":"flood","confirmed":true}));
    assert_eq!(r["closed"], true, "{r}");
    drop(c);
}

#[test]
fn shutdown_is_refused_while_sessions_are_alive() {
    let mut d = start("pd");
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "s", None);
    let mut c = sh(&d.sock, "s", "echo pid=$$; sleep 60");
    let (_, out, _) = read_until(&mut c, "\n");
    let pid = pids(&String::from_utf8_lossy(&out), "pid=")[0];
    let r = a.req(json!({"type":"shutdown"}));
    assert_eq!(r["error"]["code"], "sessions_alive", "{r}");
    let r = a.req(json!({"type":"shutdown","force":true}));
    assert_eq!(r["ok"], true, "{r}");
    let mut child = d.child.take().unwrap();
    let t0 = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(3), "后台未退出");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_until("会话结束", || !alive(pid));
}

#[test]
fn upgrade_execs_in_place_and_keeps_sessions() {
    let d = start("pu");
    let pid0 = d.child.as_ref().unwrap().id();
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "up", None);
    let script = "echo pid=$$; i=0; while [ $i -lt 60 ]; do i=$((i+1)); echo \"u $i\"; sleep 0.02; done; \
                  echo waiting; read x; exit 5";
    let mut c = sh(&d.sock, "up", script);
    read_until(&mut c, "u 3\r\n");
    let r = a.req(json!({"type":"upgrade","path": env!("CARGO_BIN_EXE_scyllauid")}));
    assert_eq!(r["ok"], true, "{r}");
    // 旧连接随 exec 关闭；新进程沿用进程号，重新监听。
    let (_, more, exit) = read_until(&mut c, "");
    assert!(exit.is_none(), "接管期间 attach 不应收到 EXIT");
    // exec 之前旧进程还在监听：重试到新进程握手成功为止（App 的重连同样如此）。
    let mut cli = None;
    wait_until("接管后重新监听", || {
        cli = std::panic::catch_unwind(|| connect(&d.sock, "cli", PROTOCOL)).ok();
        cli.is_some()
    });
    let cli = cli.unwrap();
    assert_eq!(cli.greeting["pid"], pid0, "接管后进程号应不变：{}", cli.greeting);
    assert!(!d.dir.join("takeover.json").exists(), "接管文件应已删除");
    let mut a2 = connect(&d.sock, "app", PROTOCOL);
    let r = a2.req(json!({"type":"sync","terminals":[{"terminal_id":"up","project_dir":"/w/p","cwd":"/","held":true}]}));
    let up = terminal(&r["snapshot"], "up").unwrap_or_else(|| panic!("接管后终端应存活：{r}"));
    assert_ne!(up["foreground"]["kind"], "unknown", "接管后应已读过前台：{up}");
    // 接回：快照 + 后续输出里 60 行不丢不重。
    let mut c2 = sh(&d.sock, "up", "");
    assert_eq!(c2.hello["created"], false, "{}", c2.hello);
    let (snap, out, _) = read_until(&mut c2, "waiting");
    let sink = Box::new(vt::Sink::new(-1));
    let mut t = vt::Vt::new(60, 10, &*sink);
    t.write(&snap);
    t.write(&out);
    let text = String::from_utf8_lossy(&t.plain()).into_owned();
    let nums: Vec<u32> = text.lines().filter_map(|l| l.strip_prefix("u ")?.trim().parse().ok()).collect();
    let tail: Vec<u32> = nums.iter().copied().skip_while(|&n| n < 51).collect();
    assert_eq!(tail, (51..=60).collect::<Vec<_>>(), "屏幕上的最后几行应连续：{text}（接管前收到 {}）", String::from_utf8_lossy(&more));
    assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "行号不连续：{nums:?}");
    // Shell 仍是子进程：退出码照常取得。
    std::io::Write::write_all(&mut c2._s, &{
        let mut b = Vec::new();
        scyllaui_proto::pty::write_frame(&mut b, frame::INPUT, b"\n").unwrap();
        b
    })
    .unwrap();
    let (_, _, exit) = read_until(&mut c2, "");
    assert_eq!(exit, Some(5));
    let up = a2.until(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "up" && m["terminal"]["status"] == "closed");
    assert_eq!(up["terminal"]["exit_code"], 5);
}

/// 换版本接管期间持续发来的 Hook 一条不丢（2026-11-14，docs/design.md 第 7 节「换版本」）：监听 socket 交给新进程，
/// exec 期间连进来的连接在内核里排队；旧进程交出之前把已收到的处理完。发送方照 `scyllaui hook` 的做法：连一次、
/// 写超时 100 ms、不重试。每 4 条里有一条超过 socket 缓冲（8 KB）的大载荷。
#[test]
fn hooks_sent_during_upgrade_are_not_lost() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = common::data_dir("ph");
    let cap = dir.join("cap");
    let child = common::start_in(&dir, &["--capture", cap.to_str().unwrap()]);
    let d = common::Daemon { child: Some(child), sock: dir.join("scyllauid.sock"), dir: dir.clone() };
    let pid0 = d.child.as_ref().unwrap().id();
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "up", None);
    let _c = sh(&d.sock, "up", "sleep 30");
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, sock) = (stop.clone(), d.sock.clone());
    let sender = std::thread::spawn(move || {
        let (mut sent, mut failed) = (0u32, Vec::new());
        let hello = json!({"type":"hello","role":"hook","protocol_version":PROTOCOL,"build_id":"test"});
        while !flag.load(Ordering::Relaxed) {
            let pad = if sent % 4 == 0 { "x".repeat(20_000) } else { String::new() };
            let frame = json!({"type":"hook","config":"claude","source":"claude","dropped":false,"pane_id":"up",
                "hook_t0_us":1,"hook_send_us":1,
                "payload":{"hook_event_name":"PostToolUse","session_id":"s1","seq":sent,"pad":pad}});
            let r = UnixStream::connect(&sock).and_then(|mut s| {
                s.set_write_timeout(Some(Duration::from_millis(100)))?;
                s.write_all(format!("{hello}\n{frame}\n").as_bytes())
            });
            if let Err(e) = r {
                failed.push(format!("#{sent}: {e}"));
            }
            sent += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        (sent, failed)
    });
    std::thread::sleep(Duration::from_millis(200));
    let r = a.req(json!({"type":"upgrade","path": env!("CARGO_BIN_EXE_scyllauid")}));
    assert_eq!(r["ok"], true, "{r}");
    let mut cli = None;
    wait_until("接管后重新监听", || {
        cli = std::panic::catch_unwind(|| connect(&d.sock, "cli", PROTOCOL)).ok();
        cli.as_ref().is_some_and(|c| c.greeting["pid"] == pid0)
    });
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let (sent, failed) = sender.join().unwrap();
    assert!(failed.is_empty(), "发送失败 {} 条：{:?}", failed.len(), &failed[..failed.len().min(5)]);
    let mut got = Vec::new();
    wait_until("收到的 Hook 都已落盘", || {
        let text = std::fs::read_to_string(cap.join("claude.jsonl")).unwrap_or_default();
        got = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()?["frame"]["payload"]["seq"].as_u64()).collect();
        got.len() as u32 >= sent
    });
    let missing: Vec<u32> = (0..sent).filter(|n| !got.contains(&(*n as u64))).collect();
    assert!(missing.is_empty(), "发出 {sent} 条，丢了 {:?}", missing);
    assert!(sent > 100, "应在接管前后持续发送：{sent}");
}

#[test]
fn failed_preflight_keeps_the_old_daemon() {
    let d = start("pp");
    let pid0 = d.child.as_ref().unwrap().id();
    let (mut a, _) = app(&d.sock, &[]);
    held(&mut a, "pf", None);
    let mut c = sh(&d.sock, "pf", "echo ready; read x; echo after; sleep 30");
    read_until(&mut c, "ready");
    let r = a.req(json!({"type":"upgrade","path":"relative/scyllauid"}));
    assert_eq!(r["error"]["code"], "bad_request", "{r}");
    // 库版本高于新程序支持的版本：自检不过，留在本进程。
    let st = std::process::Command::new("/usr/bin/sqlite3")
        .arg(d.dir.join("scyllaui.db"))
        .arg("PRAGMA user_version=999;")
        .status()
        .unwrap();
    assert!(st.success());
    let r = a.req(json!({"type":"upgrade","path": env!("CARGO_BIN_EXE_scyllauid")}));
    assert_eq!(r["error"]["code"], "preflight_failed", "{r}");
    assert_eq!(connect(&d.sock, "cli", PROTOCOL).greeting["pid"], pid0);
    // 会话照常：输入仍能到达，输出仍能读到。
    std::io::Write::write_all(&mut c._s, &{
        let mut b = Vec::new();
        scyllaui_proto::pty::write_frame(&mut b, frame::INPUT, b"\n").unwrap();
        b
    })
    .unwrap();
    read_until(&mut c, "after");
    assert!(!d.dir.join("takeover.json").exists());
}

/// 后台的子进程里有没有僵尸（已退出、未回收）。
fn zombies(daemon: u32) -> Vec<String> {
    let out = std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,stat="]).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() == 3 && f[1] == daemon.to_string() && f[2].starts_with('Z')
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn exited_shells_are_always_reaped() {
    let d = start("pz");
    let pid = d.child.as_ref().unwrap().id();
    let (mut a, _) = app(&d.sock, &[]);
    // exec 失败（立即以 127 退出）、正常退出、被关闭三种，各几次。
    for i in 0..4 {
        let bad = format!("bad{i}");
        held(&mut a, &bad, None);
        let c = pty_hello(&d.sock, &bad, json!({"cwd": "/", "command": ["/nonexistent-scyllaui"], "env": {}}));
        drop(c);
        let ok = format!("ok{i}");
        held(&mut a, &ok, None);
        let mut c = sh(&d.sock, &ok, "exit 4");
        let (_, _, exit) = read_until(&mut c, "");
        assert_eq!(exit, Some(4));
        let cl = format!("cl{i}");
        held(&mut a, &cl, None);
        let mut c = sh(&d.sock, &cl, "echo up; sleep 30");
        read_until(&mut c, "up");
        assert_eq!(a.req(json!({"type":"terminal.close","terminal_id":cl,"confirmed":true}))["closed"], true);
    }
    wait_until("所有保活终端都已关闭", || snapshot(&d.sock)["terminals"].as_array().unwrap().is_empty());
    wait_until("没有僵尸进程", || zombies(pid).is_empty());
}
