//! PTY 宿主（M3，docs/design.md 第 7 节「实现（M3）」）：持有每个保活终端的 PTY 主端、登录 Shell 与一份
//! libghostty-vt 屏幕。数据通路不经核心线程：每个会话一个读线程（读主端 → 写 vt → 有 attach 时转发），
//! attach 的输入由它的连接线程直接写主端。核心线程只收事件（`Cmd::Pty`），读主端的前台进程组与「在提示符」
//! 原子量，从不等会话锁。帧格式见 `scyllaui_proto::pty` 与 docs/protocol.md 第 3.1 节。

pub mod spawn;
pub mod takeover;
pub mod vt;

use crate::core::Cmd;
use scyllaui_proto::now_us;
use scyllaui_proto::pty::{self as frame, read_frame, write_frame};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// 代输前等提示符最多等这么久（`Host::type_input`）。
const TYPE_PROMPT_WAIT: Duration = Duration::from_secs(3);

/// 写给 attach 的单次写入超过这么久仍写不出去（对端不读），即断开它，不让读线程一直卡在这里。
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 新终端的启动计时：从起 Shell 起，记首字节、提示符出现（OSC 133;A，Shell 配置文件已跑完）、命令开始执行
/// （OSC 133;C）、命令的首次输出与画面稳定（命令开始后的输出静默 `SETTLE` 后，取最后一次输出的时间），写一行日志即结束。
/// 只在起 Shell 后的 `LIMIT` 内计，期间读线程以 `POLL_MS` 为超时轮询，结束后恢复不设超时（空闲时不唤醒）。
struct LaunchTrace {
    start: Instant,
    /// 带 initial_input（新建 AI）：等到画面稳定；纯 Shell 等到提示符即结束。
    command: bool,
    first: Option<Duration>,
    prompt: Option<Duration>,
    run: Option<Duration>,
    run_first: Option<Duration>,
    run_last: Option<Duration>,
}

impl LaunchTrace {
    const POLL_MS: i32 = 200;
    const SETTLE: Duration = Duration::from_millis(800);
    const LIMIT: Duration = Duration::from_secs(15);

    fn new(start: Instant, command: bool) -> LaunchTrace {
        LaunchTrace { start, command, first: None, prompt: None, run: None, run_first: None, run_last: None }
    }

    /// 读到一批输出后调用（已写入 vt，回调置好了标志）。
    fn output(&mut self, sink: &vt::Sink) {
        let at = self.start.elapsed();
        self.first.get_or_insert(at);
        if sink.prompt_start.swap(false, Ordering::Relaxed) {
            self.prompt.get_or_insert(at);
        }
        let started_now = sink.output_start.swap(false, Ordering::Relaxed) && self.run.is_none();
        if started_now {
            self.run = Some(at);
        } else if self.run.is_some() {
            self.run_first.get_or_insert(at);
            self.run_last = Some(at);
        }
    }

    fn finished(&self) -> bool {
        let now = self.start.elapsed();
        if now >= Self::LIMIT {
            return true;
        }
        if !self.command {
            return self.prompt.is_some();
        }
        self.run_last.is_some_and(|last| now - last >= Self::SETTLE)
    }

    fn report(&self, id: &str) {
        let ms = |d: Option<Duration>| d.map_or("-".to_owned(), |d| d.as_millis().to_string());
        log!(
            "启动计时：terminal={id} 命令={} 首字节={} 提示符={} 命令开始={} 命令首次输出={} 画面稳定={}（ms，自起 Shell）",
            self.command,
            ms(self.first),
            ms(self.prompt),
            ms(self.run),
            ms(self.run_first),
            ms(self.run_last)
        );
    }
}

/// 读线程交给核心线程的事件。
#[derive(Debug)]
pub enum Event {
    /// vt 收到 OSC 0/2 标题。
    Title,
    /// vt 收到 OSC 7，已按 Ghostty 的规则解析为本机的绝对路径。
    Cwd(String),
    /// vt 收到 OSC 133 命令结束。
    CommandEnd,
    /// attach 已连上并收到快照。
    Attached,
    /// 子进程进不了启动目录（多半是没有文件访问权限，风险 14）；`errno` 为 chdir 的错误号。
    ChdirFailed(i32),
    /// 读线程意外结束（panic）：这个会话已不可用。
    Failed,
}

