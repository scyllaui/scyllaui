//! 连接线程：每个连接一个读线程，只负责收发，把命令交给核心线程（core.rs）。
//! app / cli 连接另有一个写线程，经有界队列接收核心线程的输出；hook 连接只读不写。

use crate::core::{Cmd, Out};
use scyllaui_proto::{now_us, PROTOCOL_VERSION};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// 单个连接的输出队列长度；满了说明对端长时间不读，核心线程会断开它。
const QUEUE: usize = 1024;
const MAX_LINE: usize = 2 << 20;

/// 接收连接的暂停阀（换版本接管用，docs/design.md 第 7 节「换版本」）：核心线程交出状态之前叫停接收，此后连进来的
/// 连接留在内核的队列里，由沿用同一个监听 socket 的新进程接收，不丢。接收线程阻塞在 poll 上，空闲不唤醒。
pub struct AcceptGate {
    /// 管道的写端：`P` 暂停，`R` 恢复。
    ctl: RawFd,
    /// 接收线程停下后回一个确认。
    paused: Receiver<()>,
}

impl AcceptGate {
    /// 叫停接收并等接收线程确认；等不到（不该发生）返回 false，此时不交出监听 socket。
    pub fn pause(&self) -> bool {
        self.write(b'P') && self.paused.recv_timeout(Duration::from_millis(500)).is_ok()
    }

    pub fn resume(&self) {
        self.write(b'R');
    }

    fn write(&self, c: u8) -> bool {
        // SAFETY: 向本进程的管道写一个字节。
        unsafe { libc::write(self.ctl, (&c as *const u8).cast(), 1) == 1 }
    }
}

/// 返回暂停阀与接收循环（由调用方在自己的线程上运行，不返回）：每个连接起一个线程。
pub fn accept_loop(listener: UnixListener, core: Sender<Cmd>) -> (AcceptGate, impl FnOnce()) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: 建一条管道，两端都设 CLOEXEC。
    unsafe {
        libc::pipe(fds.as_mut_ptr());
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let (ack, paused) = mpsc::channel();
    let ctl = fds[0];
    let run = move || {
        let _ = listener.set_nonblocking(true);
        let lfd = listener.as_raw_fd();
        let read_ctl = || {
            let mut c = 0u8;
            // SAFETY: 从管道读一个字节（阻塞）。
            let n = unsafe { libc::read(ctl, (&mut c as *mut u8).cast(), 1) };
            (n == 1).then_some(c)
        };
        let mut n = 0u64;
        loop {
            let mut p = [
                libc::pollfd { fd: ctl, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: lfd, events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: 等两个描述符之一可读，不设超时。
            if unsafe { libc::poll(p.as_mut_ptr(), 2, -1) } < 0 {
                continue;
            }
            // 先看暂停：此时队列里的连接不接，留给新进程。
            if p[0].revents != 0 {
                if read_ctl() == Some(b'P') {
                    let _ = ack.send(());
                    while !matches!(read_ctl(), Some(b'R') | None) {}
                }
                continue;
            }
            // 每次只接一个，再回去看一眼暂停。监听 socket 是非阻塞的：对端已经断开时不会卡在这里。
            match listener.accept() {
                Ok((conn, _)) => {
                    let _ = conn.set_nonblocking(false);
                    n += 1;
                    spawn(conn, n, core.clone());
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {}
                Err(e) => log!("accept 失败：{e}"),
            }
        }
    };
    (AcceptGate { ctl: fds[1], paused }, run)
}

pub fn spawn(stream: UnixStream, conn: u64, core: Sender<Cmd>) {
    std::thread::spawn(move || serve(stream, conn, &core));
}

fn serve(stream: UnixStream, conn: u64, core: &Sender<Cmd>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let Some(hello) = read_json(&mut reader) else {
        return;
    };
    if hello["role"] == "pty" {
        // 保活终端的字节中继（M3，docs/protocol.md 第 3.1 节）：核心线程判定并在需要时起 Shell，
        // 之后的接回与中继都在本线程，不经核心线程。
        let (tx, rx) = mpsc::channel();
        if core.send(Cmd::PtyHello { hello: hello.clone(), reply: tx }).is_err() {
            return;
        }
        match rx.recv() {
            Ok(Ok((session, ok_line))) => session.attach(stream, &mut reader, &hello, &ok_line, core),
            Ok(Err(err_line)) => {
                let mut s = stream;
                let _ = s.write_all(&err_line);
            }
            Err(_) => {}
        }
        return;
    }
    if hello["role"] == "hook" {
        // Hook 来自同一构建的 scyllaui；版本不符说明窗格里还是旧 App 注入的路径，丢弃并记日志。
        if hello["protocol_version"] != PROTOCOL_VERSION {
            log!("丢弃协议版本不符的 Hook 连接：{hello}");
            return;
        }
        while let Some(frame) = read_json(&mut reader) {
            let recv_us = now_us();
            let cmd = match frame["type"].as_str() {
                Some("hook") => Cmd::Hook { frame, recv_us },
                Some("statusline") => Cmd::Statusline { frame },
                _ => continue,
            };
            if core.send(cmd).is_err() {
                break;
            }
        }
        return;
    }

    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let mut writer = stream;
    std::thread::spawn(move || {
        for out in rx {
            match out {
                Out::Line(buf) => {
                    if writer.write_all(&buf).is_err() {
                        break;
                    }
                }
                Out::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
        // 核心线程丢弃了这个连接，或对端已断开：关闭 socket，读线程随之收到 EOF。
        let _ = writer.shutdown(Shutdown::Both);
    });
    if core
        .send(Cmd::Hello {
            conn,
            hello,
            out: tx,
        })
        .is_err()
    {
        return;
    }
    while let Some(msg) = read_json(&mut reader) {
        if core.send(Cmd::Request { conn, msg }).is_err() {
            return;
        }
    }
    let _ = core.send(Cmd::Closed { conn });
}

/// 读一行 JSON。EOF 或读错误返回 None；不是合法 JSON 的行返回 `Value::Null`，由核心线程按坏请求应答。
fn read_json(reader: &mut impl BufRead) -> Option<Value> {
    let mut line = Vec::new();
    match reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut line)
    {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(serde_json::from_slice(&line).unwrap_or(Value::Null)),
    }
}
