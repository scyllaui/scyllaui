//! 项目的 Git 分支（docs/design.md 第 5 节，docs/protocol.md 第 7 节）：从项目目录向上找 `.git`，读 `HEAD`，
//! 不调用 `git`、不读工作区。kqueue 监听 git 目录的写入（切换分支时 git 以改名方式替换 `HEAD`），收到即交给核心线程。
//! 找仓库、读分支、打开监听都在 `GitWorker` 线程里做，核心线程只收结果：项目在「文稿」等受保护目录下时，
//! 首次访问会弹系统授权并阻塞到用户处理，不能卡住启动与 Hook 处理。
//! M2 第 2 块起，标题栏显示的那个项目另在这个线程里调用 `git` 读改动数（docs/protocol.md 第 7 节「Git 改动数」）。

use crate::core::Cmd;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// 改动数的防抖：一串触发结束后再算（docs/protocol.md 第 7 节）。
const STATUS_DEBOUNCE: Duration = Duration::from_millis(500);
/// 每条 git 命令的超时。
const GIT_TIMEOUT: Duration = Duration::from_secs(3);
/// 空树的对象名：还没有提交的仓库与它比较。
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// 工作区的改动数（M2 第 2 块）：只在内存中，随项目的 `git` 字段下发。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GitStatus {
    pub dirty: bool,
    pub added: u64,
    pub deleted: u64,
    pub untracked: u64,
}

impl GitStatus {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"dirty": self.dirty, "added": self.added, "deleted": self.deleted, "untracked": self.untracked})
    }
}

/// `git status --porcelain=v2 -z` 的输出：有任何条目即有改动，`?` 条目数为未跟踪文件数。改名条目（`2`）后面多一个
/// 原路径字段，跳过；`#` 开头的是表头。
pub fn parse_porcelain(out: &[u8]) -> (bool, u64) {
    let mut dirty = false;
    let mut untracked = 0;
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        match f[0] {
            b'1' | b'u' => dirty = true,
            b'2' => {
                dirty = true;
                fields.next();
            }
            b'?' => {
                dirty = true;
                untracked += 1;
            }
            _ => {}
        }
    }
    (dirty, untracked)
}

/// `git diff --numstat -z` 的输出：每条为「增加\t删除\t路径」，改名时路径为空、后跟原路径与新路径两个字段；
/// 二进制文件的增删为 `-`，不计。
pub fn parse_numstat(out: &[u8]) -> (u64, u64) {
    let (mut added, mut deleted) = (0, 0);
    let mut fields = out.split(|&b| b == 0);
    while let Some(f) = fields.next() {
        let text = String::from_utf8_lossy(f);
        let mut parts = text.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        added += a.parse::<u64>().unwrap_or(0);
        deleted += d.parse::<u64>().unwrap_or(0);
        if path.is_empty() {
            fields.next();
            fields.next();
        }
    }
    (added, deleted)
}