/// 断开期间代答查询所用的外观（`appearance.set`）。
#[derive(Clone, Default)]
pub struct Appearance {
    /// 0 未知（不应答），1 浅色，2 深色。
    pub color_scheme: u8,
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
}

pub struct Host {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    appearance: Mutex<Appearance>,
    appearance_gen: AtomicU64,
    events: Sender<Cmd>,
    /// 接管前冻结读线程时，各读线程停下后在这里报到（终端 ID）。
    parked: Mutex<Option<std::sync::mpsc::Sender<String>>>,
}

/// 唤醒管道里的指令：结束会话、冻结（接管前停止读主端）、解冻（接管失败后恢复）。
const WAKE_END: u8 = b'x';
const WAKE_FREEZE: u8 = b'f';
const WAKE_THAW: u8 = b'u';

pub struct Session {
    pub id: String,
    pub pid: libc::pid_t,
    master: RawFd,
    sink: Box<vt::Sink>,
    inner: Mutex<Inner>,
    /// 最近一次写入 vt 后的「在提示符」（Ghostty `cursorIsAtPrompt`），供核心线程不拿锁读取。
    at_prompt: AtomicBool,
    /// 唤醒读线程结束会话的管道：[读端, 写端]。
    wake: [RawFd; 2],
    /// 会话已被结束（`Host::end`）：Shell 的退出码，未知时为 None。
    ended: Mutex<Option<Option<i32>>>,
    host: Weak<Host>,
}

struct Inner {
    vt: vt::Vt,
    client: Option<(u64, UnixStream)>,
    generation: u64,
    attaches: u64,
    /// 已应用到 vt 的外观版本。
    appearance_gen: u64,
    /// 读线程已收尾（发过 EXIT）：之后连上的 attach 直接收到 EXIT。
    finished: Option<i32>,
}

