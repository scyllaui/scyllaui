//! `scyllaui attach --terminal <ID>`：保活终端的窗格启动器（M3，docs/design.md 第 7 节「实现（M3）」）。
//! 作为 libghostty Surface 的命令运行，stdin 设为 raw，把字节原样中继到 scyllauid 持有的 PTY：不解析、不改写。
//! SIGWINCH 转为 RESIZE。做法移植自 S0 的 attach.rs（`--session`，连 keepd，原文件不改），差别：连 scyllauid 的
//! socket（`pty` 角色，docs/protocol.md 第 3.1 节），hello 带上本进程的全部环境（后台以它为底构造 Shell 的环境），
//! 连接断开而没收到 EXIT 时（后台接管、重启）在 5 秒内重连。
//!
//! 用法：scyllaui attach --terminal <ID> [--sock <路径>]（socket 缺省取 `SCYLLAUI_SOCK`）

use scyllaui_proto::pty::{self as frame, read_frame, write_frame};
use scyllaui_proto::{BUILD_ID, PROTOCOL_VERSION};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 连接断开（没收到 EXIT）后重连的总时长与间隔。
const RECONNECT_FOR: Duration = Duration::from_secs(5);
const RECONNECT_EVERY: Duration = Duration::from_millis(100);

type Writer = Arc<Mutex<Option<UnixStream>>>;

pub fn run(args: &[String]) -> i32 {
    let mut terminal = None;
    let mut sock = std::env::var("SCYLLAUI_SOCK").ok().filter(|s| !s.is_empty()).map(PathBuf::from);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--terminal" => terminal = it.next().cloned(),
            "--sock" => sock = it.next().map(PathBuf::from),
            _ => {
                eprintln!("scyllaui attach：未知参数 {a}");
                return 2;
            }
        }
    }
    let (Some(terminal), Some(sock)) = (terminal, sock) else {
        eprintln!("用法：scyllaui attach --terminal <ID> [--sock <路径>]（缺省取 SCYLLAUI_SOCK）");
        return 2;
    };

    // SIGWINCH 只由专用线程 sigwait 处理：先在主线程屏蔽，之后创建的线程继承屏蔽。
    // 必须在读取窗口尺寸之前屏蔽：Surface 在启动时会调整尺寸，若先读尺寸、后屏蔽，
    // 中间到达的 SIGWINCH 按默认处理被忽略，后台 PTY 就停在旧尺寸（S0 第 4 项实测）。
    // macOS 上默认动作为「忽略」的信号（SIGWINCH 即是）在产生时就被丢弃，屏蔽了也不会挂起，
    // sigwait 收不到；先装一个空处理函数，使其能挂起（S0 第 4 项实测）。
    extern "C" fn noop(_: libc::c_int) {}
    let mut set: libc::sigset_t = 0;
    unsafe {
        libc::signal(libc::SIGWINCH, noop as *const () as libc::sighandler_t);
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    wait_first_layout(&set);

    let (stream, mut reader) = match connect(&sock, &terminal) {
        Ok(c) => c,
        Err(why) => {
            eprintln!("scyllaui attach：{why}");
            return 1;
        }
    };
    let writer: Writer = Arc::new(Mutex::new(Some(stream)));
    let saved = raw_mode();

    // 后台 → 终端；断开而没收到 EXIT 时重连。
    let w = Arc::clone(&writer);
    std::thread::spawn(move || {
        let mut out = std::io::stdout().lock();
        let code = 'outer: loop {
            loop {
                match read_frame(&mut reader) {
                    Ok((frame::SNAPSHOT | frame::OUTPUT, data)) => {
                        if out.write_all(&data).and_then(|_| out.flush()).is_err() {
                            break 'outer 1;
                        }
                    }
                    Ok((frame::EXIT, data)) => {
                        break 'outer serde_json::from_slice::<Value>(&data)
                            .ok()
                            .and_then(|v| v["code"].as_i64())
                            .unwrap_or(0) as i32;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            *w.lock().unwrap() = None;
            match reconnect(&sock, &terminal) {
                Ok((s, r)) => {
                    *w.lock().unwrap() = Some(s);
                    reader = r;
                }
                Err(why) => {
                    let _ = write!(out, "\r\n[ScyllaUI] {why}\r\n");
                    let _ = out.flush();
                    break 1;
                }
            }
        };
        restore(saved);
        std::process::exit(code);
    });

    // 窗口尺寸变化
    let w = Arc::clone(&writer);
    std::thread::spawn(move || loop {
        let mut sig = 0;
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            continue;
        }
        send(&w, frame::RESIZE, size_json().to_string().as_bytes());
    });

    // 终端 → 后台
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => send(&writer, frame::INPUT, &buf[..n]),
        }
    }
    restore(saved);
    0
}

/// 写一帧到当前连接；正在重连时丢弃（重连后由快照恢复画面）。
fn send(w: &Writer, kind: u8, payload: &[u8]) {
    let mut g = w.lock().unwrap();
    if let Some(s) = g.as_mut() {
        if write_frame(s, kind, payload).is_err() {
            *g = None;
        }
    }
}

fn reconnect(sock: &PathBuf, terminal: &str) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    let deadline = Instant::now() + RECONNECT_FOR;
    loop {
        match connect(sock, terminal) {
            Ok(c) => return Ok(c),
            Err(why) if why.contains("not_found") || Instant::now() >= deadline => {
                return Err(format!("与后台的连接已断开（{why}），这个终端已结束。"))
            }
            Err(_) => std::thread::sleep(RECONNECT_EVERY),
        }
    }
}