/// 有没有可用的 git：没装命令行工具时 `/usr/bin/git` 会弹出安装提示，所以先问 `xcode-select -p`。
pub(crate) fn git_available() -> bool {
    Path::new("/usr/bin/git").exists()
        && Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// 在 `dir` 里运行一条 git 命令：不拿 index 锁、不执行用户配置的外部程序；超时即结束进程。失败为 None。
fn run_git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("/usr/bin/git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "core.quotePath=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                log!("git 超时或失败（{}）：{}", args.join(" "), dir.display());
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.filter(|s| s.success()).map(|_| out)
}

/// `run_git_ex` 的结果：输出（超过上限时只有前一部分，`truncated` 为 true）与退出码（被信号结束时为 -1）；超时时结束进程，
/// `timed_out` 为 true，输出为超时前读到的部分（内容搜索用它显示已找到的）。
pub(crate) struct GitOut {
    pub out: Vec<u8>,
    pub code: i32,
    pub truncated: bool,
    pub timed_out: bool,
}

/// 同 `run_git`，另可给标准输入、限制输出大小（右侧栏，docs/design.md 5.12 节）：输出超过 `max_bytes` 时停止读取并结束进程，
/// 返回已读部分；不看退出码是否成功，由调用方判断（`check-ignore` 没有匹配时以 1 退出）。启动失败为 None。
pub(crate) fn run_git_ex(dir: &Path, args: &[&str], input: &[u8], max_bytes: usize) -> Option<GitOut> {
    let mut child = Command::new("/usr/bin/git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "core.quotePath=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_empty() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take()?;
    let full = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = full.clone();
    let reader = std::thread::spawn(move || {
        let (mut buf, mut chunk, mut truncated) = (Vec::new(), vec![0u8; 64 << 10], false);
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) if buf.len() + n > max_bytes => {
                    buf.extend_from_slice(&chunk[..max_bytes - buf.len()]);
                    truncated = true;
                    flag.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        (buf, truncated)
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if !killed && full.load(std::sync::atomic::Ordering::Acquire) => {
                // 读到上限就不再读了：结束进程，不等它写完。
                let _ = child.kill();
                killed = true;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                log!("git 超时或失败（{}）：{}", args.join(" "), dir.display());
                break None;
            }
        }
    };
    let (out, truncated) = reader.join().ok()?;
    let timed_out = status.is_none() && !truncated;
    Some(GitOut { out, code: status.and_then(|s| s.code()).unwrap_or(-1), truncated, timed_out })
}

/// 仓库配置里有没有会被 `status` / `diff` 执行的过滤器（clean、smudge、process），且命令不是 git-lfs 的：
/// 2026-10-16 实测这两条命令会执行仓库配置里的 clean 过滤器，仓库配置可能来自不受信任的来源。只读配置，不执行程序。
pub fn has_untrusted_filter(config_lines: &[u8]) -> bool {
    String::from_utf8_lossy(config_lines)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .any(|(_, cmd)| !cmd.trim_start().starts_with("git-lfs "))
}

/// 工作区的改动数；不是仓库、git 出错或超时为 None；仓库配置了非 git-lfs 的过滤器时也为 None（不执行它）。
pub fn status(dir: &Path) -> Option<GitStatus> {
    // 没有匹配项时 git config 以 1 退出，run_git 返回 None，即没有过滤器。
    if let Some(filters) = run_git(dir, &["config", "--get-regexp", r"^filter\..*\.(clean|smudge|process)$"]) {
        if has_untrusted_filter(&filters) {
            log!("仓库配置了非 git-lfs 的过滤器，不读改动数：{}", dir.display());
            return None;
        }
    }
    let porcelain = run_git(dir, &["status", "--porcelain=v2", "-z", "--untracked-files=normal", "--ignore-submodules=all"])?;
    let diff = |base: &str| {
        run_git(dir, &["diff", base, "--numstat", "-z", "--no-ext-diff", "--no-textconv", "--ignore-submodules=all"])
    };
    let numstat = diff("HEAD").or_else(|| diff(EMPTY_TREE))?;
    let (dirty, untracked) = parse_porcelain(&porcelain);
    let (added, deleted) = parse_numstat(&numstat);
    Some(GitStatus { dirty, added, deleted, untracked })
}

/// 项目目录所在仓库的 git 目录：`.git` 目录本身，或 worktree / 子模块中 `.git` 文件里 `gitdir:` 指向的目录。
pub fn git_dir(dir: &Path) -> Option<PathBuf> {
    repo(dir).map(|(_, g)| g)
}

/// 向上找到的仓库：`(工作区根, git 目录)`。工作区根是 `.git`（目录或文件）所在的目录，终端按它归组为项目
/// （docs/protocol.md 第 4 节 `terminal.cwd`）。
pub fn repo(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot = d.join(".git");
        if dot.join("HEAD").is_file() {
            return Some((d.to_path_buf(), dot));
        }
        if dot.is_file() {
            let text = fs::read_to_string(&dot).ok()?;
            let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            let p = d.join(target);
            return p.join("HEAD").is_file().then(|| (d.to_path_buf(), p));
        }
        cur = d.parent();
    }
    None
}

/// 终端当前目录归属的项目目录：先解析成真实路径（符号链接，以及 `/var` → `/private/var`、`/tmp` → `/private/tmp`
/// 这类系统链接），再向上找仓库的工作区根，不在仓库里为真实路径本身；解析失败（目录已不存在、没有权限）时按原样。
/// 同一个仓库不论从哪条路径进入都归为同一个项目（2026-10-23 修正：此前按 Shell 上报的路径原样判定，`cd /var/…` 的
/// 终端与从它新建、Shell 报 `/private/var/…` 的终端分成了两组）。访问文件系统，只在 `GitWorker` 线程里调用。
pub fn project_root(cwd: &str) -> String {
    let real = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    repo(&real).map_or(real, |(root, _)| root).to_string_lossy().into_owned()
}

/// 当前分支：`ref: refs/heads/<名称>` 取名称；detached 时取 7 位短 commit 号；读不到为 None。
pub fn branch(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        return Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_owned());
    }
    (head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit())).then(|| head[..7].to_owned())
}