impl Host {
    pub fn new(events: Sender<Cmd>) -> Arc<Host> {
        Arc::new(Host {
            sessions: Mutex::default(),
            appearance: Mutex::default(),
            appearance_gen: AtomicU64::new(1),
            events,
            parked: Mutex::new(None),
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn has(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// 哪个会话的 Shell（login）是这个进程。
    pub fn find_by_pid(&self, pid: i32) -> Option<String> {
        self.sessions.lock().unwrap().values().find(|s| s.pid == pid).map(|s| s.id.clone())
    }

    pub fn foreground_pgid(&self, id: &str) -> Option<i32> {
        self.get(id).and_then(|s| spawn::foreground_pgid(s.master))
    }

    /// 把字节原样写进 PTY，等同键盘输入（与 `initial_input` 同一做法）：不经终端的粘贴，Shell 开着括号粘贴时
    /// 其中的回车照常执行。在单独的线程里写，不占核心线程（主端是阻塞写，Shell 停住、输入队列满时会卡住）；
    /// 先等 Shell 画出提示符（OSC 133），最多等 `TYPE_PROMPT_WAIT`，等不到（没有 Shell 集成）也照写，按预输入处理。
    /// 没有这个会话时返回 false。
    /// 2026-11-14（第二轮长跑缺陷 C）：没等到提示符时再看一眼前台，已经是某款 AI 就不写（等待期间有人在这个终端里
    /// 起了 AI，命令会被当成提示词）；写之前先发 Ctrl+U 清掉输入行上已有的字符——提示符出现之前它是终端的行删除，
    /// 之后是 Shell 行编辑器的行删除（zsh、bash、fish 相同）。
    pub fn type_input(&self, id: &str, bytes: Vec<u8>) -> bool {
        let Some(s) = self.get(id) else { return false };
        std::thread::spawn(move || {
            let until = Instant::now() + TYPE_PROMPT_WAIT;
            while !s.at_prompt.load(Ordering::Acquire) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            let at_prompt = s.at_prompt.load(Ordering::Acquire);
            if !at_prompt {
                let fg = spawn::foreground_pgid(s.master).map(|pgid| crate::procinfo::classify_full(pgid).0);
                if matches!(fg, Some(crate::procinfo::Foreground::Agent { .. })) {
                    return log!("代输取消：terminal={} 等提示符期间前台已是 AI", s.id);
                }
            }
            log!("代输：terminal={} 在提示符={at_prompt}", s.id);
            let mut line = vec![0x15];
            line.extend_from_slice(&bytes);
            vt::write_all_fd(s.master, &line);
        });
        true
    }

    pub fn at_prompt(&self, id: &str) -> Option<bool> {
        self.get(id).map(|s| s.at_prompt.load(Ordering::Acquire))
    }

    /// 起 Shell 并开始读输出。`initial_input` 在确认 exec 成功后写入 PTY；`chdir` 失败时不写（风险 14）。
    pub fn spawn(
        self: &Arc<Host>,
        id: &str,
        spec: &spawn::SpawnSpec,
        initial_input: Option<Vec<u8>>,
    ) -> std::io::Result<Arc<Session>> {
        let trace = LaunchTrace::new(Instant::now(), initial_input.is_some());
        let spawned = spawn::spawn(spec)?;
        let size = (spec.cols, spec.rows, spec.cell_w, spec.cell_h);
        let session = match self.install(id, spawned.master, spawned.pid, size, None, Some(trace)) {
            Ok(s) => s,
            Err(e) => {
                unsafe {
                    libc::kill(spawned.pid, libc::SIGHUP);
                    libc::close(spawned.master);
                    libc::close(spawned.report);
                }
                return Err(e);
            }
        };
        log!(
            "起 Shell：terminal={id} pid={} {}x{} cwd={}",
            spawned.pid,
            spec.cols,
            spec.rows,
            spec.cwd
        );
        let (report, master, events, tid) = (spawned.report, spawned.master, self.events.clone(), id.to_owned());
        let keep = Arc::clone(&session);
        std::thread::spawn(move || {
            let problems = spawn::read_report(report);
            for &(kind, errno) in &problems {
                let what = if kind == spawn::REPORT_CHDIR { "进入启动目录" } else { "启动程序" };
                log!("终端 {tid} {what}失败：{}", std::io::Error::from_raw_os_error(errno));
            }
            if let Some(&(_, errno)) = problems.iter().find(|(k, _)| *k == spawn::REPORT_CHDIR) {
                let _ = events.send(Cmd::Pty { terminal_id: tid, event: Event::ChdirFailed(errno) });
            } else if problems.is_empty() {
                if let Some(input) = initial_input {
                    vt::write_all_fd(master, &input);
                }
            }
            drop(keep);
        });
        Ok(session)
    }

    /// 建会话对象并开始读输出（新起的 Shell，或接管来的主端）。`replay` 为接管时回放进新 vt 的快照。
    fn install(
        self: &Arc<Host>,
        id: &str,
        master: RawFd,
        pid: i32,
        (cols, rows, cell_w, cell_h): (u16, u16, u32, u32),
        replay: Option<(&[u8], bool)>,
        trace: Option<LaunchTrace>,
    ) -> std::io::Result<Arc<Session>> {
        let mut wake = [-1; 2];
        unsafe {
            if libc::pipe(wake.as_mut_ptr()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in wake {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let sink = Box::new(vt::Sink::new(master));
        let mut vt = vt::Vt::new(cols.max(1), rows.max(1), &*sink);
        sink.cols.store(cols as u32, Ordering::Relaxed);
        sink.rows.store(rows as u32, Ordering::Relaxed);
        sink.cell_w.store(cell_w, Ordering::Relaxed);
        sink.cell_h.store(cell_h, Ordering::Relaxed);
        vt.resize(cols, rows, cell_w, cell_h);
        let mut at_prompt = false;
        if let Some((bytes, prompt)) = replay {
            // 回放时 vt 发出的应答（快照里没有查询，应不发生）不写回 PTY。
            sink.attached.store(true, Ordering::Release);
            vt.write(bytes);
            sink.attached.store(false, Ordering::Release);
            for f in [&sink.title_changed, &sink.pwd_changed, &sink.command_end] {
                f.store(false, Ordering::Relaxed);
            }
            // 快照不回放 OSC 133 标记，「在提示符」沿用旧进程的值，直到下一次输出。
            at_prompt = prompt;
        }
        // 等待线程：阻塞在 waitpid 上（空闲时不唤醒），子进程一退出就回收并把退出码交给核心线程。不用 kqueue 的
        // 退出通知加非阻塞回收：通知可能早于子进程变为可回收，会漏收、留下僵尸（2026-10-17 实测）。接管后 Shell 仍是
        // 本进程（同一进程号）的子进程，同样可以等。
        let events = self.events.clone();
        std::thread::spawn(move || {
            let code = wait_child(pid);
            let _ = events.send(Cmd::ShellExited { pid, code });
        });
        let session = Arc::new(Session {
            id: id.to_owned(),
            pid,
            master,
            sink,
            inner: Mutex::new(Inner {
                vt,
                client: None,
                generation: 0,
                attaches: 0,
                appearance_gen: 0,
                finished: None,
            }),
            at_prompt: AtomicBool::new(at_prompt),
            wake,
            ended: Mutex::new(None),
            host: Arc::downgrade(self),
        });
        self.sessions.lock().unwrap().insert(id.to_owned(), Arc::clone(&session));
        let s = Arc::clone(&session);
        let events = self.events.clone();
        std::thread::spawn(move || {
            let id = s.id.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pump(&s, &events, trace))).is_err() {
                log!("终端 {id} 的读线程 panic，结束该会话");
                let _ = events.send(Cmd::Pty { terminal_id: id, event: Event::Failed });
            }
        });
        Ok(session)
    }

    /// 结束会话：从表中移除，唤醒读线程发出 EXIT 并关闭 attach。主端在最后一个引用释放时关闭。
    pub fn end(&self, id: &str, code: Option<i32>) {
        let Some(s) = self.sessions.lock().unwrap().remove(id) else {
            return;
        };
        *s.ended.lock().unwrap() = Some(code);
        s.wake(WAKE_END);
    }

    /// 接管前：让所有读线程停止读主端并报到（最多等 `timeout`）。之后主端里的输出留在内核缓冲区，交给新进程读。
    pub fn freeze(&self, timeout: Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.parked.lock().unwrap() = Some(tx);
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        for s in &sessions {
            s.wake(WAKE_FREEZE);
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut left = sessions.len();
        while left > 0 {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(_) => left -= 1,
                Err(_) => break,
            }
        }
        *self.parked.lock().unwrap() = None;
        left == 0
    }

    pub fn thaw(&self) {
        for s in self.sessions.lock().unwrap().values() {
            s.wake(WAKE_THAW);
        }
    }

    /// 冻结后导出接管所需的会话状态（每个会话的快照在其锁内生成）。
    pub fn export(&self) -> Vec<takeover::Entry> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        sessions
            .iter()
            .map(|s| {
                let inner = s.inner.lock().unwrap();
                takeover::Entry {
                    terminal_id: s.id.clone(),
                    master_fd: s.master,
                    pid: s.pid,
                    cols: s.sink.cols.load(Ordering::Relaxed) as u16,
                    rows: s.sink.rows.load(Ordering::Relaxed) as u16,
                    cell_w: s.sink.cell_w.load(Ordering::Relaxed),
                    cell_h: s.sink.cell_h.load(Ordering::Relaxed),
                    snapshot: inner.vt.snapshot().bytes,
                    at_prompt: s.at_prompt.load(Ordering::Acquire),
                    // 核心线程的内存状态由 Core::upgrade 填入。
                    notice: None,
                    current_dir: None,
                    agent: None,
                }
            })
            .collect()
    }

    /// 新进程接管旧进程的会话：沿用主端，回放快照，开始读输出。返回接到的会话（连同要交给核心还原的内存状态）。
    pub fn adopt(self: &Arc<Host>, h: takeover::Handoff) -> Vec<takeover::Adopted> {
        let a = h.appearance;
        self.set_appearance(|x| *x = a);
        let mut out = Vec::new();
        for e in h.entries {
            takeover::set_cloexec(e.master_fd, true);
            match self.install(&e.terminal_id, e.master_fd, e.pid, (e.cols, e.rows, e.cell_w, e.cell_h), Some((&e.snapshot, e.at_prompt)), None) {
                Ok(_) => out.push(takeover::Adopted { terminal_id: e.terminal_id, notice: e.notice, current_dir: e.current_dir, agent: e.agent }),
                Err(err) => log!("接管终端 {} 失败：{err}", e.terminal_id),
            }
        }
        out
    }

    /// 关闭终端：照 Ghostty `killCommand` 对 Shell 的进程组发 SIGHUP（Darwin 上对 root 的 login 会得到 EPERM，
    /// 与 Ghostty 一样忽略），再结束会话、关闭主端（内核随之向会话发 SIGHUP）。不等进程退出，回收在退出通知里做。
    pub fn hangup(&self, id: &str) {
        if let Some(s) = self.get(id) {
            unsafe {
                let pgid = libc::getpgid(s.pid);
                if pgid > 0 {
                    libc::killpg(pgid, libc::SIGHUP);
                }
            }
        }
        self.end(id, None);
    }

    pub fn hangup_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.hangup(&id);
        }
    }

    pub fn set_appearance(&self, f: impl FnOnce(&mut Appearance)) {
        let scheme = {
            let mut a = self.appearance.lock().unwrap();
            f(&mut a);
            a.color_scheme
        };
        self.appearance_gen.fetch_add(1, Ordering::AcqRel);
        for s in self.sessions.lock().unwrap().values() {
            s.sink.color_scheme.store(scheme, Ordering::Relaxed);
        }
    }

    pub fn appearance(&self) -> Appearance {
        self.appearance.lock().unwrap().clone()
    }
}

impl Session {
    fn wake(&self, cmd: u8) {
        unsafe { libc::write(self.wake[1], &cmd as *const u8 as *const libc::c_void, 1) };
    }

    fn apply_appearance(&self, inner: &mut Inner) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let gen = host.appearance_gen.load(Ordering::Acquire);
        if inner.appearance_gen == gen {
            return;
        }
        inner.appearance_gen = gen;
        let a = host.appearance();
        self.sink.color_scheme.store(a.color_scheme, Ordering::Relaxed);
        inner.vt.set_colors(a.foreground.unwrap_or([0xff, 0xff, 0xff]), a.background.unwrap_or([0x28, 0x2c, 0x34]));
    }

    fn apply_size(&self, inner: &mut Inner, (cols, rows, cw, ch): (u16, u16, u32, u32)) {
        self.sink.cols.store(cols as u32, Ordering::Relaxed);
        self.sink.rows.store(rows as u32, Ordering::Relaxed);
        self.sink.cell_w.store(cw, Ordering::Relaxed);
        self.sink.cell_h.store(ch, Ordering::Relaxed);
        spawn::set_size(self.master, cols, rows, cw, ch);
        inner.vt.resize(cols, rows, cw, ch);
    }

    /// 在 attach 的连接线程里执行：按其尺寸调整 PTY 与 vt，发 `hello_ok` 与快照，之后转发它的输入，直到断开。
    /// 同一终端的新连接接管旧连接。`reader` 是读过 hello 的同一个缓冲读端。
    pub fn attach(&self, mut stream: UnixStream, reader: &mut impl Read, hello: &Value, ok_line: &[u8], events: &Sender<Cmd>) {
        let _ = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let my_gen;
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(code) = inner.finished {
                let _ = stream.write_all(ok_line);
                let _ = write_frame(&mut stream, frame::EXIT, json!({"code": code}).to_string().as_bytes());
                return;
            }
            self.apply_appearance(&mut inner);
            self.apply_size(&mut inner, size_of(hello));
            let t0 = now_us();
            let snap = inner.vt.snapshot();
            let snap_us = now_us() - t0;
            let sent = stream.write_all(ok_line).and_then(|_| write_frame(&mut stream, frame::SNAPSHOT, &snap.bytes));
            if sent.is_err() {
                return;
            }
            if let Some((_, old)) = inner.client.take() {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
            inner.generation += 1;
            inner.attaches += 1;
            my_gen = inner.generation;
            self.sink.attached.store(true, Ordering::Release);
            let replies = self.sink.detached_replies.swap(0, Ordering::Relaxed);
            log!(
                "attach 连上：terminal={} 第 {} 次 快照 {} 字节 {} us alt={} scrollback={} fallback={} selfcheck={} 断开期间代答 {replies} 次",
                self.id, inner.attaches, snap.bytes.len(), snap_us, snap.alt_screen, snap.scrollback_rows, snap.fallback, snap.selfcheck_ok
            );
            match stream.try_clone() {
                Ok(w) => inner.client = Some((my_gen, w)),
                Err(_) => return,
            }
        }
        let _ = events.send(Cmd::Pty { terminal_id: self.id.clone(), event: Event::Attached });

        while let Ok((kind, payload)) = read_frame(reader) {
            match kind {
                frame::INPUT => vt::write_all_fd(self.master, &payload),
                frame::RESIZE => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                        let mut inner = self.inner.lock().unwrap();
                        self.apply_size(&mut inner, size_of(&v));
                    }
                }
                _ => {}
            }
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.client.as_ref().is_some_and(|(g, _)| *g == my_gen) {
            inner.client = None;
            self.sink.attached.store(false, Ordering::Release);
            log!("attach 断开：terminal={}，进入后台持有", self.id);
        }
    }

