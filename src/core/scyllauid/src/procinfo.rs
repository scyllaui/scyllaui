//! 进程层（docs/design.md 4.1、4.2 节）：识别终端前台进程组属于哪款 CLI，并用 kqueue 监听 Agent 进程退出。
//! 本文件只放 macOS 系统调用（3.2 节平台层）；识别规则在 `scyllaui_proto::adapters`。

use crate::core::Cmd;
use scyllaui_proto::adapters::{adapter, identify_proc, Process, VersionSource};
use scyllaui_proto::Tool;
use serde_json::{json, Value};
use std::os::fd::RawFd;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

/// `sys/proc_info.h`：按进程组列出进程（libc 未导出该常量）。
const PROC_PGRP_ONLY: u32 = 2;

/// 前台程序层（4.1 节），与 Hook 给出的 Agent 状态分开。
#[derive(Clone, Debug, PartialEq)]
pub enum Foreground {
    /// 还没读到，或 Agent 刚退出、等 App 重新上报。
    Unknown,
    /// Shell 空闲：首进程的路径在 `/etc/shells` 中。
    Shell {
        pid: i32,
        path: String,
    },
    Command {
        pid: i32,
        path: String,
    },
    Agent {
        pid: i32,
        path: String,
        tool: Tool,
    },
}

impl Foreground {
    pub fn to_json(&self) -> Value {
        let (kind, pid, path, tool) = match self {
            Foreground::Unknown => ("unknown", None, None, None),
            Foreground::Shell { pid, path } => ("shell", Some(*pid), Some(path), None),
            Foreground::Command { pid, path } => ("command", Some(*pid), Some(path), None),
            Foreground::Agent { pid, path, tool } => {
                ("agent", Some(*pid), Some(path), Some(tool.as_str()))
            }
        };
        let mut v = json!({"kind": kind, "tool": tool, "pid": pid, "path": path});
        // Agent 进程的启动时间（Unix 毫秒）：还没提问、会话 ID 未知时侧栏显示「新会话 · 启动时间」（M1 第 6 块）。
        if let Foreground::Agent { pid, .. } = self {
            v["started_at"] = json!(start_ms(*pid));
        }
        v
    }
}

/// 进程是否还在（含被 Ctrl+Z 挂起的）。
pub fn process_exists(pid: i32) -> bool {
    // SAFETY: 信号 0 只检查进程是否存在与权限，不发送信号。
    let ok = unsafe { libc::kill(pid, 0) } == 0;
    pid > 0 && (ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// 进程的启动时间（Unix 毫秒）；读不到为 None。
pub fn start_ms(pid: i32) -> Option<i64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: 缓冲区为 proc_bsdinfo，大小与传入的一致。
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    (n == size).then(|| info.pbi_start_tvsec as i64 * 1000 + info.pbi_start_tvusec as i64 / 1000)
}

/// 进程的控制终端设备号（`e_tdev`）；没有控制终端或读不到为 None（M2 第 2 块：终端进程树统计）。
pub fn tty_of(pid: i32) -> Option<u32> {
    let info = bsdinfo(pid)?;
    (info.e_tdev != u32::MAX && info.e_tdev != 0).then_some(info.e_tdev)
}

fn bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: 缓冲区为 proc_bsdinfo，大小与传入的一致。
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    (n == size).then_some(info)
}

/// 一次进程树采样（docs/protocol.md 第 7 节「进程树统计」）：每个进程（进程号 + 启动时间）的累计 CPU 纳秒，与内存之和。
pub struct TreeSample {
    pub cpu_ns: std::collections::HashMap<(i32, u64), u64>,
    pub mem_bytes: u64,
}