/// 交给 `GitWorker` 的文件系统工作，结果以 `Cmd` 送回核心线程。
pub enum Job {
    /// 找项目目录所在的仓库、读分支；`watch` 为 true 时监听其 git 目录（同一目录只监听一次）。
    Track { project_id: i64, dir: String, watch: bool },
    /// 终端当前目录所在仓库的工作区根（不在仓库里为目录本身），终端据此改归项目；`at` 为上报时刻。
    Resolve { terminal_id: String, cwd: String, at: i64 },
    /// git 目录有写入后重读分支。
    Reread { git_dir: PathBuf },
    /// 读标题栏显示的项目的改动数（M2 第 2 块）：防抖后执行，期间又来的请求合并。
    Status { project_id: i64, dir: String },
}

/// 读仓库与分支的工作线程：按收到的顺序逐个处理，空闲时阻塞在队列上。
pub struct GitWorker {
    tx: Sender<Job>,
}

impl GitWorker {
    pub fn new(kq: GitWatch, core: Sender<Cmd>) -> GitWorker {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let mut watched: HashMap<PathBuf, RawFd> = HashMap::new();
            let git_ok = git_available();
            if !git_ok {
                log!("没有可用的 git（xcode-select -p 失败），不读改动数");
            }
            // 等着防抖到期的改动数请求：（到期时间, 项目, 目录）。
            let mut pending: Option<(Instant, i64, String)> = None;
            loop {
                let job = match &pending {
                    None => match rx.recv() {
                        Ok(j) => j,
                        Err(_) => return,
                    },
                    Some((due, ..)) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                        Ok(j) => j,
                        Err(RecvTimeoutError::Timeout) => {
                            let (_, project_id, dir) = pending.take().unwrap();
                            let t = Instant::now();
                            let status = if git_ok { crate::guarded("读改动数", || status(Path::new(&dir))).flatten() } else { None };
                            log!("改动数：project={project_id} took={}ms {:?}", t.elapsed().as_millis(), status);
                            if core.send(Cmd::GitStatus { project_id, status }).is_err() {
                                return;
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    },
                };
                if let Job::Status { project_id, dir } = job {
                    let due = pending.as_ref().map_or_else(|| Instant::now() + STATUS_DEBOUNCE, |(d, ..)| *d);
                    pending = Some((due, project_id, dir));
                    continue;
                }
                let Some(cmd) = crate::guarded("读 Git 仓库", || match job {
                    Job::Status { .. } => unreachable!("上面已处理"),
                    Job::Track { project_id, dir, watch } => {
                        let git_dir = git_dir(Path::new(&dir));
                        let branch = git_dir.as_deref().and_then(branch);
                        let fd = match &git_dir {
                            Some(g) if watch => match watched.get(g) {
                                Some(fd) => Some(*fd),
                                None => {
                                    let fd = kq.watch(g);
                                    match fd {
                                        Some(fd) => {
                                            watched.insert(g.clone(), fd);
                                        }
                                        None => log!("监听 git 目录失败：{}", g.display()),
                                    }
                                    fd
                                }
                            },
                            _ => None,
                        };
                        Cmd::RepoFound { project_id, git_dir, branch, fd }
                    }
                    Job::Resolve { terminal_id, cwd, at } => {
                        let root = project_root(&cwd);
                        Cmd::CwdResolved { terminal_id, cwd, root, at }
                    }
                    Job::Reread { git_dir } => Cmd::BranchRead { branch: branch(&git_dir), git_dir },
                }) else {
                    continue;
                };
                if core.send(cmd).is_err() {
                    return;
                }
            }
        });
        GitWorker { tx }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

/// kqueue 监听 git 目录：专门的线程阻塞在 kevent 上，目录有写入时把描述符交给核心线程。空闲时不唤醒。
pub struct GitWatch {
    kq: RawFd,
}

impl GitWatch {
    pub fn new(core: Sender<Cmd>) -> std::io::Result<GitWatch> {
        // SAFETY: kqueue 无参数；描述符由本结构与监听线程共用，进程生命周期内不关闭。
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        std::thread::spawn(move || loop {
            // SAFETY: 读取一个事件到栈上的结构体。
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
            if n == 1 && ev.filter == libc::EVFILT_VNODE {
                if core.send(Cmd::GitChanged { fd: ev.ident as RawFd }).is_err() {
                    return;
                }
            } else if n < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return;
            }
        });
        Ok(GitWatch { kq })
    }