    /// 读一段输出：写 vt、更新「在提示符」、有 attach 时转发；取走回调置的标志，锁外通知核心线程。
    fn feed(&self, data: &[u8], events: &Sender<Cmd>) {
        let mut cwd = None;
        {
            let mut inner = self.inner.lock().unwrap();
            self.apply_appearance(&mut inner);
            inner.vt.write(data);
            self.at_prompt.store(inner.vt.cursor_at_prompt(), Ordering::Release);
            if self.sink.pwd_changed.swap(false, Ordering::Relaxed) {
                cwd = parse_pwd_url(&inner.vt.pwd());
            }
            if let Some((_, c)) = inner.client.as_mut() {
                if write_frame(c, frame::OUTPUT, data).is_err() {
                    let (_, c) = inner.client.take().unwrap();
                    let _ = c.shutdown(std::net::Shutdown::Both);
                    self.sink.attached.store(false, Ordering::Release);
                    log!("attach 写不进去，断开：terminal={}", self.id);
                }
            }
        }
        let send = |event| {
            let _ = events.send(Cmd::Pty { terminal_id: self.id.clone(), event });
        };
        if self.sink.title_changed.swap(false, Ordering::Relaxed) {
            send(Event::Title);
        }
        if let Some(cwd) = cwd {
            send(Event::Cwd(cwd));
        }
        if self.sink.command_end.swap(false, Ordering::Relaxed) {
            send(Event::CommandEnd);
        }
    }

