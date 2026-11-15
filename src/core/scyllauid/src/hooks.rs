//! Hook 配置监听（M1 第 7 块，docs/design.md 4.7 节「配置监听用 kqueue」）：kqueue 监听四款 CLI 的 Hook 配置文件
//! 与所在目录，改动后防抖 200 ms 重新分析（规则在 `scyllaui_proto::hookcfg`），状态有变化才交给核心线程推送。
//! 与 Git 分支监听同一机制，不加依赖；空闲时阻塞在 kevent 上，不唤醒。每轮分析后重新挂监听：
//! 文件被原子替换（写临时文件再改名）后旧描述符指向的已是旧文件。

use crate::core::Cmd;
use scyllaui_proto::hookcfg::{self, Status, TOOLS};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// 一串改动结束后再分析：CLI 保存配置时常常连续写几次。
const DEBOUNCE: Duration = Duration::from_millis(200);
/// EVFILT_USER 的标识：核心线程请求立即重查（修复后、前台第一次识别到之前没装的 CLI）。
const NUDGE: usize = 1;

/// 四款的当前状态（顺序同 `TOOLS`）。
pub fn analyze_all(home: &str) -> Vec<Status> {
    TOOLS.iter().map(|t| hookcfg::analyze(&hookcfg::read(*t, home))).collect()
}

pub struct HookWatch {
    kq: Option<RawFd>,
}

impl HookWatch {
    /// 启动监听线程：先分析一次并交给核心，之后每次改动都重查。
    pub fn new(core: Sender<Cmd>, home: String) -> std::io::Result<HookWatch> {
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
        std::thread::spawn(move || {
            let mut last: Option<Vec<Status>> = None;
            loop {
                let fds: Vec<RawFd> = watch_paths(&home).iter().filter_map(|p| watch(kq, p)).collect();
                // 出错时沿用上次的结果，等下一次配置变化再分析。
                let now = crate::guarded("分析 Hook 配置", || analyze_all(&home)).or_else(|| last.clone()).unwrap_or_default();
                if last.as_ref() != Some(&now) {
                    if core.send(Cmd::HooksChanged { statuses: now.clone() }).is_err() {
                        return;
                    }
                    last = Some(now);
                }
                // 等第一个事件（不设超时），然后等到连续 200 ms 没有新事件。
                if !wait(kq, None) {
                    return;
                }
                while wait(kq, Some(DEBOUNCE)) {}
                for fd in fds {
                    // SAFETY: 关闭本轮打开的监听描述符；kqueue 随之移除对应事件。
                    unsafe { libc::close(fd) };
                }
            }
        });
        Ok(HookWatch { kq: Some(kq) })
    }

    /// 测试用：不监听，`recheck` 什么也不做。
    #[cfg(test)]
    pub fn inert() -> HookWatch {
        HookWatch { kq: None }
    }