/// 「Shell 及其所有子进程」：控制终端为 `tdev` 且属于当前用户的进程，加上它们的全部子进程（docs/protocol.md 第 7 节）。
/// 系统以 root 身份运行的 `/usr/bin/login` 不计。读不到某个进程时跳过它。
pub fn tree_sample(tdev: u32) -> TreeSample {
    // SAFETY: getuid 无参数，总是成功。
    let uid = unsafe { libc::getuid() };
    use std::collections::{HashMap, HashSet};
    let mut pids = vec![0i32; 4096];
    // SAFETY: 缓冲区大小按字节传入；返回实际写入的进程数。
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as libc::c_int) };
    pids.truncate(n.max(0) as usize);
    let mut children: HashMap<u32, Vec<i32>> = HashMap::new();
    let mut infos = HashMap::new();
    let mut queue: Vec<i32> = Vec::new();
    for &pid in &pids {
        let Some(info) = bsdinfo(pid) else { continue };
        children.entry(info.pbi_ppid).or_default().push(pid);
        if info.e_tdev == tdev && info.pbi_uid == uid {
            queue.push(pid);
        }
        infos.insert(pid, info);
    }
    let mut seen = HashSet::new();
    let mut sample = TreeSample { cpu_ns: HashMap::new(), mem_bytes: 0 };
    let (numer, denom) = timebase();
    while let Some(pid) = queue.pop() {
        if !seen.insert(pid) {
            continue;
        }
        queue.extend(children.get(&(pid as u32)).into_iter().flatten().copied());
        let mut ru: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: 缓冲区为 rusage_info_v2，与请求的版本一致。
        let ok = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&mut ru as *mut libc::rusage_info_v2).cast()) } == 0;
        if !ok {
            continue;
        }
        let start = infos.get(&pid).map_or(0, |i| i.pbi_start_tvsec * 1_000_000 + i.pbi_start_tvusec);
        let ticks = ru.ri_user_time + ru.ri_system_time;
        sample.cpu_ns.insert((pid, start), (ticks as u128 * numer as u128 / denom as u128) as u64);
        sample.mem_bytes += ru.ri_phys_footprint;
    }
    sample
}

/// `mach/mach_time.h`；libc crate 中的同名定义已标为弃用（建议引入 mach2），只为这一处不加依赖，自己声明。
#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
}

/// `rusage` 的 CPU 时间单位（mach 绝对时间）换算为纳秒的比例；Apple Silicon 上不是 1:1。
fn timebase() -> (u32, u32) {
    static TB: OnceLock<(u32, u32)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = MachTimebase { numer: 0, denom: 0 };
        // SAFETY: 写入栈上的结构体。
        unsafe { mach_timebase_info(&mut info) };
        if info.denom == 0 { (1, 1) } else { (info.numer, info.denom) }
    })
}

/// 两次采样之间的 CPU 百分比（单核计）：只算两次都在的同一进程。
pub fn cpu_percent(prev: &TreeSample, cur: &TreeSample, elapsed: std::time::Duration) -> f64 {
    let used: u64 = cur
        .cpu_ns
        .iter()
        .filter_map(|(k, now)| prev.cpu_ns.get(k).map(|before| now.saturating_sub(*before)))
        .sum();
    let wall = elapsed.as_nanos().max(1) as f64;
    used as f64 / wall * 100.0
}

/// 识别前台进程组：组内任一进程被某个适配器识别即为 Agent；否则看首进程是不是 Shell。
#[cfg(test)]
pub fn classify(pgid: i32) -> Foreground {
    classify_full(pgid).0
}

/// 同 `classify`，识别为 Agent 时另给出版本来源与被识别出的那个进程的可执行文件（M1 第 7 块）。
pub fn classify_full(pgid: i32) -> (Foreground, Option<(VersionSource, String)>) {
    if pgid <= 0 {
        return (Foreground::Unknown, None);
    }
    let mut members = group_members(pgid);
    // 首进程排在最前，便于报告与判断 Shell。
    members.sort_by_key(|p| *p != pgid);
    let procs: Vec<(String, Vec<String>)> = members
        .iter()
        .filter_map(|&pid| Some((exe_path(pid)?, resolved_args(pid))))
        .collect();
    let views: Vec<Process> = procs
        .iter()
        .map(|(path, args)| Process { path, args })
        .collect();
    let Some(leader) = exe_path(pgid) else {
        return (Foreground::Unknown, None);
    };
    if let Some((tool, p)) = identify_proc(&views) {
        let version = adapter(tool).version_source(p).map(|s| (s, p.path.to_owned()));
        return (
            Foreground::Agent {
                pid: pgid,
                path: leader,
                tool,
            },
            version,
        );
    }
    let fg = if shells().iter().any(|s| *s == leader) {
        Foreground::Shell {
            pid: pgid,
            path: leader,
        }
    } else {
        Foreground::Command {
            pid: pgid,
            path: leader,
        }
    };
    (fg, None)
}

fn shells() -> &'static [String] {
    static SHELLS: OnceLock<Vec<String>> = OnceLock::new();
    SHELLS.get_or_init(|| {
        std::fs::read_to_string("/etc/shells")
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('/'))
            .map(str::to_owned)
            .collect()
    })
}