    /// 开始监听一个 git 目录，返回描述符（事件以它标识）；打不开时返回 None（只是没有实时更新）。
    pub fn watch(&self, git_dir: &Path) -> Option<RawFd> {
        let path = CString::new(git_dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: O_EVTONLY 只为接收事件打开，不阻止卸载卷；路径为以 NUL 结尾的 C 字符串。
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let ev = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: 只注册一个事件，不取回事件。
        if unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            // SAFETY: 关闭刚打开、尚未交出的描述符。
            unsafe { libc::close(fd) };
            return None;
        }
        Some(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use std::sync::mpsc;
    use std::time::Duration;

    fn write(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// 改动数的两种输出（M2 第 2 块）：改名条目多一个字段、二进制不计、表头不算。
    #[test]
    fn parses_status_and_numstat() {
        let porcelain = b"# branch.oid abc\x001 .M N... 100644 100644 100644 a b src/x.rs\x002 R. N... 100644 100644 100644 a b R100 new.rs\x00old.rs\x00? tmp.txt\x00? o.log\x00";
        assert_eq!(parse_porcelain(porcelain), (true, 2));
        assert_eq!(parse_porcelain(b""), (false, 0));
        let numstat = b"3\t1\tsrc/x.rs\x00-\t-\timg.png\x0010\t0\t\x00old.rs\x00new.rs\x00";
        assert_eq!(parse_numstat(numstat), (13, 1));
        assert_eq!(parse_numstat(b""), (0, 0));
    }

    /// 真实仓库：新建、修改、暂存、未跟踪、还没有提交（与空树比较）都与 git 自己的结果一致。
    #[test]
    fn status_of_a_real_repo() {
        if !git_available() {
            return;
        }
        let d = test_dir("git-status");
        let git = |args: &[&str]| {
            assert!(Command::new("/usr/bin/git").args(args).current_dir(&d).stdout(Stdio::null()).stderr(Stdio::null())
                .status().unwrap().success(), "{args:?}");
        };
        git(&["init", "-q"]);
        fs::write(d.join("a.txt"), "1\n2\n").unwrap();
        git(&["add", "a.txt"]);
        assert_eq!(status(&d), Some(GitStatus { dirty: true, added: 2, deleted: 0, untracked: 0 }), "还没有提交");
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "x"]);
        assert_eq!(status(&d), Some(GitStatus::default()), "干净");
        fs::write(d.join("a.txt"), "1\nzwei\n3\n").unwrap();
        fs::write(d.join("new.txt"), "n\n").unwrap();
        assert_eq!(status(&d), Some(GitStatus { dirty: true, added: 2, deleted: 1, untracked: 1 }));
        git(&["add", "a.txt"]);
        assert_eq!(status(&d), Some(GitStatus { dirty: true, added: 2, deleted: 1, untracked: 1 }), "暂存后仍相对 HEAD 计");
        assert_eq!(status(&d.join("..").join("not-a-repo-xyz")), None);
        // 仓库配置里的 clean 过滤器会被 status / diff 执行：配置了非 git-lfs 的过滤器时不读改动数，过滤器没有被执行。
        let marker = d.join("FILTER_RAN");
        fs::write(d.join(".gitattributes"), "*.txt filter=probe\n").unwrap();
        git(&["config", "filter.probe.clean", &format!("sh -c 'touch {}; cat'", marker.display())]);
        fs::write(d.join("a.txt"), "changed again\n").unwrap();
        assert_eq!(status(&d), None);
        assert!(!marker.exists(), "过滤器不应被执行");
        git(&["config", "filter.probe.clean", "git-lfs clean -- %f"]);
        assert!(status(&d).is_some(), "git-lfs 的过滤器照常读");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn untrusted_filter_detection() {
        assert!(!has_untrusted_filter(b""));
        assert!(!has_untrusted_filter(b"filter.lfs.clean git-lfs clean -- %f\nfilter.lfs.process git-lfs filter-process\n"));
        assert!(has_untrusted_filter(b"filter.lfs.clean git-lfs clean -- %f\nfilter.x.clean sh -c evil\n"));
    }

    /// 项目目录按真实路径判定：经符号链接进入同一仓库（含子目录）得到同一个工作区根；不在仓库里为真实路径本身；
    /// 解析不了的按原样。测试目录在 `$TMPDIR` 下，本身就是 `/var/folders/…`（→ `/private/var/folders/…`）这类路径。
    #[test]
    fn project_root_resolves_symlinks() {
        let d = test_dir("git-symlink");
        write(&d.join("repo/.git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(d.join("repo/src")).unwrap();
        fs::create_dir_all(d.join("plain")).unwrap();
        std::os::unix::fs::symlink(d.join("repo"), d.join("alias")).unwrap();
        let real = fs::canonicalize(d.join("repo")).unwrap().to_string_lossy().into_owned();
        for via in [d.join("repo"), d.join("alias"), d.join("alias/src"), d.join("repo/src")] {
            assert_eq!(project_root(&via.to_string_lossy()), real, "{}", via.display());
        }
        let plain = fs::canonicalize(d.join("plain")).unwrap().to_string_lossy().into_owned();
        assert_eq!(project_root(&d.join("plain").to_string_lossy()), plain);
        assert_eq!(project_root("/nonexistent-scyllaui/x"), "/nonexistent-scyllaui/x", "解析不了按原样");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn finds_branch_in_repo_subdir_worktree_and_detached() {
        let d = test_dir("git-layouts");
        // 普通仓库，项目目录在子目录里。
        write(&d.join("repo/.git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(d.join("repo/src/app")).unwrap();
        let g = git_dir(&d.join("repo/src/app")).unwrap();
        assert_eq!(g, d.join("repo/.git"));
        assert_eq!(repo(&d.join("repo/src/app")).unwrap().0, d.join("repo"), "工作区根");
        assert_eq!(branch(&g).as_deref(), Some("main"));
        // worktree：.git 是文件，gitdir 可为相对路径。
        write(&d.join("repo/.git/worktrees/wt/HEAD"), "ref: refs/heads/feature/x\n");
        write(&d.join("wt/.git"), "gitdir: ../repo/.git/worktrees/wt\n");
        let g = git_dir(&d.join("wt")).unwrap();
        assert_eq!(branch(&g).as_deref(), Some("feature/x"));
        assert_eq!(repo(&d.join("wt")).unwrap().0, d.join("wt"), "worktree 的工作区根是 .git 文件所在目录");
        // detached。
        write(&d.join("det/.git/HEAD"), "4f1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c\n");
        assert_eq!(branch(&git_dir(&d.join("det")).unwrap()).as_deref(), Some("4f1c2d3"));
        // 不是仓库（向上找到根也没有）；HEAD 内容异常。
        fs::create_dir_all(d.join("plain")).unwrap();
        assert!(git_dir(&d.join("plain")).is_none());
        write(&d.join("bad/.git/HEAD"), "garbage\n");
        assert_eq!(branch(&d.join("bad/.git")), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn replacing_head_by_rename_wakes_the_watch() {
        let d = test_dir("git-watch");
        write(&d.join(".git/HEAD"), "ref: refs/heads/main\n");
        let (tx, rx) = mpsc::channel();
        let w = GitWatch::new(tx).unwrap();
        let fd = w.watch(&d.join(".git")).unwrap();
        // git 的写法：先写 HEAD.lock，再改名覆盖 HEAD。
        write(&d.join(".git/HEAD.lock"), "ref: refs/heads/dev\n");
        fs::rename(d.join(".git/HEAD.lock"), d.join(".git/HEAD")).unwrap();
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Cmd::GitChanged { fd: got }) => assert_eq!(got, fd),
            _ => panic!("应收到 git 目录变化"),
        }
        assert_eq!(branch(&d.join(".git")).as_deref(), Some("dev"));
        let _ = fs::remove_dir_all(&d);
    }
}
