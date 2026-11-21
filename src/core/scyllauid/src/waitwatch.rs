//! 监听会话记录（M2 第 1 块，docs/design.md 4.3 节「实现（M2 第 1 块）」、docs/protocol.md 第 7 节）：
//! 适配器声明了 `waiting_scan` 的工具，终端进入「待审批」（声明了 `scan_while_running` 的也包括运行中）时由核心线程
//! 请求监听该对话的会话记录，从当时的文件末尾起逐行交给适配器判定；认出了就把对应的统一事件发给核心线程。
//! Antigravity 的记录是 SQLite 对话库（M4b 块 9b）：盯库文件、`-wal` 与所在目录，有动静就只读查询一次交给适配器判定。
//! 认出中断或一轮结束（回到 Ready）即停止监听；认出批准、需确认继续监听，等待结束由核心线程叫停。
//! 专门的线程阻塞在 kevent 上，只在被监听的文件有写入或核心线程发来请求时醒来，空闲不唤醒。
//! 三条通用规则（2026-11-14，docs/design.md 4.3 节「会话记录监听」）：记录的定位与打开在本线程做，打不开时每
//! `RETRY_EVERY` 重试、最多 `RETRY_FOR`（只在有待打开的监听时才定时醒来），打开成功或放弃都回报核心线程；逐行记录
//! 开始时补读末尾 `REPLAY_MAX` 字节对齐此前已写入的内容；带轮次号的事件把轮次交给核心线程核对，不自行停止监听。
//! 另一种监听（`Scan::Submits`，docs/design.md 4.7 节「用会话记录对账」）：只找开始监听之后写入的、用户提交的提示词，
//! 每条报给核心线程，由它核对对应的 Hook 事件到了没有；不补读，一直监听到核心线程叫停或记录没了。

use crate::core::Cmd;
use scyllaui_proto::adapters::prompts::user_prompts;
use scyllaui_proto::adapters::reply::ReplySource;
use scyllaui_proto::adapters::waiting::{agy_run, antigravity, AgyRead, LineScan, Scan, WaitScan, AGY_STEP_WAITING};
use scyllaui_proto::Kind;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

/// EVFILT_USER 的标识：核心线程发来了新请求。
const NUDGE: usize = 1;
/// 记录还打不开（新会话的记录文件、索引行稍后才写出来）时的重试间隔与最长时间。
const RETRY_EVERY: Duration = Duration::from_millis(200);
const RETRY_FOR: Duration = Duration::from_secs(10);
/// 开始监听时补读的末尾字节数上限。
const REPLAY_MAX: u64 = 256 * 1024;
/// 与主记录一起监听的其他记录的个数上限。
const EXTRA_MAX: usize = 8;

/// 会话记录在哪里。
#[derive(Clone, Debug)]
pub enum Source {
    Path(PathBuf),
    /// Kimi：按 `<home>/.kimi-code/session_index.jsonl` 查会话目录，其下的 `agents/main/wire.jsonl`。
    KimiIndex { home: PathBuf, session: String },
    /// Grok：`<home>/.grok/sessions/<启动目录>/<会话>/events.jsonl`，在各启动目录下找。
    GrokSessions { home: PathBuf, session: String },
}

impl Source {
    fn resolve(&self) -> Option<PathBuf> {
        match self {
            Source::Path(p) => Some(p.clone()),
            Source::KimiIndex { home, session } => {
                Some(crate::title::kimi_session_dir(home, session)?.join("agents/main/wire.jsonl"))
            }
            Source::GrokSessions { home, session } => Some(crate::title::grok_session_dir(home, session)?.join("events.jsonl")),
        }
    }

    /// 与主记录一起监听的其他记录。Grok：还在跑的子代理各自的会话记录——子代理的命令审批只在主代理等它输出时才
    /// 写进主会话的记录，否则只写在子会话自己的 `events.jsonl` 里（2026-11-14 实测 1.0.77）。子会话由主会话目录下
    /// `subagents/<ID>/meta.json` 的 `child_session_id` 给出，与主会话目录同级；最多取最近的 `EXTRA_MAX` 个。
    fn extra(&self, main: &Path) -> Vec<PathBuf> {
        let Source::GrokSessions { .. } = self else { return Vec::new() };
        let Some(dir) = main.parent() else { return Vec::new() };
        let Some(root) = dir.parent() else { return Vec::new() };
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir.join("subagents"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let meta = e.path().join("meta.json");
                let v: Value = serde_json::from_slice(&std::fs::read(&meta).ok()?).ok()?;
                if v["status"].as_str().is_some_and(|s| s != "running") {
                    return None;
                }
                let child = Path::new(v["child_session_id"].as_str()?).file_name()?.to_owned();
                let events = root.join(child).join("events.jsonl");
                Some((std::fs::metadata(&meta).and_then(|m| m.modified()).ok()?, events))
            })
            .collect();
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.into_iter().take(EXTRA_MAX).map(|(_, p)| p).collect()
    }
}