    /// 会话被结束后：读完主端里剩下的输出，给 attach 发 EXIT 并关闭它。
    fn finish(&self, events: &Sender<Cmd>, eof: bool) {
        let mut buf = vec![0u8; 64 * 1024];
        while !eof && readable(self.master, 0) {
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            self.feed(&buf[..n as usize], events);
        }
        let code = self.ended.lock().unwrap().flatten().unwrap_or(0);
        let mut inner = self.inner.lock().unwrap();
        inner.finished = Some(code);
        if let Some((_, mut c)) = inner.client.take() {
            let _ = write_frame(&mut c, frame::EXIT, json!({"code": code}).to_string().as_bytes());
            let _ = c.flush();
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        self.sink.attached.store(false, Ordering::Release);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.master);
            libc::close(self.wake[0]);
            libc::close(self.wake[1]);
        }
    }
}

/// 读线程：poll 主端与唤醒管道，不设超时，空闲时不唤醒（启动计时期间例外，见 `LaunchTrace`）。主端读到 EOF
/// （所有从端都已关闭）后只等唤醒。
fn pump(s: &Session, events: &Sender<Cmd>, mut trace: Option<LaunchTrace>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut eof = false;
    loop {
        let mut fds = [
            libc::pollfd { fd: s.wake[0], events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if eof { -1 } else { s.master }, events: libc::POLLIN, revents: 0 },
        ];
        let timeout = if trace.is_some() { LaunchTrace::POLL_MS } else { -1 };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            if trace.as_ref().is_some_and(LaunchTrace::finished) {
                trace.take().unwrap().report(&s.id);
            }
            continue;
        }
        if fds[0].revents != 0 {
            match read_wake(s.wake[0]) {
                WAKE_FREEZE => {
                    // 停止读主端，报到后只等唤醒管道：解冻则继续，结束则收尾（接管时进程直接 exec，不会回来）。
                    let tx = s.host.upgrade().and_then(|h| h.parked.lock().unwrap().clone());
                    if let Some(tx) = tx {
                        let _ = tx.send(s.id.clone());
                    }
                    loop {
                        match read_wake(s.wake[0]) {
                            WAKE_THAW => break,
                            WAKE_END => return s.finish(events, eof),
                            _ => {}
                        }
                    }
                    continue;
                }
                WAKE_THAW => continue,
                _ => break,
            }
        }
        if fds[1].revents != 0 {
            let n = unsafe { libc::read(s.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n <= 0 {
                eof = true;
                continue;
            }
            // 主端里此刻已有的数据接着读完（最多一个缓冲区），合成一批再写 vt、转发一帧：PTY 每次读只给一小块，
            // 逐块转发时每块都要经过 attach 再到 Surface，大量输出时吞吐明显下降（2026-10-17 M3 验收实测）。
            let mut len = n as usize;
            while len < buf.len() && readable(s.master, 0) {
                let m = unsafe { libc::read(s.master, buf[len..].as_mut_ptr() as *mut libc::c_void, buf.len() - len) };
                if m <= 0 {
                    break;
                }
                len += m as usize;
            }
            s.feed(&buf[..len], events);
            if let Some(t) = trace.as_mut() {
                t.output(&s.sink);
                if t.finished() {
                    trace.take().unwrap().report(&s.id);
                }
            }
        }
    }
    s.finish(events, eof);
}

/// 阻塞等待子进程退出并回收，返回退出码（被信号结束为 128 + 信号号，与 Shell 的约定相同）；等不到为 None。
fn wait_child(pid: i32) -> Option<i32> {
    let mut status = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            break;
        }
        if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        None
    }
}

