//! 按进程认出「审批已批准」（2026-11-14，docs/design.md 4.3 节，第二轮长跑缺陷 E）：Codex 批准审批后、命令跑完前不发
//! Hook，会话记录里也没有及时的记录；唯一及时的信号是它的子进程里出现 `/bin/zsh -c <命令原文>`，参数末项与
//! PermissionRequest 载荷里的命令逐字相等（实测 0.167.2、0.169.0，批准后 50—270 ms）。
//! 待审批期间用 kqueue 盯这个 AI 进程和它已有的后代进程的 fork（`NOTE_FORK`；命令也可能由它的某个常驻子进程去起，
//! 如 Codex 的 app-server），每次 fork 之后在几个时间点查它的后代进程（刚 fork 出来的进程还没换成要执行的命令）；
//! 开始时已经在的进程不算。没有 fork 就不醒来，不轮询。

use crate::core::Cmd;
use crate::procinfo;
use scyllaui_proto::Kind;
use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const NUDGE: usize = 1;
/// 一次 fork 之后查后代进程的时间点（相对 fork）。
const CHECKS_MS: [u64; 5] = [15, 50, 120, 300, 700];

pub enum Job {
    Start { terminal_id: String, session_id: Option<String>, pid: i32, command: String },
    Stop { terminal_id: String },
}

pub struct ApprovalWatch {
    tx: Sender<Job>,
    kq: RawFd,
}

struct Entry {
    session_id: Option<String>,
    pid: i32,
    command: String,
    /// 开始时已经在的后代进程：同一条命令先前那次执行还没结束时不能算。
    baseline: HashSet<i32>,
    /// 还要查的时间点。
    due: Vec<Instant>,
}

