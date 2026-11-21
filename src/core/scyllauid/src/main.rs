//! `scyllauid`：常驻后台（docs/protocol.md）。由 App 按需拉起，App 退出后继续运行；数据目录里的
//! socket 路径固定，单实例由文件锁保证。核心线程独占状态与数据库（core.rs），连接线程只负责收发
//! （conn.rs），空闲时不唤醒；不用 tokio（docs/design.md 第 9 节）。
//!
//! 用法：scyllauid [--data-dir <目录>] [--capture <目录>] [--takeover <接管文件>] | scyllauid --build-id
//!       scyllauid --preflight --data-dir <目录> [--takeover-format <N>]（换版本前的自检，docs/protocol.md 第 1 节）

/// 日志写到 stderr（App 拉起时接到 `~/Library/Logs/ScyllaUI/scyllauid.log`），带本地时间与进程号。
macro_rules! log {
    ($($arg:tt)*) => { $crate::log_line(format_args!($($arg)*)) };
}

mod approval;
mod cliquota;
mod conn;
mod core;
mod git;
mod hooks;
mod hooktime;
mod procinfo;
mod prompts;
mod pty;
mod retention;
mod route;
mod store;
mod title;
mod version;
mod waitwatch;
mod workspace;

use scyllaui_proto::{default_data_dir, now_ms, socket_path, BUILD_ID, PROTOCOL_VERSION};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// sockaddr_un.sun_path 为 104 字节，含结尾的 NUL。
const MAX_SOCK_PATH: usize = 103;
/// 换版本时旧进程可能还没退出，取锁最多等这么久。
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// 另一个实例持有锁时的退出码。
const EXIT_LOCKED: i32 = 3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir = None;
    let mut capture = None;
    let mut takeover = None;
    let mut preflight = false;
    let mut format = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--build-id" => {
                println!("{BUILD_ID}");
                return;
            }
            "--data-dir" => data_dir = it.next().map(PathBuf::from),
            "--capture" => capture = it.next().map(PathBuf::from),
            "--takeover" => takeover = it.next().map(PathBuf::from),
            "--preflight" => preflight = true,
            "--takeover-format" => format = it.next().and_then(|v| v.parse::<u64>().ok()),
            _ => fail(&format!("未知参数 {a}")),
        }
    }
    let data_dir = data_dir
        .or_else(default_data_dir)
        .unwrap_or_else(|| fail("无法确定数据目录（HOME 未设置）"));
    if preflight {
        // 只读检查，不建目录、不取锁、不写日志以外的任何东西。
        if format.is_some_and(|f| f != pty::takeover::FORMAT) {
            fail(&format!("不支持接管文件格式 {format:?}"));
        }
        store::Store::check(&data_dir.join("scyllaui.db")).unwrap_or_else(|e| fail(&e));
        return;
    }
    prepare_dir(&data_dir)
        .unwrap_or_else(|e| fail(&format!("数据目录 {} 不可用：{e}", data_dir.display())));
    // 换版本接管（M3）：沿用旧进程的单实例锁与各会话的主端，不重新取锁。
    let handoff = takeover.map(|f| pty::takeover::read(&f).unwrap_or_else(|e| fail(&format!("接管失败：{e}"))));
    let _lock = match &handoff {
        // SAFETY: 描述符由旧进程（同一进程号，exec 之前）去掉 CLOEXEC 后交过来，此后只由这个 File 持有。
        Some(h) => unsafe { File::from_raw_fd(h.lock_fd) },
        None => lock(&data_dir.join("scyllauid.lock")),
    };
    pty::takeover::set_cloexec(_lock.as_raw_fd(), true);
    // 上次（被强制结束、崩溃或换版本接管前）没来得及结束的查额度进程（docs/design.md 3.1.1 节）。
    cliquota::reap_leftovers(&data_dir);

    let sock = socket_path(&data_dir);
    if sock.as_os_str().len() > MAX_SOCK_PATH {
        fail(&format!(
            "socket 路径超过 {MAX_SOCK_PATH} 字节：{}",
            sock.display()
        ));
    }
    let db = data_dir.join("scyllaui.db");
    let store = store::Store::open(&db).unwrap_or_else(|e| fail(&e));
    if let Some(dir) = &capture {
        fs::create_dir_all(dir).unwrap_or_else(|e| fail(&format!("创建采集目录失败：{e}")));
    }
    let listener = match handoff.as_ref().and_then(|h| h.listen_fd) {
        // 换版本接管：沿用旧进程的监听 socket，exec 期间连进来的连接还在它的队列里（2026-11-14）。
        // SAFETY: 描述符由旧进程（同一进程号，exec 之前）去掉 CLOEXEC 后交过来，此后只由这个 UnixListener 持有。
        Some(fd) => {
            pty::takeover::set_cloexec(fd, true);
            log!("沿用旧进程的监听 socket");
            unsafe { UnixListener::from_raw_fd(fd) }
        }
        None => {
            // 持有锁就说明没有别的实例在用这个 socket：残留文件是上次异常退出留下的。
            match fs::remove_file(&sock) {
                Ok(()) => log!("删除上次遗留的 socket"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => fail(&format!("删除遗留 socket 失败：{e}")),
            }
            UnixListener::bind(&sock)
                .and_then(|l| fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)).map(|_| l))
                .unwrap_or_else(|e| fail(&format!("监听 {} 失败：{e}", sock.display())))
        }
    };
    let (tx, rx) = mpsc::channel();
    let exits = procinfo::ExitWatch::new(tx.clone())
        .unwrap_or_else(|e| fail(&format!("创建 kqueue 失败：{e}")));
    let kq = git::GitWatch::new(tx.clone()).unwrap_or_else(|e| fail(&format!("创建 kqueue 失败：{e}")));
    let git = git::GitWorker::new(kq, tx.clone());
    let test_backend = Some(&data_dir) != default_data_dir().as_ref();
    let titles = title::TitleWorker::new(tx.clone(), !test_backend);
    let routes = route::RouteWorker::new(tx.clone());
    // 四款 CLI 的配置所在的家目录：测试时由 App 以 SCYLLAUI_CLI_HOME 指向临时目录（docs/protocol.md 第 1 节）。
    // 原样使用，不规范化：Codex 信任记录的键按 HOME 原样拼接。
    let cli_home_env = std::env::var("SCYLLAUI_CLI_HOME").ok().filter(|s| !s.is_empty());
    // 测试后台（数据目录不是默认位置）没有指定 CLI 配置目录时，只读不改真实配置（docs/protocol.md 第 1 节）。
    let guard = cli_home_env.is_none() && Some(&data_dir) != default_data_dir().as_ref();
    let cli_home = cli_home_env
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| fail("无法确定 CLI 配置目录（HOME 未设置）"));
    let hook_watch = hooks::HookWatch::new(tx.clone(), cli_home.clone())
        .unwrap_or_else(|e| fail(&format!("创建 kqueue 失败：{e}")));
    let versions = version::VersionWorker::new(tx.clone());
    let waits = waitwatch::WaitWatch::new(tx.clone()).unwrap_or_else(|e| fail(&format!("创建 kqueue 失败：{e}")));
    log!("CLI 配置目录 {cli_home}{}", if guard { "（测试后台，只读）" } else { "" });
    let host = pty::Host::new(tx.clone());
    let listen_fd = listener.as_raw_fd();
    let (gate, accept) = conn::accept_loop(listener, tx.clone());
    let adopted = handoff.map(|h| host.adopt(h));
    let mut core = core::Core::new(store, sock.clone(), capture, exits, git, titles, routes, hook_watch, versions, waits, cli_home, host, data_dir.join("retention.json"))
        .unwrap_or_else(|e| fail(&format!("读取数据库失败：{e}")))
        .guard_real_config(guard)
        .allow_command(test_backend)
        .lock_fd(_lock.as_raw_fd())
        .listener(listen_fd, gate)
        .approvals(approval::ApprovalWatch::new(tx.clone()).unwrap_or_else(|e| fail(&format!("创建 kqueue 失败：{e}"))))
        .prompts(prompts::PromptWorker::new(tx.clone()))
        .workspace(workspace::WorkspaceDirs::load(data_dir.join("workspace.json")))
        .hook_times(hooktime::Timeline::load(data_dir.join("hook-timeline.json")));
    if let Some(adopted) = adopted {
        log!("换版本接管：接到 {} 个保活终端", adopted.len());
        core.adopt(adopted);
    }
    log!(
        "启动 build={BUILD_ID} protocol={PROTOCOL_VERSION} sock={} db={}",
        sock.display(),
        db.display()
    );

    // 核心线程 panic 时整个进程退出，不留半死的后台（docs/design.md 风险 17）：保活终端随之结束，App 重连后对账。
    // 其他工作线程 panic 只结束该线程（日志里有 Rust 默认的 panic 信息）。
    std::thread::spawn(move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| core.run(rx))).is_err() {
            log!("核心线程 panic，退出");
            std::process::abort();
        }
    });
    accept();
}