/// 读唤醒管道的一个字节（会阻塞到有指令）；读失败按结束处理。
fn read_wake(fd: RawFd) -> u8 {
    let mut b = 0u8;
    loop {
        let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        if n == 1 {
            return b;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return WAKE_END;
    }
}

fn readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut p, 1, timeout_ms) == 1 && p.revents & libc::POLLIN != 0 }
}

pub fn size_of(v: &Value) -> (u16, u16, u32, u32) {
    let n = |k: &str, d: u64| v.get(k).and_then(Value::as_u64).unwrap_or(d);
    (
        n("cols", 80).clamp(1, u16::MAX as u64) as u16,
        n("rows", 24).clamp(1, u16::MAX as u64) as u16,
        n("cell_w", 0).min(u32::MAX as u64) as u32,
        n("cell_h", 0).min(u32::MAX as u64) as u32,
    )
}

/// OSC 7 的值按 Ghostty 的规则（`src/termio/stream_handler.zig` reportPwd）解析为路径：只认 `file://` 与
/// `kitty-shell-cwd://`，主机名须为 `localhost` 或本机名（不允许为空）；`file://` 的路径做百分号解码，
/// `kitty-shell-cwd://` 原样。解析不了返回 None（与 Ghostty 一样忽略）。
pub fn parse_pwd_url(url: &str) -> Option<String> {
    let (rest, raw) = if let Some(r) = url.strip_prefix("file://") {
        (r, false)
    } else if let Some(r) = url.strip_prefix("kitty-shell-cwd://") {
        (r, true)
    } else {
        return None;
    };
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    if !is_local_host(host) {
        return None;
    }
    if raw {
        return Some(path.to_owned());
    }
    let b = path.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_local_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return false;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    !host.is_empty() && host.as_bytes() == &buf[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hostname() -> String {
        let mut buf = [0u8; 256];
        unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        let end = buf.iter().position(|&c| c == 0).unwrap();
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }

    #[test]
    fn pwd_url_follows_ghostty() {
        assert_eq!(parse_pwd_url("file://localhost/tmp/a%20b").as_deref(), Some("/tmp/a b"));
        let h = hostname();
        assert_eq!(parse_pwd_url(&format!("file://{h}/Users/x/%E6%96%87")).as_deref(), Some("/Users/x/文"));
        assert_eq!(parse_pwd_url("kitty-shell-cwd://localhost/tmp/a%20b").as_deref(), Some("/tmp/a%20b"));
        // 空主机、别的主机、别的协议、没有路径：忽略。
        assert_eq!(parse_pwd_url("file:///tmp"), None);
        assert_eq!(parse_pwd_url("file://other-host-xyz/tmp"), None);
        assert_eq!(parse_pwd_url("http://localhost/tmp"), None);
        assert_eq!(parse_pwd_url("file://localhost"), None);
        assert_eq!(parse_pwd_url("/tmp"), None);
        assert_eq!(parse_pwd_url("file://localhost/bad%zz"), None);
    }
}