fn group_members(pgid: i32) -> Vec<i32> {
    let mut pids = vec![0i32; 64];
    loop {
        let bytes = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: 缓冲区大小与传入的字节数一致。
        let n = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, pids.as_mut_ptr().cast(), bytes)
        };
        if n <= 0 {
            return vec![pgid];
        }
        let count = n as usize / std::mem::size_of::<i32>();
        if count < pids.len() {
            pids.truncate(count);
            pids.retain(|&p| p > 0);
            return pids;
        }
        pids.resize(pids.len() * 2, 0);
    }
}

/// `root` 的全部后代进程及其参数（认 Codex 批准后执行命令的子进程用，approval.rs）。
pub fn descendants_args(root: i32) -> Vec<(i32, Vec<String>)> {
    use std::collections::HashMap;
    let mut pids = vec![0i32; 4096];
    // SAFETY: 缓冲区大小按字节传入；返回实际写入的进程数。
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as libc::c_int) };
    pids.truncate(n.max(0) as usize);
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for &pid in &pids {
        if let Some(info) = bsdinfo(pid) {
            children.entry(info.pbi_ppid as i32).or_default().push(pid);
        }
    }
    let (mut out, mut queue) = (Vec::new(), vec![root]);
    while let Some(p) = queue.pop() {
        for &c in children.get(&p).map(Vec::as_slice).unwrap_or(&[]) {
            if c != root && !out.iter().any(|(x, _)| *x == c) {
                out.push((c, proc_args(c)));
                queue.push(c);
            }
        }
    }
    out
}

/// 进程组内每个进程的参数（守护进程托管会话的归属反查用，route.rs）。
pub fn group_args(pgid: i32) -> Vec<(i32, Vec<String>)> {
    if pgid <= 0 {
        return Vec::new();
    }
    group_members(pgid).into_iter().map(|pid| (pid, proc_args(pid))).collect()
}

/// 识别用的可执行文件路径：`proc_pidpath`，读不到时退回进程启动时记录的路径（解析符号链接）。CLI 在运行中自动升级、
/// 可执行文件被替换后，`proc_pidpath` 对该进程失败（2026-10-15 实测 Claude Code 2.1.296→2.1.300，docs/design.md 3.1 节）。
fn exe_path(pid: i32) -> Option<String> {
    pid_path(pid).or_else(|| {
        let exec = scyllaui_proto::parse_exec_path(&procargs_raw(pid)?)?;
        std::fs::canonicalize(exec).ok().map(|p| p.to_string_lossy().into_owned())
    })
}

/// 可执行文件的真实路径（已解析符号链接）。
pub fn pid_path(pid: i32) -> Option<String> {
    // 缓冲区必须正好是 PROC_PIDPATHINFO_MAXSIZE，更大会调用失败（与 keepd 的 S0 实测相同）。
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: 缓冲区大小与传入的长度一致。
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// 进程参数（`KERN_PROCARGS2`），其中的绝对路径解析为真实路径：`codex` 命令经符号链接指向 `codex.js`，
/// node 的参数里是链接路径。
fn resolved_args(pid: i32) -> Vec<String> {
    proc_args(pid)
        .into_iter()
        .map(|a| match a.starts_with('/') {
            true => std::fs::canonicalize(&a)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(a),
            false => a,
        })
        .collect()
}

fn proc_args(pid: i32) -> Vec<String> {
    procargs_raw(pid).map(|buf| scyllaui_proto::parse_procargs(&buf)).unwrap_or_default()
}

/// `KERN_PROCARGS2` 的原始内容（参数个数、启动路径、参数、环境）。
fn procargs_raw(pid: i32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: 先取所需大小，再按该大小分配缓冲区读取。
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let ok = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok != 0 {
        return None;
    }
    buf.truncate(size);
    Some(buf)
}


/// kqueue 监听进程退出：专门的线程阻塞在 kevent 上，退出时交给核心线程。空闲时不唤醒。
pub struct ExitWatch {
    kq: RawFd,
}

impl ExitWatch {
    pub fn new(core: Sender<Cmd>) -> std::io::Result<ExitWatch> {
        // SAFETY: kqueue 无参数；返回的描述符由本结构与监听线程共用，进程生命周期内不关闭。
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // kqueue 描述符不被 fork 继承（kqueue(2)），保活终端的 Shell（M3 起由 scyllauid 启动）拿不到它。
        std::thread::spawn(move || loop {
            // SAFETY: 读取一个事件到栈上的结构体。
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
            if n == 1 && ev.filter == libc::EVFILT_PROC {
                if core
                    .send(Cmd::ProcExit {
                        pid: ev.ident as i32,
                    })
                    .is_err()
                {
                    return;
                }
            } else if n < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return;
            }
        });
        Ok(ExitWatch { kq })
    }

    /// 开始监听；进程已不存在时返回 false（调用方按已退出处理）。
    pub fn watch(&self, pid: i32) -> bool {
        let ev = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: 只注册一个事件，不取回事件。
        unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
    }
}