    /// 请求立即重查。
    pub fn recheck(&self) {
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

/// 要监听的文件与目录：每款的主配置、停用与信任记录，以及它们所在的目录（原子替换、新建、删除都会让目录变化）。
/// 某款没装（配置目录不存在）时监听家目录，装上后即可发现。
fn watch_paths(home: &str) -> Vec<PathBuf> {
    let home = Path::new(home);
    let mut out: Vec<PathBuf> = vec![];
    let mut add = |p: &Path| {
        if p.exists() && !out.iter().any(|x| x == p) {
            out.push(p.to_path_buf());
        }
    };
    for tool in TOOLS {
        let lay = hookcfg::layout(tool, home);
        add(&lay.dir);
        if !lay.dir.exists() {
            add(home);
        }
        for f in std::iter::once(&lay.main).chain(lay.aux.as_ref()) {
            add(f);
            if let Some(parent) = f.parent() {
                add(parent);
            }
        }
        // 组织策略文件存在时也监听（原地改写不会让所在目录变化）；新出现的由所在目录或下一次重查发现。
        for f in &lay.policy {
            add(f);
        }
    }
    out
}

/// 打开并注册一个路径；打不开（不存在、无权限）时跳过，只是这一处没有实时更新。
fn watch(kq: RawFd, path: &Path) -> Option<RawFd> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: O_EVTONLY 只为接收事件打开；路径为以 NUL 结尾的 C 字符串。
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let ev = libc::kevent {
        ident: fd as libc::uintptr_t,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_ATTRIB,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: 只注册一个事件，不取回事件。
    if unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
        // SAFETY: 关闭刚打开、尚未交出的描述符。
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

/// 等一个事件；`timeout` 为 None 时一直等。收到事件为 true，超时为 false；kqueue 出错时线程结束（返回 false 且
/// 不再等待由调用方处理：第一次等待出错即退出线程）。
fn wait(kq: RawFd, timeout: Option<Duration>) -> bool {
    let ts = timeout.map(|d| libc::timespec { tv_sec: d.as_secs() as libc::time_t, tv_nsec: d.subsec_nanos() as libc::c_long });
    loop {
        // SAFETY: 读取一个事件到栈上的结构体。
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, ts.as_ref().map_or(std::ptr::null(), |t| t as *const _))
        };
        if n >= 0 {
            return n == 1;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use scyllaui_proto::hookcfg::State;
    use scyllaui_proto::Tool;
    use std::sync::mpsc;
    use std::time::Instant;

    fn next(rx: &mpsc::Receiver<Cmd>, within: Duration) -> Option<Vec<Status>> {
        match rx.recv_timeout(within) {
            Ok(Cmd::HooksChanged { statuses }) => Some(statuses),
            _ => None,
        }
    }

    fn state(s: &[Status], tool: Tool) -> State {
        s.iter().find(|x| x.tool == tool).unwrap().state
    }

    /// 改动、删除、原子替换、新装一款 CLI 都能在 1 秒内收到；状态没变化时不重复通知。
    #[test]
    fn config_changes_are_noticed() {
        let dir = test_dir("hooks-watch");
        let home = dir.display().to_string();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let settings = dir.join(".claude/settings.json");
        let snap = hookcfg::read(Tool::Claude, &home);
        let plan = hookcfg::plan(&snap, hookcfg::Action::Repair).unwrap().unwrap();
        hookcfg::apply(&[plan], "20261015-000000").unwrap();
        let (tx, rx) = mpsc::channel();
        let w = HookWatch::new(tx, home.clone()).unwrap();
        let s = next(&rx, Duration::from_secs(2)).expect("启动后先通知一次");
        assert_eq!(state(&s, Tool::Claude), State::Ok);
        assert_eq!(state(&s, Tool::Kimi), State::Absent);
        // 就地改写：删掉全部条目。
        let t = Instant::now();
        std::fs::write(&settings, "{}\n").unwrap();
        let s = next(&rx, Duration::from_secs(2)).expect("就地改写");
        assert_eq!(state(&s, Tool::Claude), State::Missing);
        assert!(t.elapsed() < Duration::from_secs(1), "1 秒内收到：{:?}", t.elapsed());
        // 原子替换（写临时文件再改名）回正常。
        let snap = hookcfg::read(Tool::Claude, &home);
        hookcfg::apply(&[hookcfg::plan(&snap, hookcfg::Action::Repair).unwrap().unwrap()], "20261015-000001").unwrap();
        assert_eq!(state(&next(&rx, Duration::from_secs(2)).expect("原子替换"), Tool::Claude), State::Ok);
        // 删除文件。
        std::fs::remove_file(&settings).unwrap();
        assert_eq!(state(&next(&rx, Duration::from_secs(2)).expect("删除"), Tool::Claude), State::Missing);
        // 新装一款（配置目录出现）：监听家目录才能发现。
        std::fs::create_dir_all(dir.join(".kimi-code")).unwrap();
        assert_eq!(state(&next(&rx, Duration::from_secs(2)).expect("新装"), Tool::Kimi), State::Missing);
        // 与状态无关的改动不通知；recheck 在状态不变时也不通知。
        std::fs::write(dir.join(".claude/history.jsonl"), "x\n").unwrap();
        w.recheck();
        assert!(next(&rx, Duration::from_millis(800)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