impl ApprovalWatch {
    pub fn new(core: Sender<Cmd>) -> std::io::Result<ApprovalWatch> {
        // SAFETY: kqueue 无参数；描述符由本结构与监听线程共用，进程生命周期内不关闭。
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let nudge = libc::kevent { ident: NUDGE, filter: libc::EVFILT_USER, flags: libc::EV_ADD | libc::EV_CLEAR, fflags: 0, data: 0, udata: std::ptr::null_mut() };
        // SAFETY: 只注册一个事件。
        if unsafe { libc::kevent(kq, &nudge, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || run(kq, rx, core));
        Ok(ApprovalWatch { tx, kq })
    }

    pub fn start(&self, terminal_id: &str, session_id: Option<String>, pid: i32, command: String) {
        self.send(Job::Start { terminal_id: terminal_id.to_owned(), session_id, pid, command });
    }

    pub fn stop(&self, terminal_id: &str) {
        self.send(Job::Stop { terminal_id: terminal_id.to_owned() });
    }

    fn send(&self, job: Job) {
        let _ = self.tx.send(job);
        let kq = self.kq;
        let ev = libc::kevent { ident: NUDGE, filter: libc::EVFILT_USER, flags: 0, fflags: libc::NOTE_TRIGGER, data: 0, udata: std::ptr::null_mut() };
        // SAFETY: 触发已注册的用户事件。
        unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }
}

fn watch_pid(kq: RawFd, pid: i32, on: bool) {
    let ev = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: if on { libc::EV_ADD | libc::EV_CLEAR } else { libc::EV_DELETE },
        fflags: libc::NOTE_FORK | libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: 注册或删除一个进程事件；进程已不在时失败，忽略。
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
}

fn unwatch(kq: RawFd, e: &Entry) {
    watch_pid(kq, e.pid, false);
    for p in &e.baseline {
        watch_pid(kq, *p, false);
    }
}

/// 后代进程里有没有参数末项等于这条命令、且不在开始时就有的进程。
fn approved(e: &Entry) -> bool {
    procinfo::descendants_args(e.pid)
        .iter()
        .any(|(pid, args)| !e.baseline.contains(pid) && args.len() >= 2 && args.last() == Some(&e.command))
}

fn run(kq: RawFd, rx: Receiver<Job>, core: Sender<Cmd>) {
    let mut entries: HashMap<String, Entry> = HashMap::new();
    loop {
        // 有要查的时间点时等到最近的那个，否则一直等。
        let next = entries.values().flat_map(|e| e.due.iter()).min().copied();
        let wait = next.map(|t| {
            let d = t.saturating_duration_since(Instant::now());
            libc::timespec { tv_sec: d.as_secs() as libc::time_t, tv_nsec: d.subsec_nanos() as libc::c_long }
        });
        let timeout = wait.as_ref().map_or(std::ptr::null(), |w| w as *const libc::timespec);
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
                    Job::Start { terminal_id, session_id, pid, command } => {
                        if let Some(old) = entries.remove(&terminal_id) {
                            unwatch(kq, &old);
                        }
                        let baseline: HashSet<i32> = procinfo::descendants_args(pid).into_iter().map(|(p, _)| p).collect();
                        watch_pid(kq, pid, true);
                        for p in &baseline {
                            watch_pid(kq, *p, true);
                        }
                        entries.insert(terminal_id, Entry { session_id, pid, command, baseline, due: Vec::new() });
                    }
                    Job::Stop { terminal_id } => {
                        if let Some(old) = entries.remove(&terminal_id) {
                            unwatch(kq, &old);
                        }
                    }
                }
            }
        } else if n == 1 && ev.filter == libc::EVFILT_PROC {
            let pid = ev.ident as i32;
            if ev.fflags & libc::NOTE_EXIT != 0 {
                // AI 进程自己退出才结束；它的某个后代退出不管（内核随之移除那个进程的事件）。
                let gone: Vec<String> = entries.iter().filter(|(_, e)| e.pid == pid).map(|(t, _)| t.clone()).collect();
                for t in gone {
                    if let Some(old) = entries.remove(&t) {
                        unwatch(kq, &old);
                    }
                }
            }
            if ev.fflags & libc::NOTE_FORK != 0 {
                let now = Instant::now();
                for e in entries.values_mut().filter(|e| e.pid == pid || e.baseline.contains(&pid)) {
                    e.due = CHECKS_MS.iter().map(|ms| now + Duration::from_millis(*ms)).collect();
                }
            }
        }
        // 到点的查一次。
        let now = Instant::now();
        let mut done = Vec::new();
        for (tid, e) in entries.iter_mut() {
            let before = e.due.len();
            e.due.retain(|t| *t > now);
            if e.due.len() < before && approved(e) {
                done.push(tid.clone());
            }
        }
        for tid in done {
            let Some(e) = entries.remove(&tid) else { continue };
            unwatch(kq, &e);
            if core.send(Cmd::SessionRecord { terminal_id: tid, session_id: e.session_id, kind: Kind::PermissionDone, turn: None, agent: None }).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// 真实进程：被盯的进程 fork 出参数末项等于命令原文的子进程即报告批准；开始之前就在跑的同一条命令不算，
    /// 别的子进程也不算。
    #[test]
    fn child_running_the_exact_command_is_reported() {
        let cmd = "sleep 2; echo ZQAPPROVAL";
        // 先起一个已经在跑同一条命令的子进程（基线），1 秒后再起别的命令，2 秒后才起要认的那条。
        let script = format!("/bin/zsh -c '{cmd}' & sleep 1; /bin/zsh -c 'sleep 0.2; echo other'; sleep 1; /bin/zsh -c '{cmd}'; sleep 1");
        let mut parent = Command::new("/bin/sh").args(["-c", &script]).stdout(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let (tx, rx) = mpsc::channel();
        let w = ApprovalWatch::new(tx).unwrap();
        let t0 = Instant::now();
        w.start("t1", Some("s1".into()), parent.id() as i32, cmd.to_owned());
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Cmd::SessionRecord { terminal_id, session_id, kind: Kind::PermissionDone, .. }) => {
                assert_eq!((terminal_id.as_str(), session_id.as_deref()), ("t1", Some("s1")));
            }
            _ => panic!("应报告批准"),
        }
        let took = t0.elapsed();
        assert!(took > Duration::from_millis(1400), "基线里的同名进程与别的命令不算：{took:?}");
        assert!(took < Duration::from_millis(2600), "命令进程出现后很快认出：{took:?}");
        let _ = parent.kill();
        let _ = parent.wait();
    }

    /// 命令由被盯进程已有的子进程去起（中间隔一层常驻进程）时同样认得出。
    #[test]
    fn command_started_by_an_existing_child_is_reported() {
        let cmd = "sleep 2; echo ZQGRANDCHILD";
        // 外层只等着；内层（开始时已经在）1 秒后才起命令。
        let inner = format!("sleep 1; /bin/zsh -c '\\''{cmd}'\\''; sleep 1");
        let script = format!("/bin/sh -c '{inner}' & wait");
        let mut parent = Command::new("/bin/sh").args(["-c", &script]).stdout(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let (tx, rx) = mpsc::channel();
        let w = ApprovalWatch::new(tx).unwrap();
        w.start("t1", None, parent.id() as i32, cmd.to_owned());
        assert!(matches!(rx.recv_timeout(Duration::from_secs(4)), Ok(Cmd::SessionRecord { kind: Kind::PermissionDone, .. })), "应报告批准");
        let _ = parent.kill();
        let _ = parent.wait();
    }

    /// 停止后不再报告。
    #[test]
    fn stopped_watch_reports_nothing() {
        let cmd = "sleep 1; echo ZQSTOPPED";
        let script = format!("sleep 0.6; /bin/zsh -c '{cmd}'; sleep 1");
        let mut parent = Command::new("/bin/sh").args(["-c", &script]).stdout(Stdio::null()).spawn().unwrap();
        let (tx, rx) = mpsc::channel();
        let w = ApprovalWatch::new(tx).unwrap();
        w.start("t1", None, parent.id() as i32, cmd.to_owned());
        w.stop("t1");
        assert!(rx.recv_timeout(Duration::from_millis(1500)).is_err());
        let _ = parent.kill();
        let _ = parent.wait();
    }
}