/// 测试用的「只休眠」程序（2026-10-16）：macOS 的启动约束（AMFI Launch Constraint）会很快杀掉复制到别处运行的
/// Apple 系统程序（如 `/bin/sleep`，内核日志 Launch Constraint Violation），扮演 CLI 的测试因此时过时不过。改为复制
/// 本测试程序自己（ad-hoc 签名，不是系统程序），只运行其中被忽略的 `sleeper` 测试。
#[cfg(test)]
pub(crate) mod testbin {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// 把本测试程序写到 `dest`（用写入而不是复制：修改时间为现在）。
    pub fn install(dest: &Path) {
        let me = std::env::current_exe().unwrap();
        std::fs::write(dest, std::fs::read(me).unwrap()).unwrap();
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 运行 `exe`（`install` 写入的，或指向它的链接）：休眠 `secs` 秒。
    pub fn sleep(exe: &Path, secs: u32) -> Command {
        let mut c = Command::new(exe);
        c.args(["--exact", "procinfo::testbin::sleeper", "--ignored", "--test-threads=1", "-q"])
            .env("SCYLLAUI_TEST_SLEEP", secs.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }

    #[test]
    #[ignore = "只供 sleep() 在子进程中运行"]
    fn sleeper() {
        let secs = std::env::var("SCYLLAUI_TEST_SLEEP").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    /// 进程树统计（M2 第 2 块）：在一个伪终端里跑忙循环的子进程，CPU 计入、内存非零；同一进程两次采样的差值即用量。
    #[test]
    fn tree_sample_counts_processes_on_the_tty() {
        let mut master = 0;
        let mut slave = 0;
        // SAFETY: openpty 写入两个描述符。
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        use std::os::unix::process::CommandExt;
        let mut child = unsafe {
            std::process::Command::new("/bin/sh")
                .args(["-c", "while :; do :; done"])
                .pre_exec(move || {
                    libc::setsid();
                    libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let tdev = tty_of(child.id() as i32).expect("子进程应有控制终端");
        let a = tree_sample(tdev);
        let t = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let b = tree_sample(tdev);
        let pct = cpu_percent(&a, &b, t.elapsed());
        child.kill().unwrap();
        child.wait().unwrap();
        // SAFETY: 关闭本测试打开的描述符。
        unsafe { libc::close(master); libc::close(slave) };
        assert_eq!(a.cpu_ns.len(), 1, "只有这一个进程在该终端上");
        assert!(b.mem_bytes > 0);
        assert!(pct > 50.0 && pct < 150.0, "忙循环约占一个核：{pct}");
        assert_eq!(tty_of(std::process::id() as i32).filter(|t| *t == tdev), None);
    }

    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::sync::mpsc;
    use std::time::Duration;

    /// 把休眠程序（`testbin`）放到伪装路径，在独立进程组中运行（进程组 ID 即其 PID）。
    fn disguised(root: &Path, rel: &str) -> (Child, PathBuf) {
        let exe = root.join(rel);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        super::testbin::install(&exe);
        let child = super::testbin::sleep(&exe, 30)
            .process_group(0)
            .spawn()
            .unwrap();
        (child, exe)
    }

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scyllauid-proc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn wait_classify(pid: i32, want: impl Fn(&Foreground) -> bool) -> Foreground {
        // exec 完成前读到的是父进程映像，稍等片刻。
        for _ in 0..100 {
            let f = classify(pid);
            if want(&f) {
                return f;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        classify(pid)
    }

    #[test]
    fn identifies_disguised_cli_paths_in_real_processes() {
        let r = root("ident");
        for (rel, tool) in [
            (".kimi-code/bin/kimi", Tool::Kimi),
            (".grok/downloads/grok-1.0.50-macos-aarch64", Tool::Grok),
            ("nm/@anthropic-ai/claude-code/bin/claude.exe", Tool::Claude),
        ] {
            let (mut child, exe) = disguised(&r, rel);
            let pid = child.id() as i32;
            let f = wait_classify(pid, |f| matches!(f, Foreground::Agent { .. }));
            let real = std::fs::canonicalize(&exe)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                f,
                Foreground::Agent {
                    pid,
                    path: real.clone(),
                    tool
                },
                "{rel}"
            );
            // 版本来源指向被识别出的那个进程（M1 第 7 块）。
            let want = match tool {
                Tool::Kimi => VersionSource::Command { path: real.clone() },
                Tool::Grok => VersionSource::Known { version: "1.0.50".into(), file: real.clone() },
                _ => VersionSource::PackageJson {
                    path: real.replace("bin/claude.exe", "package.json"),
                    strip: "",
                },
            };
            assert_eq!(classify_full(pid).1, Some((want, real.clone())), "{rel}");
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn identifies_codex_script_argument_through_symlink() {
        let r = root("codex");
        let script = r.join("nm/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        // 末尾的 `:` 防止 Shell 把最后一条命令 exec 掉（那样参数就不再是脚本路径）。
        std::fs::write(&script, "sleep 30\n:\n").unwrap();
        // 与 nvm 安装相同：PATH 中的 `codex` 是指向 codex.js 的符号链接，node 的参数里是链接路径。
        let link = r.join("bin/codex");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&script, &link).unwrap();
        let mut child = Command::new("/bin/sh")
            .arg(&link)
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let f = wait_classify(pid, |f| matches!(f, Foreground::Agent { .. }));
        assert!(
            matches!(
                f,
                Foreground::Agent {
                    tool: Tool::Codex,
                    ..
                }
            ),
            "{f:?}"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn plain_commands_and_shells_are_not_agents() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let f = wait_classify(pid, |f| matches!(f, Foreground::Command { .. }));
        assert_eq!(
            f,
            Foreground::Command {
                pid,
                path: "/bin/sleep".into()
            }
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let mut sh = Command::new("/bin/zsh")
            .args(["-c", "sleep 30; :"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = sh.id() as i32;
        let f = wait_classify(pid, |f| matches!(f, Foreground::Shell { .. }));
        assert_eq!(
            f,
            Foreground::Shell {
                pid,
                path: "/bin/zsh".into()
            },
            "zsh 在 /etc/shells 中"
        );
        sh.kill().unwrap();
        sh.wait().unwrap();
        assert_eq!(classify(0), Foreground::Unknown);
        assert_eq!(classify(999_999), Foreground::Unknown, "不存在的进程组");
    }

    #[test]
    fn exit_watch_reports_kill9_and_refuses_dead_pids() {
        let (tx, rx) = mpsc::channel();
        let w = ExitWatch::new(tx).unwrap();
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(w.watch(pid));
        child.kill().unwrap(); // SIGKILL
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Cmd::ProcExit { pid: p } => assert_eq!(p, pid),
            _ => panic!("应收到进程退出"),
        }
        child.wait().unwrap();
        assert!(!w.watch(pid), "已退出的进程无法监听");
    }

    /// 运行中的可执行文件被替换（CLI 自动升级：旧版本删除、链接指向新版本）：`proc_pidpath` 读不到，
    /// 退回启动路径，解析链接后得到新版本的路径（2026-10-15 实测 Claude Code 2.1.296→2.1.300）。
    #[test]
    fn replaced_executable_falls_back_to_exec_path() {
        let dir = crate::store::test_dir("procinfo-replaced-exe");
        let (old, new, link) = (dir.join("v1"), dir.join("v2"), dir.join("sleep"));
        super::testbin::install(&old);
        std::os::unix::fs::symlink(&old, &link).unwrap();
        let mut child = super::testbin::sleep(&link, 30).spawn().unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));
        std::fs::remove_file(&old).unwrap();
        super::testbin::install(&new);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        let canon = std::fs::canonicalize(&new).unwrap().to_string_lossy().into_owned();
        let (direct, got) = (pid_path(pid), exe_path(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(direct, None, "旧文件已删，proc_pidpath 读不到");
        assert_eq!(got.as_deref(), Some(canon.as_str()));
    }

    #[test]
    fn start_time_of_a_running_process() {
        let ms = start_ms(std::process::id() as i32).expect("本进程的启动时间");
        let now = scyllaui_proto::now_ms();
        assert!(ms <= now && now - ms < 3_600_000, "{ms} vs {now}");
        assert_eq!(start_ms(i32::MAX), None);
    }

    #[test]
    fn parses_procargs_layout() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/node\0\0\0\0node\0/x/codex.js\0HOME=/u\0");
        assert_eq!(scyllaui_proto::parse_procargs(&buf), ["node", "/x/codex.js"]);
        assert_eq!(scyllaui_proto::parse_exec_path(&buf).as_deref(), Some("/usr/bin/node"));
        assert!(scyllaui_proto::parse_procargs(&[1, 0]).is_empty());
    }
}
