//! 另起 CLI 进程查账号额度（2026-11-01 用户同意间接联网：「如果需要网络请求才能获取的，可以按照这个策略」）：由 CLI 用它
//! 自己的登录查询，ScyllaUI 不读、不存任何登录信息。Antigravity 用打印模式的 `/usage`（title.rs `run_quiet`）；本文件是
//! Kimi 与 Grok，两款都没有能直接打印额度的命令：
//! - Kimi：`kimi web --no-open --port 0` 只绑本机起一个它自己的服务，读启动时打印的地址与这个本机服务的令牌，
//!   `GET /api/v1/oauth/usage`（与它 `/usage` 斜杠命令同源）后以 SIGTERM 结束（它会注销自己的实例登记）。令牌只在内存里
//!   用这一次，不记日志。
//! - Grok：`grok agent --no-leader stdio`（ACP），`initialize` 后调扩展方法 `_x.ai/billing`，关掉标准输入即退出。
//!
//! 都在调用方另起的线程里运行，超时即结束进程；失败返回原因，调用方记一行日志。
//!
//! 另起的进程（含 Antigravity）都经 `spawn` 记在数据目录的 `quota-children/` 下，结束后删掉记录；后台查到一半被强制结束、
//! 崩溃或换版本接管时来不及结束它们，下次启动由 `reap_leftovers` 结束（`kimi web` 不会自己退出，见 docs/design.md 3.1.1 节）。

use scyllaui_proto::adapters::usage::{grok_quotas, kimi_quotas, kimi_web_address, Quota};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

/// 记录目录；`reap_leftovers` 设好之前（如单元测试）不记录。
static LEDGER: OnceLock<PathBuf> = OnceLock::new();

/// 另起的查额度进程与它的记录（`quota-children/<进程号>`，内容为启动时间毫秒）。进程结束后才能丢掉，丢掉时删记录。
pub struct Tracked {
    pub child: Child,
    mark: Option<PathBuf>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(p) = &self.mark {
            let _ = fs::remove_file(p);
        }
    }
}

/// 起进程并记下进程号与启动时间（用来认出是不是同一个进程，进程号会被复用）。
pub fn spawn(cmd: &mut Command) -> std::io::Result<Tracked> {
    let child = cmd.spawn()?;
    let mark = LEDGER.get().and_then(|dir| {
        let pid = child.id() as i32;
        let start = crate::procinfo::start_ms(pid)?;
        let p = dir.join(pid.to_string());
        fs::write(&p, start.to_string()).ok()?;
        Some(p)
    });
    Ok(Tracked { child, mark })
}

/// 启动时（已取得单实例锁、还没开始查额度）调用：设好记录目录，结束上次留下的查额度进程。只结束进程号与启动时间都对得上的；
/// 结束在另起的线程里做，不拖慢启动。
pub fn reap_leftovers(data_dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    let dir = data_dir.join("quota-children");
    if let Err(e) = fs::DirBuilder::new().mode(0o700).create(&dir) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            log!("查额度进程的记录目录不可用（{e}），不记录");
            return;
        }
    }
    let left: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    let _ = LEDGER.set(dir);
    if left.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for p in left {
            let pid = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<i32>().ok());
            let start = fs::read_to_string(&p).ok().and_then(|s| s.trim().parse::<i64>().ok());
            if let (Some(pid), Some(start)) = (pid, start) {
                if pid > 1 && crate::procinfo::start_ms(pid) == Some(start) {
                    let how = terminate(pid);
                    log!("结束上次留下的查额度进程 {pid}（{how}）");
                }
            }
            let _ = fs::remove_file(&p);
        }
    });
}

/// 先 SIGTERM，5 秒不退再 SIGKILL。换版本接管后它仍是本进程的子进程，顺带回收。
fn terminate(pid: i32) -> &'static str {
    let gone = || {
        // SAFETY: 只回收这个进程号；不是本进程的子进程时返回错误，不影响别处。
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        !crate::procinfo::process_exists(pid)
    };
    // SAFETY: 进程号与启动时间已核对，是本程序起的查额度进程。
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if gone() {
            return "SIGTERM";
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: 同上。
    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(200));
    if gone() { "SIGKILL" } else { "SIGKILL 后仍在" }
}

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
    let mut tracked = spawn(command(exe, &["web", "--no-open", "--port", "0"], home).stdin(Stdio::null()))
        .map_err(|_| "启动失败")?;
    let child = &mut tracked.child;
    let got = (|| -> Result<Vec<Quota>, QuotaMiss> {
        let rx = lines(child).ok_or("没有输出")?;
        let (port, token) = wait_line(&rx, Instant::now() + STEP_TIMEOUT, kimi_web_address).ok_or("没有等到本机地址")?;
        let body = http_get(port, "/api/v1/oauth/usage", &token).ok_or("请求失败")?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| "返回认不出")?;
        Ok(kimi_quotas(&v, scyllaui_proto::now_ms()))
    })();
    stop(child);
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
    let mut tracked = spawn(command(exe, &["agent", "--no-leader", "stdio"], home).stdin(Stdio::piped()))
        .map_err(|_| "启动失败")?;
    let child = &mut tracked.child;
    let got = (|| -> Result<Vec<Quota>, QuotaMiss> {
        let rx = lines(child).ok_or("没有输出")?;
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
    stop(child);
    got
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查额度时记下进程号与启动时间，进程结束、丢掉后记录随之删掉。
    #[test]
    fn spawn_records_and_drop_removes() {
        let data = std::env::temp_dir().join(format!("scyllauid-cliquota-{}", std::process::id()));
        let _ = fs::remove_dir_all(&data);
        fs::create_dir_all(&data).unwrap();
        reap_leftovers(&data);
        let mut t = spawn(Command::new("/bin/sleep").arg("30")).unwrap();
        let pid = t.child.id() as i32;
        let mark = data.join("quota-children").join(pid.to_string());
        let start: i64 = fs::read_to_string(&mark).unwrap().parse().unwrap();
        assert_eq!(Some(start), crate::procinfo::start_ms(pid));
        stop(&mut t.child);
        assert!(mark.exists(), "进程结束前记录不该删");
        drop(t);
        assert!(!mark.exists());
        let _ = fs::remove_dir_all(&data);
    }
}