fn fail(msg: &str) -> ! {
    log!("{msg}");
    std::process::exit(1);
}

/// 数据目录权限必须是 0700（docs/design.md 第 8 节）；不存在则按 0700 创建。
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other("权限不是 0700"));
    }
    Ok(())
}

/// 单实例锁（flock）。进程退出时由系统释放，异常退出也不会残留。
fn lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap_or_else(|e| fail(&format!("打开锁文件 {} 失败：{e}", path.display())));
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return file,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                log!(
                    "另一个 scyllauid 正在运行（{} 已被锁定），退出",
                    path.display()
                );
                std::process::exit(EXIT_LOCKED);
            }
            Err(TryLockError::Error(e)) => fail(&format!("锁定 {} 失败：{e}", path.display())),
        }
    }
}

/// 工作线程的一次任务：panic 时记日志并返回 None，线程接着处理下一个（docs/design.md 10.5 节决策 21）。原来一次 panic
/// 会让该线程悄悄结束，相应功能从此不再响应。核心线程不用它，仍是 panic 即退出（风险 17）。
pub(crate) fn guarded<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned());
            log!("{what}出错，跳过这次任务：{}", msg.unwrap_or_default());
            None
        }
    }
}

pub(crate) fn log_line(args: std::fmt::Arguments) {
    let ms = now_ms();
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r 只写入调用方提供的 tm。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    eprintln!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} scyllauid[{}]: {args}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms % 1000,
        std::process::id()
    );
}
