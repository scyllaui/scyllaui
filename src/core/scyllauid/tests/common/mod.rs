//! 跨进程测试共用：在 /tmp 短目录中真实启动 scyllauid（socket 路径不能超过 103 字节），以 app / cli 身份连接，
//! 用 scyllaui hook 喂入载荷。依赖同一 target 目录下已构建的 scyllaui（`cargo test --workspace` 会一并构建）。
#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const PROTOCOL: u64 = 3;

/// 测试用数据目录，结束时删除；其中的 scyllauid 进程在 Drop 时结束。
pub struct Daemon {
    pub child: Option<Child>,
    pub dir: PathBuf,
    pub sock: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn data_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/pw-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

pub fn spawn(dir: &Path, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_scyllauid"))
        .arg("--data-dir")
        .arg(dir)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // 不接管道：测试不读日志，管道写满会卡住后台。
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

pub fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "5 秒内未等到：{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 启动并等到可以连接（socket 文件存在且能连上）。
pub fn start_in(dir: &Path, extra: &[&str]) -> Child {
    let child = spawn(dir, extra);
    let sock = dir.join("scyllauid.sock");
    wait_until("scyllauid 监听", || UnixStream::connect(&sock).is_ok());
    child
}

pub fn start(name: &str) -> Daemon {
    let dir = data_dir(name);
    let child = start_in(&dir, &[]);
    Daemon {
        child: Some(child),
        sock: dir.join("scyllauid.sock"),
        dir,
    }
}

pub struct Client {
    pub reader: BufReader<UnixStream>,
    pub writer: UnixStream,
    pub greeting: Value,
    next_id: i64,
}

pub fn connect(sock: &Path, role: &str, version: u64) -> Client {
    let mut writer = UnixStream::connect(sock).unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let hello = json!({"type":"hello","role":role,"protocol_version":version,"build_id":"test"});
    writeln!(writer, "{hello}").unwrap();
    let mut reader = BufReader::new(writer.try_clone().unwrap());
    let greeting = read(&mut reader);
    Client {
        reader,
        writer,
        greeting,
        next_id: 0,
    }
}

pub fn read(r: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("读到的不是 JSON（{e}）：{line:?}"))
}

impl Client {
    /// 发一条请求并返回应答；其间收到的推送放进 `pushes`。
    pub fn request(&mut self, mut msg: Value, pushes: &mut Vec<Value>) -> Value {
        self.next_id += 1;
        msg["id"] = json!(self.next_id);
        writeln!(self.writer, "{msg}").unwrap();
        loop {
            let m = read(&mut self.reader);
            if m["type"] == "reply" && m["id"] == self.next_id {
                return m;
            }
            pushes.push(m);
        }
    }

    pub fn req(&mut self, msg: Value) -> Value {
        self.request(msg, &mut Vec::new())
    }

    /// 读推送，直到满足条件（跳过其他推送）。
    pub fn until(&mut self, mut f: impl FnMut(&Value) -> bool) -> Value {
        loop {
            let m = read(&mut self.reader);
            if f(&m) {
                return m;
            }
        }
    }
}

/// 以 App 身份连接并登记终端（项目目录 /w/p）。
pub fn app(sock: &Path, terminals: &[&str]) -> (Client, Value) {
    let mut c = connect(sock, "app", PROTOCOL);
    assert_eq!(c.greeting["type"], "hello_ok", "{}", c.greeting);
    let list: Vec<Value> = terminals
        .iter()
        .map(|t| json!({"terminal_id": t, "project_dir": "/w/p", "cwd": "/w/p"}))
        .collect();
    let reply = c.req(json!({"type":"sync","terminals":list}));
    assert_eq!(reply["ok"], true, "{reply}");
    (c, reply["snapshot"].clone())
}

pub fn snapshot(sock: &Path) -> Value {
    let mut c = connect(sock, "cli", PROTOCOL);
    c.req(json!({"type":"snapshot.get"}))["snapshot"].clone()
}

pub fn scyllaui() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_scyllauid")).with_file_name("scyllaui")
}

pub fn hook(
    sock: &Path,
    config: &str,
    pane: &str,
    extra_env: &[(&str, &str)],
    payload: Value,
) -> std::process::Output {
    let mut cmd = Command::new(scyllaui());
    cmd.args(["hook", "--config", config])
        .env_clear()
        .env("SCYLLAUI_SOCK", sock)
        .env("SCYLLAUI_PANE_ID", pane)
        .env("XAI_API_KEY", "must-not-leak")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
