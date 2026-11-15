//! 另起 CLI 进程查账号额度（2026-11-01 用户同意间接联网：「如果需要网络请求才能获取的，可以按照这个策略」）：由 CLI 用它
//! 自己的登录查询，ScyllaUI 不读、不存任何登录信息。Antigravity 用打印模式的 `/usage`（title.rs `run_quiet`）；本文件是
//! Kimi 与 Grok，两款都没有能直接打印额度的命令：
//! - Kimi：`kimi web --no-open --port 0` 只绑本机起一个它自己的服务，读启动时打印的地址与这个本机服务的令牌，
//!   `GET /api/v1/oauth/usage`（与它 `/usage` 斜杠命令同源）后以 SIGTERM 结束（它会注销自己的实例登记）。令牌只在内存里
//!   用这一次，不记日志。
//! - Grok：`grok agent --no-leader stdio`（ACP），`initialize` 后调扩展方法 `_x.ai/billing`，关掉标准输入即退出。
//!
//! 都在调用方另起的线程里运行，超时即结束进程；失败返回原因，调用方记一行日志。

use scyllaui_proto::adapters::usage::{grok_quotas, kimi_quotas, kimi_web_address, Quota};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 等 CLI 启动、回应的上限。
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// 没查到额度：`why` 记日志；`note` 为能告诉用户的原因代码（协议 `quota.note`，2026-11-01），没有时为 None。
pub struct QuotaMiss {
    pub why: &'static str,
    pub note: Option<&'static str>,
}

impl From<&'static str> for QuotaMiss {
    fn from(why: &'static str) -> QuotaMiss {
        QuotaMiss { why, note: None }
    }
}

/// 环境只留 HOME 与最小 PATH，在家目录下运行，标准错误丢弃。
fn command(exe: &Path, args: &[&str], home: &Path) -> Command {
    let mut c = Command::new(exe);
    c.args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

/// 标准输出逐行交给通道，读到结束为止（不读完，CLI 写满管道会卡住）。
fn lines(child: &mut Child) -> Option<mpsc::Receiver<String>> {
    let out = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    Some(rx)
}

/// 先 SIGTERM 让它正常退出，5 秒不退再强制结束。
fn stop(child: &mut Child) {
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 在 `deadline` 前等到第一行满足 `pick` 的输出。
fn wait_line<T>(rx: &mpsc::Receiver<String>, deadline: Instant, mut pick: impl FnMut(&str) -> Option<T>) -> Option<T> {
    loop {
        let line = rx.recv_timeout(deadline.checked_duration_since(Instant::now())?).ok()?;
        if let Some(v) = pick(&line) {
            return Some(v);
        }
    }
}

/// Kimi 的 5 小时、7 天（按月计的账号还有月）额度。
pub fn kimi_web(exe: &Path, home: &Path) -> Result<Vec<Quota>, QuotaMiss> {
    let mut child = command(exe, &["web", "--no-open", "--port", "0"], home)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|_| "启动失败")?;
    let got = (|| -> Result<Vec<Quota>, QuotaMiss> {
        let rx = lines(&mut child).ok_or("没有输出")?;
        let (port, token) = wait_line(&rx, Instant::now() + STEP_TIMEOUT, kimi_web_address).ok_or("没有等到本机地址")?;
        let body = http_get(port, "/api/v1/oauth/usage", &token).ok_or("请求失败")?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| "返回认不出")?;
        Ok(kimi_quotas(&v, scyllaui_proto::now_ms()))
    })();
    stop(&mut child);
    got
}

/// 向本机端口发一个 GET（`Connection: close`，读到对方关闭），只认 200 且不分块的响应，返回正文。
fn http_get(port: u16, path: &str, token: &str) -> Option<Vec<u8>> {
    let mut s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), STEP_TIMEOUT).ok()?;
    s.set_read_timeout(Some(STEP_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(STEP_TIMEOUT)).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nAccept: application/json\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
    let ok = head.starts_with("http/1.") && head.split_whitespace().nth(1) == Some("200");
    (ok && !head.contains("transfer-encoding: chunked")).then(|| buf[split + 4..].to_vec())
}

/// Grok 账号本期（本机为每周）额度。
pub fn grok_billing(exe: &Path, home: &Path) -> Result<Vec<Quota>, QuotaMiss> {
    let mut child = command(exe, &["agent", "--no-leader", "stdio"], home)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|_| "启动失败")?;
    let got = (|| -> Result<Vec<Quota>, QuotaMiss> {
        let rx = lines(&mut child).ok_or("没有输出")?;
        let mut stdin = child.stdin.take().ok_or("没有输入")?;
        let reply = |id: i64| {
            move |line: &str| serde_json::from_str::<serde_json::Value>(line).ok().filter(|m| m["id"] == id)
        };
        let deadline = Instant::now() + STEP_TIMEOUT;
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientCapabilities":{{}}}}}}"#)
            .map_err(|_| "写入失败")?;
        wait_line(&rx, deadline, reply(1)).ok_or("初始化没有回应")?;
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"_x.ai/billing","params":{{}}}}"#).map_err(|_| "写入失败")?;
        let m = wait_line(&rx, deadline, reply(2)).ok_or("查询没有回应")?;
        if m.get("result").is_none() {
            // 没登录 grok.com（如只用 API key）时为 `Authentication required`：告诉用户原因。不据此判断按量计费：登录前就已启动
            // 的会话照样能用（2026-11-01 实测团队账号会话），而这里查不到。
            if m["error"]["message"].as_str().is_some_and(|s| s.contains("Authentication required")) {
                return Err(QuotaMiss { why: "Grok 返回未登录 grok.com", note: Some("login_required") });
            }
            return Err("Grok 返回错误".into());
        }
        Ok(grok_quotas(&m["result"], scyllaui_proto::now_ms()))
    })();
    // 关掉标准输入它会自己退出（约 4 秒），这里不等，直接结束。
    stop(&mut child);
    got
}