/// 连接并握手：hello 带当前尺寸、目录与本进程的全部环境（只在后台需要起 Shell 时使用）。
fn connect(sock: &PathBuf, terminal: &str) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    let mut stream =
        UnixStream::connect(sock).map_err(|e| format!("连接 {} 失败：{e}", sock.display()))?;
    let env: Map<String, Value> = std::env::vars().map(|(k, v)| (k, Value::String(v))).collect();
    let mut hello = json!({
        "type": "hello", "role": "pty", "protocol_version": PROTOCOL_VERSION, "build_id": BUILD_ID,
        "terminal_id": terminal,
        "spawn": {
            "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
            "env": env,
        },
    });
    if let Value::Object(size) = size_json() {
        hello.as_object_mut().unwrap().extend(size);
    }
    let mut line = hello.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| format!("发送握手失败：{e}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(|e| format!("读取握手应答失败：{e}"))?;
    let v: Value = serde_json::from_str(&reply).map_err(|_| "握手应答不是 JSON".to_string())?;
    if v["type"] != "hello_ok" {
        return Err(format!("后台拒绝连接：{}", v["code"].as_str().unwrap_or("-")));
    }
    Ok((stream, reader))
}

/// 等 Surface 完成首次布局再读尺寸、连接后台：Surface 先按默认尺寸建 PTY 并启动本进程，窗口布局后才调整到
/// 实际尺寸。若按默认尺寸连接，会话里的 CLI 会多收到一次尺寸变化（Kimi 因此清屏，丢掉启动前的
/// Shell 行，S0 第 4 项实测）。收到第一个 SIGWINCH 即视为布局完成；macOS 没有 sigtimedwait，按 10 ms
/// 轮询挂起信号，最多等 300 ms。
/// 尺寸已不是 Surface 的初始尺寸（TerminalSurfaceView 以 800×600 点创建）时布局已经完成，不再等：M3 中 Surface
/// 在显示时才创建，约 10 ms 就布局完，而本进程经 login、bash 启动要 100 ms 以上，那个 SIGWINCH 早已丢失，
/// 原来每次都等满 300 ms（2026-10-17 尺寸日志实测）。
/// 收到尺寸变化后重新判断，仍是初始尺寸就继续等：App 启动时恢复的终端先后报 49×17（1 倍）、99×35（2 倍）两个
/// 初始尺寸才排版到实际尺寸，原来只等第一个就连上，会话里的 CLI 多收到一对尺寸变化（2026-10-29 实测，OpenCode
/// 因此停在 99 列）。
fn wait_first_layout(set: &libc::sigset_t) {
    for _ in 0..30 {
        if !is_initial(&winsize()) {
            return;
        }
        let mut pending: libc::sigset_t = 0;
        unsafe { libc::sigpending(&mut pending) };
        if unsafe { libc::sigismember(&pending, libc::SIGWINCH) } == 1 {
            let mut sig = 0;
            unsafe { libc::sigwait(set, &mut sig) };
            continue;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn winsize() -> libc::winsize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    ws
}

/// 是否仍是 Surface 的初始尺寸（800×600 点，1—3 倍屏）。Ghostty 报的像素是整格对齐、去掉边距后的网格大小
/// （800×600 点在 2 倍屏上报 1592×1192，1 倍屏报 784×578，2026-10-29 实测），不会恰好等于 800×600 的整数倍：
/// 按「不超过、且差不到两格」判断。
fn is_initial(ws: &libc::winsize) -> bool {
    let (cols, rows) = (ws.ws_col.max(1) as u32, ws.ws_row.max(1) as u32);
    let (x, y) = (ws.ws_xpixel as u32, ws.ws_ypixel as u32);
    let (cw, ch) = (x / cols, y / rows);
    (1..=3).any(|k| x <= 800 * k && 800 * k < x + 2 * cw && y <= 600 * k && 600 * k < y + 2 * ch)
}

fn size_json() -> Value {
    let ws = winsize();
    let cols = ws.ws_col.max(1);
    let rows = ws.ws_row.max(1);
    json!({
        "cols": cols, "rows": rows,
        "cell_w": ws.ws_xpixel as u32 / cols as u32,
        "cell_h": ws.ws_ypixel as u32 / rows as u32,
    })
}

fn raw_mode() -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let saved = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(saved)
    }
}

fn restore(saved: Option<libc::termios>) {
    if let Some(t) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(cols: u16, rows: u16, x: u16, y: u16) -> libc::winsize {
        libc::winsize { ws_col: cols, ws_row: rows, ws_xpixel: x, ws_ypixel: y }
    }

    /// 2026-10-29 实测 App 启动恢复终端时 Surface 依次报的尺寸：前两个是初始尺寸，要等；排版后的实际尺寸不等。
    #[test]
    fn initial_surface_sizes() {
        assert!(is_initial(&ws(49, 17, 784, 578)), "1 倍屏的初始尺寸");
        assert!(is_initial(&ws(99, 35, 1592, 1192)), "2 倍屏的初始尺寸");
        assert!(!is_initial(&ws(149, 49, 2384, 1696)), "排版后的实际尺寸");
        assert!(!is_initial(&ws(120, 40, 1920, 1360)));
        assert!(!is_initial(&ws(0, 0, 0, 0)), "读不到尺寸时不等");
    }
}