/// 一次监听的请求。
pub struct Start {
    /// 这次监听的标识（一个终端可以同时有几个监听：会话记录、进程状态文件），停止时按它。
    pub key: String,
    pub terminal_id: String,
    pub session_id: Option<String>,
    pub source: Source,
    pub scan: Scan,
    /// 窗格当前的轮次标识（Kimi 为收到提问的时刻）：补读按它判断记录里的结束是不是这一轮的，认出的事件也带着它交给
    /// 核心核对。
    pub turn: Option<String>,
    /// 窗格此刻在待审批。
    pub waiting: bool,
    /// 换版本接管后的对账：此前可能漏了事件，补读的结论里包括空档中的批准与中断。
    pub resync: bool,
    /// 这份记录是子代理的（Codex：子代理的审批指向子线程的记录）。
    pub sub: bool,
    /// 认出的事件属于哪个子代理（Antigravity 子代理的对话库）：核心按这个子代理的事件处理；这种监听认出一轮结束
    /// 后不自行停止（同一个子代理还会接到新任务）。
    pub agent: Option<String>,
}

pub enum WaitJob {
    Start(Start),
    Stop { key: String },
}

pub struct WaitWatch {
    tx: Sender<WaitJob>,
    kq: Option<RawFd>,
}

impl WaitWatch {
    pub fn new(core: Sender<Cmd>) -> std::io::Result<WaitWatch> {
        // SAFETY: kqueue 无参数；描述符由本结构与监听线程共用，进程生命周期内不关闭。
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let nudge = libc::kevent {
            ident: NUDGE,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: 只注册一个事件。
        if unsafe { libc::kevent(kq, &nudge, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || run(kq, rx, core));
        Ok(WaitWatch { tx, kq: Some(kq) })
    }

    /// 测试用：请求照收，但不监听。
    #[cfg(test)]
    pub fn inert() -> WaitWatch {
        let (tx, rx) = mpsc::channel();
        std::mem::forget(rx);
        WaitWatch { tx, kq: None }
    }

    pub fn start(&self, job: Start) {
        self.send(WaitJob::Start(job));
    }

    /// 测试用：按路径监听，不带轮次、不对账。
    #[cfg(test)]
    fn start_path(&self, terminal_id: &str, session_id: Option<String>, path: PathBuf, scan: Scan) {
        self.start(Start { key: terminal_id.to_owned(), terminal_id: terminal_id.to_owned(), session_id, source: Source::Path(path), scan, turn: None, waiting: false, resync: false, sub: false, agent: None });
    }

    pub fn stop(&self, key: &str) {
        self.send(WaitJob::Stop { key: key.to_owned() });
    }

    fn send(&self, job: WaitJob) {
        let _ = self.tx.send(job);
        let Some(kq) = self.kq else { return };
        let ev = libc::kevent {
            ident: NUDGE,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: 触发已注册的用户事件。
        unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }
}

struct Entry {
    terminal_id: String,
    session_id: Option<String>,
    agent: Option<String>,
    state: WaitScan,
    watch: Watch,
}

enum Watch {
    /// JSON Lines：从开始监听时的文件末尾起逐行判定。
    /// 几个文件（主记录与 `Source::extra` 给出的）共用一份判定状态。
    Lines { files: Vec<LineFile>, scan: LineScan },
    /// 整份 JSON、原地改写：每次有写入读出整份判定。
    Whole { fd: RawFd, scan: LineScan },
    /// Antigravity 对话库：盯所在目录（`-wal` 的建与删）、库文件与 `-wal`（有写入）；agy 空闲时关掉库、`-wal` 随之删除
    /// （2026-11-01 实测 1.4.12），所以这两个随时可能不在，按目录的变化重开。
    Db { path: PathBuf, dir: RawFd, db: Option<RawFd>, wal: Option<RawFd> },
    /// 用户提交的提示词：从开始监听时的文件末尾起读，不补读。
    Submits { file: LineFile, source: ReplySource },
}

struct LineFile {
    fd: RawFd,
    /// 子代理的记录（`Source::extra` 给出的，或请求里标明的）。
    sub: bool,
    offset: u64,
    /// 上次读到的不完整的一行。
    partial: Vec<u8>,
}

impl Entry {
    fn fds(&self) -> Vec<RawFd> {
        match &self.watch {
            Watch::Lines { files, .. } => files.iter().map(|f| f.fd).collect(),
            Watch::Whole { fd, .. } => vec![*fd],
            Watch::Submits { file, .. } => vec![file.fd],
            Watch::Db { dir, db, wal, .. } => [Some(*dir), *db, *wal].into_iter().flatten().collect(),
        }
    }
}

/// 打开一次监听。记录还定位不到或打不开时返回 None（逐行记录稍后重试）。成功时返回补读或第一次读出的结论。
fn open(kq: RawFd, job: &Start) -> Option<(Entry, Option<Kind>)> {
    let path = job.source.resolve()?;
    let mut state = WaitScan::default();
    state.session = job.session_id.clone();
    state.sub = job.sub;
    state.turn = job.turn.clone();
    let (watch, first) = match job.scan {
        Scan::Lines(f) => {
            state.replay = true;
            let mut files = vec![open_lines(kq, &path, f, &mut state)?];
            state.sub = true;
            files.extend(job.source.extra(&path).iter().filter_map(|p| open_lines(kq, p, f, &mut state)));
            state.sub = job.sub;
            let first = state.replayed(job.turn.as_deref(), job.waiting, job.resync);
            (Watch::Lines { files, scan: f }, first)
        }
        Scan::Whole(f) => {
            let fd = watch_path(kq, &path, libc::O_RDONLY)?;
            // 开始时读一次，只记下当时的状态。
            state.replay = true;
            if let Some(v) = read_whole(fd) {
                f(&mut state, &v);
            }
            let first = state.replayed(job.turn.as_deref(), job.waiting, job.resync);
            (Watch::Whole { fd, scan: f }, first)
        }
        Scan::AntigravityDb => (open_db(kq, &path)?, None),
        Scan::Submits(source) => {
            let fd = watch_path(kq, &path, libc::O_RDONLY)?;
            // SAFETY: fd 有效；fstat 写入栈上的结构体。
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 {
                close_fd(fd);
                return None;
            }
            (Watch::Submits { file: LineFile { fd, sub: false, offset: st.st_size as u64, partial: Vec::new() }, source }, None)
        }
    };
    let mut e = Entry { terminal_id: job.terminal_id.clone(), session_id: job.session_id.clone(), agent: job.agent.clone(), state, watch };
    // 对话库开始时读一次：记下执行次数，报告当时是否在等用户（与当前状态一致时核心线程不理会）。
    let first = if matches!(e.watch, Watch::Db { .. }) { judge_db(&mut e) } else { first };
    Some((e, first))
}

fn run(kq: RawFd, rx: Receiver<WaitJob>, core: Sender<Cmd>) {
    let mut entries: HashMap<String, Entry> = HashMap::new();
    // 描述符 → 终端。
    let mut owner: HashMap<RawFd, String> = HashMap::new();
    // 还没打开的监听与放弃的时间。
    let mut pending: Vec<(Start, Instant)> = Vec::new();
    loop {
        // 有待打开的监听时定时醒来重试，否则一直等。
        let wait = libc::timespec { tv_sec: 0, tv_nsec: RETRY_EVERY.as_nanos() as libc::c_long };
        let timeout = if pending.is_empty() { std::ptr::null() } else { &wait as *const libc::timespec };
        // SAFETY: 读取一个事件到栈上的结构体。
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, timeout) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if n == 1 && ev.filter == libc::EVFILT_USER {
            while let Ok(job) = rx.try_recv() {
                match job {
                    WaitJob::Start(job) => {
                        remove(&mut entries, &mut owner, &job.key);
                        pending.retain(|(p, _)| p.key != job.key);
                        pending.push((job, Instant::now() + RETRY_FOR));
                    }
                    WaitJob::Stop { key } => {
                        remove(&mut entries, &mut owner, &key);
                        pending.retain(|(p, _)| p.key != key);
                    }
                }
            }
        }
        // 打开待打开的监听：成功或到时间放弃都回报核心线程。
        let now = Instant::now();
        let mut i = 0;
        while i < pending.len() {
            let (job, until) = &pending[i];
            let opened = open(kq, job);
            // 对话库的目录都打不开时不重试（与原先一致）。
            if opened.is_none() && now < *until && !matches!(job.scan, Scan::AntigravityDb) {
                i += 1;
                continue;
            }
            let (job, _) = pending.remove(i);
            let tid = job.key.clone();
            let msg = Cmd::WatchOpened { key: tid.clone(), session_id: job.session_id.clone(), ok: opened.is_some() };
            if core.send(msg).is_err() {
                return;
            }
            let Some((e, first)) = opened else { continue };
            for fd in e.fds() {
                owner.insert(fd, tid.clone());
            }
            entries.insert(tid.clone(), e);
            if let Some(kind) = first {
                if !report(&core, &mut entries, &mut owner, &tid, kind) {
                    return;
                }
            }
        }
        if n != 1 || ev.filter == libc::EVFILT_USER {
            continue;
        }
        let fd = ev.ident as RawFd;
        let gone = ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0;
        let Some(tid) = owner.get(&fd).cloned() else { continue };
        let Some(e) = entries.get_mut(&tid) else { continue };
        let kinds: Vec<Kind> = match &mut e.watch {
            Watch::Lines { files, scan } => {
                let main = files.first().map(|f| f.fd);
                // 一次写入可能有几件事（Kimi：一轮开始与结束、上一轮结束与另开的一轮），读完再按顺序报告；只报第一件
                // 的话，后面的要等下一次写入才读到，CLI 不再写就一直停在那里（2026-11-15）。
                let mut kinds = Vec::new();
                if let Some(f) = files.iter_mut().find(|f| f.fd == fd) {
                    e.state.sub = f.sub;
                    while let Some(kind) = read_new(f.fd, &mut f.offset, &mut f.partial, *scan, &mut e.state) {
                        kinds.push(kind);
                    }
                }
                // 主记录没了才停；一起监听的其他记录没了不管。
                if kinds.is_empty() && gone && main == Some(fd) {
                    remove(&mut entries, &mut owner, &tid);
                    continue;
                }
                kinds
            }
            Watch::Whole { fd, scan } => {
                // 原地改写时可能读到半截：解析不了就等下一次写入。进程退出时文件被删，停止监听。
                let kind = read_whole(*fd).and_then(|v| scan(&mut e.state, &v));
                if kind.is_none() && gone {
                    remove(&mut entries, &mut owner, &tid);
                    continue;
                }
                kind.into_iter().collect()
            }
            Watch::Db { .. } => {
                sync_db(kq, &tid, e, fd, gone, &mut owner);
                judge_db(e).into_iter().collect()
            }
            Watch::Submits { file, source } => {
                let found = read_submits(file, *source);
                let (terminal_id, session_id) = (e.terminal_id.clone(), e.session_id.clone());
                if found.is_empty() && gone {
                    remove(&mut entries, &mut owner, &tid);
                    continue;
                }
                for at in found {
                    let msg = Cmd::RecordSubmit { terminal_id: terminal_id.clone(), session_id: session_id.clone(), at };
                    if core.send(msg).is_err() {
                        return;
                    }
                }
                Vec::new()
            }
        };
        for kind in kinds {
            // 报告了一轮结束而停止的监听，余下的不再报。
            if !entries.contains_key(&tid) {
                break;
            }
            if !report(&core, &mut entries, &mut owner, &tid, kind) {
                return;
            }
        }
    }
}

/// 把认出的事件发给核心线程；中断与一轮结束后停止监听（批准、需确认继续）。带轮次号的事件要由核心线程核对是不是
/// 当前这一轮，不在这里停，由核心线程叫停。核心线程已退出时返回 false。
fn report(core: &Sender<Cmd>, entries: &mut HashMap<String, Entry>, owner: &mut HashMap<RawFd, String>, tid: &str, kind: Kind) -> bool {
    let Some(e) = entries.get_mut(tid) else { return true };
    let (terminal_id, session_id, turn, agent) = (e.terminal_id.clone(), e.session_id.clone(), e.state.turn.clone(), e.agent.clone());
    let stop = turn.is_none() && agent.is_none() && matches!(kind, Kind::TurnInterrupt | Kind::TurnEnd);
    if core.send(Cmd::SessionRecord { terminal_id, session_id, kind, turn, agent }).is_err() {
        return false;
    }
    if stop {
        remove(entries, owner, tid);
    }
    true
}

fn remove(entries: &mut HashMap<String, Entry>, owner: &mut HashMap<RawFd, String>, tid: &str) {
    if let Some(e) = entries.remove(tid) {
        for fd in e.fds() {
            owner.remove(&fd);
            close_fd(fd);
        }
    }
}

fn close_fd(fd: RawFd) {
    // SAFETY: 关闭本线程打开的描述符；kqueue 随之移除对应事件。
    unsafe { libc::close(fd) };
}

/// 打开并注册监听；`flags` 为 open 的标志。打不开时返回 None。
fn watch_path(kq: RawFd, path: &Path, flags: libc::c_int) -> Option<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: 路径为以 NUL 结尾的 C 字符串；只读或只用于事件的打开。
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let ev = libc::kevent {
        ident: fd as libc::uintptr_t,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: 同上；只注册一个事件，不取回事件。
    if unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
        close_fd(fd);
        return None;
    }
    Some(fd)
}

/// 读出整份文件并解析（最多 1 MB）；读不到或不是 JSON 为 None。
fn read_whole(fd: RawFd) -> Option<Value> {
    let mut buf = vec![0u8; 1 << 20];
    // SAFETY: 读入本函数持有的缓冲区，长度不超过其大小。
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
    (n > 0).then(|| serde_json::from_slice(&buf[..n as usize]).ok()).flatten()
}

/// 打开文件、注册监听，补读末尾 `REPLAY_MAX` 字节里完整的行（只记状态，`state.replay` 由调用方置上），之后从补读到的
/// 位置接着读。打不开时返回 None。
fn open_lines(kq: RawFd, path: &Path, scan: LineScan, state: &mut WaitScan) -> Option<LineFile> {
    let fd = watch_path(kq, path, libc::O_RDONLY)?;
    // SAFETY: fd 有效；fstat 写入栈上的结构体。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        close_fd(fd);
        return None;
    }
    let size = st.st_size as u64;
    let start = size.saturating_sub(REPLAY_MAX);
    let mut buf = vec![0u8; (size - start) as usize];
    let mut got = 0;
    while got < buf.len() {
        // SAFETY: 读入本函数持有的缓冲区，长度不超过剩余大小。
        let n = unsafe { libc::pread(fd, buf[got..].as_mut_ptr().cast(), buf.len() - got, (start + got as u64) as libc::off_t) };
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    buf.truncate(got);
    // 从中间读起时第一行不完整，跳过；最后一个换行之后的半行留给之后的读取。
    let begin = if start > 0 { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(begin, |i| i + 1).max(begin);
    for line in buf[begin..end].split(|&b| b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            scan(state, &v);
        }
    }
    // 整段里没有换行（一行超过上限）时从文件末尾读起。
    let offset = if end > begin || start == 0 { start + end as u64 } else { size };
    Some(LineFile { fd, sub: state.sub, offset, partial: Vec::new() })
}

/// 对话库：目录打不开时返回 None；库文件与 `-wal` 不在也照常监听（新对话的库可能稍后才建）。
fn open_db(kq: RawFd, path: &Path) -> Option<Watch> {
    let dir = watch_path(kq, path.parent()?, libc::O_EVTONLY)?;
    let db = watch_path(kq, path, libc::O_EVTONLY);
    let wal = watch_path(kq, &wal_of(path), libc::O_EVTONLY);
    Some(Watch::Db { path: path.to_owned(), dir, db, wal })
}

fn wal_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// 被删或改名的库文件、`-wal` 关掉；不在监听的那个如已存在就打开（目录有变化时多半是 `-wal` 建了或删了）。
fn sync_db(kq: RawFd, tid: &str, e: &mut Entry, fired: RawFd, gone: bool, owner: &mut HashMap<RawFd, String>) {
    let Watch::Db { path, db, wal, .. } = &mut e.watch else { return };
    let files = [path.clone(), wal_of(path)];
    for (slot, file) in [db, wal].into_iter().zip(files) {
        if gone && *slot == Some(fired) {
            owner.remove(&fired);
            close_fd(fired);
            *slot = None;
        }
        if slot.is_none() {
            if let Some(fd) = watch_path(kq, &file, libc::O_EVTONLY) {
                owner.insert(fd, tid.to_owned());
                *slot = Some(fd);
            }
        }
    }
}

fn judge_db(e: &mut Entry) -> Option<Kind> {
    let Watch::Db { path, .. } = &e.watch else { return None };
    let read = read_agy(path)?;
    antigravity(&mut e.state, &read)
}

/// 只读查询 Antigravity 对话库（表结构非官方承诺，2026-11-01 实测 1.4.12）：执行记录的行数与最后一行，以及本次执行里
/// 有没有在等用户的步骤。库不在或查询出错为 None（这次不判定，下次有写入再读）。
fn read_agy(path: &Path) -> Option<AgyRead> {
    let conn = open_sqlite_ro(path).ok()??;
    let (runs, last): (i64, Option<Vec<u8>>) = conn
        .query_row("SELECT count(*), (SELECT data FROM executor_metadata ORDER BY idx DESC LIMIT 1) FROM executor_metadata", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok()?;
    let (last_end, last_step) = last.as_deref().map(agy_run).unwrap_or((None, None));
    let floor = last_step.map_or(-1, |s| s as i64);
    let waiting: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM steps WHERE idx > ?1 AND status = ?2)", [floor, AGY_STEP_WAITING], |r| r.get(0))
        .ok()?;
    Some(AgyRead { runs: runs as u64, last_end, waiting })
}

/// 只读打开别的程序的 SQLite 库（WAL 模式）。写入方开着库时 `-shm` 在，按普通只读打开，读得到 `-wal` 里的新内容；
/// 写入方关库后 `-shm`、`-wal` 都删了，只读连接建不了 `-shm`、打不开，此时库文件就是全部内容，按不可变方式读
/// （Antigravity 1.4.12 实测，2026-11-01）。库不在为 Ok(None)。对话标题（`title.rs`）共用。
pub(crate) fn open_sqlite_ro(path: &Path) -> rusqlite::Result<Option<rusqlite::Connection>> {
    use rusqlite::{Connection, OpenFlags};
    if !path.exists() {
        return Ok(None);
    }
    // 打开是惰性的，打不开要到第一次查询才报错，所以先查一次。
    let ro = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = if ro.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_ok() {
        ro
    } else {
        let p = path.to_string_lossy().replace('%', "%25").replace('?', "%3f").replace('#', "%23");
        Connection::open_with_flags(format!("file:{p}?immutable=1"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?
    };
    conn.busy_timeout(std::time::Duration::from_millis(100))?;
    Ok(Some(conn))
}

/// 读新写入的完整行，返回第一个认出的统一事件；后面的行留到下次（偏移停在这一行之后）。
fn read_new(fd: RawFd, offset: &mut u64, partial: &mut Vec<u8>, scan: LineScan, state: &mut WaitScan) -> Option<Kind> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: 读入本函数持有的缓冲区，长度不超过其大小。
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), *offset as libc::off_t) };
        if n <= 0 {
            return None;
        }
        *offset += n as u64;
        partial.extend_from_slice(&buf[..n as usize]);
        while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = partial.drain(..=pos).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                if let Some(kind) = scan(state, &v) {
                    // 尚未判定的部分退回文件偏移，下次接着读。
                    *offset -= partial.len() as u64;
                    partial.clear();
                    return Some(kind);
                }
            }
        }
    }
}

/// 读新写入的完整行里用户提交的提示词（`Prompt::submitted`），返回各条的时间（记录里没有时间的为 None）；不完整的
/// 一行留到下次。
fn read_submits(f: &mut LineFile, source: ReplySource) -> Vec<Option<i64>> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: 读入本函数持有的缓冲区，长度不超过其大小。
        let n = unsafe { libc::pread(f.fd, buf.as_mut_ptr().cast(), buf.len(), f.offset as libc::off_t) };
        if n <= 0 {
            break;
        }
        f.offset += n as u64;
        f.partial.extend_from_slice(&buf[..n as usize]);
    }
    let Some(end) = f.partial.iter().rposition(|&b| b == b'\n') else { return Vec::new() };
    let done: Vec<u8> = f.partial.drain(..=end).collect();
    user_prompts(source, &String::from_utf8_lossy(&done)).into_iter().filter(|p| p.submitted).map(|p| p.at).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use scyllaui_proto::adapters::waiting::claude;
    use std::io::Write;
    use std::time::Duration;

    fn append(path: &std::path::Path, text: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    const ERR: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"x"}]}}"#;
    const MARK: &str = r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;

    fn rejected(rx: &mpsc::Receiver<Cmd>, within: Duration) -> Option<(String, Option<String>)> {
        match rx.recv_timeout(within) {
            Ok(Cmd::SessionRecord { terminal_id, session_id, kind: Kind::TurnInterrupt, .. }) => Some((terminal_id, session_id)),
            Ok(Cmd::WatchOpened { .. }) => rejected(rx, within),
            _ => None,
        }
    }

    /// 开始监听之前已有的拒绝记录不算；之后分两次写入（一行被拆开）的拒绝记录在 1 秒内认出；认出后停止监听。
    #[test]
    fn rejection_written_after_start_is_reported_once() {
        let d = test_dir("waitwatch");
        let path = d.join("t.jsonl");
        std::fs::write(&path, format!("{ERR}\n{MARK}\n")).unwrap();
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start_path("t1", Some("s1".into()), path.clone(), Scan::Lines(claude));
        std::thread::sleep(Duration::from_millis(100));
        append(&path, "{\"type\":\"assistant\",\"message\":{}}\n");
        assert!(rejected(&rx, Duration::from_millis(300)).is_none(), "旧的拒绝记录不算");
        append(&path, &format!("{ERR}\n{}", &MARK[..20]));
        append(&path, &format!("{}\n", &MARK[20..]));
        assert_eq!(rejected(&rx, Duration::from_secs(1)), Some(("t1".into(), Some("s1".into()))));
        append(&path, &format!("{ERR}\n{MARK}\n"));
        assert!(rejected(&rx, Duration::from_millis(300)).is_none(), "认出后已停止监听");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 用户提交的提示词：开始监听之前写下的不算；之后写入的每条报一次（带记录里的时间），`!命令` 与分两次写入时
    /// 还不完整的一行不报。
    #[test]
    fn submitted_prompts_written_after_start_are_reported() {
        let d = test_dir("waitwatch-submits");
        let path = d.join("events.jsonl");
        let line = |text: &str, at: &str| format!(r#"{{"type":"user.message","timestamp":"{at}","data":{{"content":"{text}"}}}}"#);
        std::fs::write(&path, format!("{}\n", line("旧的", "2026-11-21T01:00:00.000Z"))).unwrap();
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start_path("t1", Some("s1".into()), path.clone(), Scan::Submits(ReplySource::CopilotEvents));
        let submit = |within: Duration| loop {
            match rx.recv_timeout(within) {
                Ok(Cmd::RecordSubmit { terminal_id, session_id, at }) => return Some((terminal_id, session_id, at)),
                Ok(Cmd::WatchOpened { ok, .. }) => assert!(ok),
                _ => return None,
            }
        };
        std::thread::sleep(Duration::from_millis(100));
        append(&path, "{\"type\":\"assistant.message\",\"data\":{}}\n");
        assert_eq!(submit(Duration::from_millis(300)), None, "开始之前写下的提示词不算，回复也不算");
        let new = line("新的", "2026-11-21T01:00:10.000Z");
        append(&path, &new[..30]);
        assert_eq!(submit(Duration::from_millis(300)), None, "一行还没写完");
        append(&path, &format!("{}\n", &new[30..]));
        assert_eq!(submit(Duration::from_secs(1)), Some(("t1".into(), Some("s1".into()), Some(1_795_222_810_000))));
        append(&path, &format!("{}\n", line("再一条", "2026-11-21T01:00:20.000Z")));
        assert!(submit(Duration::from_secs(1)).is_some(), "认出后继续监听");
        w.stop("t1");
        std::thread::sleep(Duration::from_millis(100));
        append(&path, &format!("{}\n", line("停了以后", "2026-11-21T01:00:30.000Z")));
        assert_eq!(submit(Duration::from_millis(300)), None, "叫停后不再报");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn agy_db(path: &std::path::Path) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0);
             CREATE TABLE executor_metadata (idx integer PRIMARY KEY, data blob);
             INSERT INTO steps VALUES (0, 14, 3), (1, 15, 3);
             INSERT INTO executor_metadata VALUES (0, x'0804100218011a00');",
        )
        .unwrap();
        c
    }

    fn next_kind(rx: &mpsc::Receiver<Cmd>, within: Duration) -> Option<Kind> {
        match rx.recv_timeout(within) {
            Ok(Cmd::SessionRecord { kind, .. }) => Some(kind),
            Ok(Cmd::WatchOpened { .. }) => next_kind(rx, within),
            _ => None,
        }
    }

    /// Antigravity 对话库（WAL，agy 开着库）：开始时报告不在等；出现等待用户的步骤即需确认、消失即批准；多出一次
    /// 取消的执行即中断，之后停止监听。
    #[test]
    fn antigravity_db_changes_are_reported() {
        let d = test_dir("waitwatch-agy");
        let path = d.join("c1.db");
        let w = agy_db(&path);
        let (tx, rx) = mpsc::channel();
        let watch = WaitWatch::new(tx).unwrap();
        watch.start_path("t1", Some("c1".into()), path.clone(), Scan::AntigravityDb);
        let t = Duration::from_secs(1);
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionDone), "开始时读一次");
        w.execute_batch("INSERT INTO steps VALUES (2, 14, 3), (3, 132, 9);").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionWait));
        w.execute_batch("UPDATE steps SET status = 2 WHERE idx = 3;").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionDone));
        w.execute_batch("INSERT INTO steps VALUES (4, 132, 9);").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionWait));
        // 拒绝：步骤改为 7，写一行取消的执行记录（字段 1 = 2，字段 3 = 4）。
        w.execute_batch("UPDATE steps SET status = 7 WHERE idx = 4; INSERT INTO executor_metadata VALUES (1, x'08021804');").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::TurnInterrupt));
        w.execute_batch("INSERT INTO steps VALUES (5, 132, 9);").unwrap();
        assert_eq!(next_kind(&rx, Duration::from_millis(400)), None, "认出中断后已停止监听");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// agy 关库后（`-wal`、`-shm` 删了）按不可变方式读；之后 agy 重新开库写入，按目录变化盯上新的 `-wal`。
    #[test]
    fn antigravity_db_closed_then_reopened() {
        let d = test_dir("waitwatch-agy-closed");
        let path = d.join("c2.db");
        let c = agy_db(&path);
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(c);
        // agy 关库时 `-wal`、`-shm` 都删掉；系统的 SQLite 默认留着，这里照 agy 的样子删掉。
        for ext in ["-wal", "-shm"] {
            let mut s = path.as_os_str().to_owned();
            s.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(s));
        }
        let (tx, rx) = mpsc::channel();
        let watch = WaitWatch::new(tx).unwrap();
        watch.start_path("t1", None, path.clone(), Scan::AntigravityDb);
        let t = Duration::from_secs(1);
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionDone), "关着的库也读得出");
        let w = rusqlite::Connection::open(&path).unwrap();
        w.execute_batch("INSERT INTO steps VALUES (2, 132, 9);").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::PermissionWait));
        w.execute_batch("INSERT INTO executor_metadata VALUES (1, x'08041802');").unwrap();
        assert_eq!(next_kind(&rx, t), Some(Kind::TurnEnd), "正常结束的执行记录");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn opened(rx: &mpsc::Receiver<Cmd>, within: Duration) -> Option<bool> {
        match rx.recv_timeout(within) {
            Ok(Cmd::WatchOpened { ok, .. }) => Some(ok),
            _ => None,
        }
    }

    fn kimi_job(home: &std::path::Path, turn: Option<&str>) -> Start {
        Start {
            key: "t1".into(),
            terminal_id: "t1".into(),
            session_id: Some("session_k1".into()),
            source: Source::KimiIndex { home: home.to_owned(), session: "session_k1".into() },
            scan: Scan::Lines(scyllaui_proto::adapters::waiting::kimi),
            turn: turn.map(str::to_owned),
            waiting: false,
            resync: false,
            sub: false,
            agent: None,
        }
    }

    const K_END: &str = r#"{"type":"turn.ended","agentId":"main","turnId":TURN,"reason":"completed","durationMs":5}"#;
    const K_ASK: &str = r#"{"type":"turn.prompt","agentId":"main","origin":{"kind":"user"},"turnId":TURN,"trigger":"normal","time":AT}"#;

    fn k_ask(turn: u32, at: u64) -> String {
        K_ASK.replace("TURN", &turn.to_string()).replace("AT", &at.to_string())
    }

    /// Kimi：索引与记录在开始监听之后才出现（新会话第一轮）也能接上；认出的结束带着这次监听的轮次标识（收到提问的
    /// 时刻）报告，不自行停止，由核心线程核对后叫停。
    #[test]
    fn kimi_record_appearing_later_is_picked_up() {
        let d = test_dir("waitwatch-kimi-late");
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start(kimi_job(&d, Some("1794668003000")));
        std::thread::sleep(Duration::from_millis(300));
        let session = d.join(".kimi-code/sessions/wd_w_abc/session_k1");
        std::fs::create_dir_all(session.join("agents/main")).unwrap();
        let wire = session.join("agents/main/wire.jsonl");
        std::fs::write(&wire, format!("{}\n", k_ask(0, 1_794_668_003_010))).unwrap();
        std::fs::write(
            d.join(".kimi-code/session_index.jsonl"),
            format!("{}\n", serde_json::json!({"sessionId": "session_k1", "sessionDir": session})),
        )
        .unwrap();
        assert_eq!(opened(&rx, Duration::from_secs(2)), Some(true));
        append(&wire, &format!("{}\n", K_END.replace("TURN", "0")));
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::SessionRecord { kind: Kind::TurnEnd, turn, .. }) => assert_eq!(turn.as_deref(), Some("1794668003000")),
            _ => panic!("应报告轮次结束"),
        }
        append(&wire, &format!("{}\n{}\n", k_ask(1, 1_794_668_009_000), K_END.replace("TURN", "1")));
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::SessionRecord { kind: Kind::PromptSubmit, .. }) => {}
            _ => panic!("新开的一轮按提交报告"),
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::SessionRecord { kind: Kind::TurnEnd, turn, .. }) => {
                assert_eq!(turn.as_deref(), Some("1794668003000"), "带轮次标识的不自行停止")
            }
            _ => panic!("应继续监听"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Grok：子代理的审批只写在子会话的记录里时也认得出——主会话记录与还在跑的子代理的会话记录一起监听，按总数配对。
    #[test]
    fn grok_child_session_records_are_watched_too() {
        let d = test_dir("waitwatch-grok-child");
        let root = d.join(".grok/sessions/%2Fw");
        let (main, child, done) = (root.join("m1"), root.join("c1"), root.join("c0"));
        for (dir, id, status) in [(&child, "c1", "running"), (&done, "c0", "completed")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::create_dir_all(main.join("subagents").join(id)).unwrap();
            std::fs::write(
                main.join("subagents").join(id).join("meta.json"),
                serde_json::json!({"parent_session_id": "m1", "child_session_id": id, "status": status}).to_string(),
            )
            .unwrap();
        }
        const REQ: &str = r#"{"ts":"2026-11-14T02:47:58.644Z","type":"permission_requested","tool_name":"run_terminal_command"}"#;
        const RES: &str = r#"{"ts":"2026-11-14T02:48:15.883Z","type":"permission_resolved","tool_name":"run_terminal_command","decision":"allow","wait_ms":17239}"#;
        std::fs::write(main.join("events.jsonl"), "{\"type\":\"turn_ended\"}\n").unwrap();
        std::fs::write(child.join("events.jsonl"), format!("{REQ}\n")).unwrap();
        // 已结束的子代理留下的没配对的记录不算。
        std::fs::write(done.join("events.jsonl"), format!("{REQ}\n")).unwrap();
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start(Start {
            key: "t1".into(),
            terminal_id: "t1".into(),
            session_id: Some("m1".into()),
            source: Source::GrokSessions { home: d.clone(), session: "m1".into() },
            scan: Scan::Lines(scyllaui_proto::adapters::waiting::grok),
            turn: None,
            waiting: true,
            resync: false,
            sub: false,
            agent: None,
        });
        assert_eq!(opened(&rx, Duration::from_secs(1)), Some(true));
        append(&child.join("events.jsonl"), &format!("{RES}\n"));
        assert_eq!(next_kind(&rx, Duration::from_secs(1)), Some(Kind::PermissionDone), "子会话记录里的批准");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Claude 的进程状态文件（原地改写）：开始时读到等审批，之后写回 busy 即批准；同一终端的另一个监听互不影响。
    #[test]
    fn claude_status_file_is_watched_whole() {
        let d = test_dir("waitwatch-claude-status");
        let path = d.join("4242.json");
        let write = |status: &str, wf: &str| {
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new().write(true).create(true).open(&path).unwrap();
            let text = format!("{{\"pid\":4242,\"sessionId\":\"s1\",\"status\":\"{status}\"{wf}}}");
            f.set_len(0).unwrap();
            f.rewind().unwrap();
            f.write_all(text.as_bytes()).unwrap();
        };
        write("waiting", ",\"waitingFor\":\"permission prompt\"");
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start(Start {
            key: "t1#status".into(),
            terminal_id: "t1".into(),
            session_id: Some("s1".into()),
            source: Source::Path(path.clone()),
            scan: Scan::Whole(scyllaui_proto::adapters::waiting::claude_status),
            turn: None,
            waiting: true,
            resync: false,
            sub: false,
            agent: None,
        });
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::WatchOpened { key, ok: true, .. }) => assert_eq!(key, "t1#status"),
            _ => panic!("应回报已打开"),
        }
        write("busy", "");
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::SessionRecord { terminal_id, kind: Kind::PermissionDone, .. }) => assert_eq!(terminal_id, "t1"),
            _ => panic!("应报告批准"),
        }
        write("waiting", ",\"waitingFor\":\"permission prompt\"");
        std::thread::sleep(Duration::from_millis(100));
        write("idle", "");
        assert_eq!(next_kind(&rx, Duration::from_secs(1)), Some(Kind::TurnInterrupt), "拒绝");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Kimi：开始监听之前已经写入的本轮结束靠补读认出；上一轮的不算（这一轮的提问还没写进记录时，最后一轮是上一轮）。
    #[test]
    fn kimi_end_written_before_start_is_replayed() {
        let d = test_dir("waitwatch-kimi-replay");
        let session = d.join(".kimi-code/sessions/wd_w_abc/session_k1");
        std::fs::create_dir_all(session.join("agents/main")).unwrap();
        let wire = session.join("agents/main/wire.jsonl");
        std::fs::write(
            &wire,
            format!(
                "{}\n{}\n{}\n{}\n",
                k_ask(1, 1_794_668_000_000),
                K_END.replace("TURN", "1"),
                k_ask(2, 1_794_668_003_016),
                K_END.replace("TURN", "2")
            ),
        )
        .unwrap();
        std::fs::write(
            d.join(".kimi-code/session_index.jsonl"),
            format!("{}\n", serde_json::json!({"sessionId": "session_k1", "sessionDir": session})),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start(kimi_job(&d, Some("1794668010000")));
        assert_eq!(opened(&rx, Duration::from_secs(1)), Some(true));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "上一轮的结束不算");
        w.start(kimi_job(&d, Some("1794668003000")));
        assert_eq!(opened(&rx, Duration::from_secs(1)), Some(true));
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::SessionRecord { kind: Kind::TurnEnd, turn, .. }) => assert_eq!(turn.as_deref(), Some("1794668003000")),
            _ => panic!("应补读出本轮的结束"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 停止后不再报告；文件不存在时什么也不做。
    #[test]
    fn stopped_or_missing_file_reports_nothing() {
        let d = test_dir("waitwatch-stop");
        let path = d.join("t.jsonl");
        std::fs::write(&path, "").unwrap();
        let (tx, rx) = mpsc::channel();
        let w = WaitWatch::new(tx).unwrap();
        w.start_path("t1", None, path.clone(), Scan::Lines(claude));
        w.start_path("t2", None, d.join("missing.jsonl"), Scan::Lines(claude));
        w.stop("t1");
        std::thread::sleep(Duration::from_millis(100));
        append(&path, &format!("{ERR}\n{MARK}\n"));
        assert!(rejected(&rx, Duration::from_millis(400)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
