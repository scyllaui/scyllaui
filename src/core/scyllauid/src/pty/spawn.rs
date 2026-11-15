//! 在后台持有的 PTY 中启动登录 Shell（M3，docs/design.md 第 7 节「实现（M3）」）。移植自 S0 的
//! `keepd/src/pty.rs`，做法逐项对照固定 commit 的 Ghostty：
//! - `src/pty.zig` open：openpty、主端 CLOEXEC、主端开 IUTF8；childPreExec：13 个信号恢复默认、setsid、TIOCSCTTY。
//! - `src/termio/Exec.zig` Subprocess.init：环境变量；execCommand（macOS）：
//!   `/usr/bin/login [-q] -flp <用户> /bin/bash --noprofile --norc -c "exec -l <shell>"`。
//! - `src/apprt/embedded.zig` defaultTermioEnv、`src/termio/shell_integration.zig` setupZsh。
//! 与 S0 的差别：环境以 attach 进程的环境为底（风险 16）；子进程只保留 0/1/2 号描述符；`chdir` 失败经
//! CLOEXEC 管道报给后台（风险 14），与 Ghostty 一样不中止启动。

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

/// 与 vt.rs 的 XTVERSION 同源：`ghostty +version` 实测值。
pub const GHOSTTY_VERSION: &str = "1.3.2-HEAD+34f3900";
/// `Config.zig` ShellIntegrationFeatures 默认值（cursor、path、title），cursor-style-blink 未设置时取 blink。
/// attach 的环境里已有 libghostty 按用户配置设好的值时不覆盖。
const SHELL_FEATURES: &str = "cursor:blink,path,title";
/// ScyllaUI 自己的 zsh 入口，相对 App 包的 `Contents/Resources`（源文件 `scyllauid/shell/zsh/.zshenv`，M4b 块 5 P3）。
const OWN_ZSH_DIR: &str = "scyllaui-shell/zsh";

/// 子进程报告给后台的启动问题（错误管道里的第一个字节）。
pub const REPORT_CHDIR: u8 = b'c';
pub const REPORT_EXEC: u8 = b'e';

pub struct SpawnSpec {
    pub cwd: String,
    /// 环境的底：attach 进程的环境（libghostty 为该窗格构造的）。测试中可为空。
    pub base_env: Vec<(String, String)>,
    /// 直接执行的 argv（只在测试后台中使用）；为空时启动登录 Shell。
    pub command: Option<Vec<String>>,
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

pub struct Spawned {
    pub master: RawFd,
    pub pid: libc::pid_t,
    /// 错误管道的读端：exec 成功时读到 EOF；`chdir` 失败时先读到 `REPORT_CHDIR` + errno（4 字节小端）。
    pub report: RawFd,
}

struct Env(Vec<(String, String)>);

impl Env {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
    fn put(&mut self, k: &str, v: String) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k.to_string(), v)),
        }
    }
    fn remove(&mut self, k: &str) {
        self.0.retain(|(key, _)| key != k);
    }
}

/// Exec.zig appendEnv：原值为空时只取新值。已含该项时不再追加（attach 的环境已经过 libghostty 处理一次）。
fn append_env(current: &str, value: &str) -> String {
    if current.is_empty() {
        value.to_string()
    } else if current.split(':').any(|e| e == value) {
        current.to_string()
    } else {
        format!("{current}:{value}")
    }
}

pub fn build_env(base: Vec<(String, String)>, shell: &str) -> Vec<(String, String)> {
    let mut env = Env(base);
    // attach 外面那层 `bash -c "exec -l …"`（libghostty 包装命令的方式）设置的变量：去掉，由后台的
    // login、bash、zsh 照直连时的方式重新设置。
    for k in ["SHLVL", "_", "PWD", "OLDPWD"] {
        env.remove(k);
    }
    // embedded.zig defaultTermioEnv
    if env.get("__XCODE_BUILT_PRODUCTS_DIR_PATHS").is_some() {
        for k in [
            "__XCODE_BUILT_PRODUCTS_DIR_PATHS",
            "__XPC_DYLD_LIBRARY_PATH",
            "DYLD_FRAMEWORK_PATH",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "LD_LIBRARY_PATH",
            "SECURITYSESSIONID",
            "XPC_SERVICE_NAME",
        ] {
            env.remove(k);
        }
    }
    env.remove("GHOSTTY_MAC_LAUNCH_SOURCE");
    // Surface.zig
    env.remove("GHOSTTY_LOG");
    // Surface 的内部编号：后台持有的会话没有对应的 Surface，接回后 attach 也换了 Surface，不设置。
    env.remove("GHOSTTY_SURFACE_ID");

    // Exec.zig Subprocess.init
    let res = env.get("GHOSTTY_RESOURCES_DIR").map(str::to_string);
    match &res {
        Some(res) => {
            env.put("TERM", "xterm-ghostty".into());
            env.put("COLORTERM", "truecolor".into());
            let parent = Path::new(res)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            env.put("TERMINFO", format!("{parent}/terminfo"));
        }
        None => {
            env.put("TERM", "xterm-256color".into());
            env.put("COLORTERM", "truecolor".into());
        }
    }
    if let Some(bin) = env.get("GHOSTTY_BIN_DIR").map(str::to_string) {
        let path = env.get("PATH").unwrap_or("").to_string();
        env.put("PATH", append_env(&path, &bin));
    }
    if let Some(res) = &res {
        let xdg = env
            .get("XDG_DATA_DIRS")
            .unwrap_or("/usr/local/share:/usr/share")
            .to_string();
        env.put("XDG_DATA_DIRS", append_env(&xdg, &format!("{res}/..")));
        // Ghostty 写的是 `<原值>:<目录>`，原值为空时以冒号开头（表示同时使用系统默认的 man 路径）。
        let man = env.get("MANPATH").unwrap_or("").to_string();
        let dir = format!("{res}/../man");
        if !man.split(':').any(|e| e == dir) {
            env.put("MANPATH", format!("{man}:{dir}"));
        }
    }
    env.put("TERM_PROGRAM", "ghostty".into());
    env.put("TERM_PROGRAM_VERSION", GHOSTTY_VERSION.into());
    env.remove("VTE_VERSION");
    if env.get("GHOSTTY_SHELL_FEATURES").is_none() {
        env.put("GHOSTTY_SHELL_FEATURES", SHELL_FEATURES.into());
    }
    // shell_integration.zig setupZsh（本机登录 Shell 为 zsh；其他 Shell 不注入，与 Ghostty 检测失败时相同）。
    // M4b 块 5 P3（docs/design.md 10.6 节 D11）：包里有 ScyllaUI 自己的入口时 ZDOTDIR 指向它，它先执行 Ghostty 的
    // .zshenv 再定义 codex 函数；没有时（测试路径）照旧指向 Ghostty 的集成目录。
    if let Some(res) = &res {
        let integ = format!("{res}/shell-integration/zsh");
        if Path::new(shell).file_name().is_some_and(|n| n == "zsh") && Path::new(&integ).is_dir() {
            let dir = Path::new(res)
                .parent()
                .map(|p| p.join(OWN_ZSH_DIR).display().to_string())
                .filter(|d| Path::new(d).is_dir())
                .unwrap_or_else(|| integ.clone());
            if let Some(old) = env.get("ZDOTDIR").map(str::to_string).filter(|z| *z != integ && *z != dir) {
                env.put("GHOSTTY_ZSH_ZDOTDIR", old);
            }
            env.put("ZDOTDIR", dir);
        }
    }
    env.0
}

pub fn login_argv(user: &str, shell: &str, hush: bool) -> Vec<String> {
    let mut argv = vec!["/usr/bin/login".to_string()];
    if hush {
        argv.push("-q".into());
    }
    argv.extend([
        "-flp".into(),
        user.into(),
        "/bin/bash".into(),
        "--noprofile".into(),
        "--norc".into(),
        "-c".into(),
    ]);
    argv.push(format!("exec -l {shell}"));
    argv
}

/// 当前用户的用户名、家目录、登录 Shell（passwd）。
pub fn passwd() -> io::Result<(String, String, String)> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let s = |p: *const libc::c_char| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        Ok((s((*pw).pw_name), s((*pw).pw_dir), s((*pw).pw_shell)))
    }
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "含 NUL"))
}

/// 本进程此刻打开的描述符（`/dev/fd`）。子进程在 exec 前逐个关闭 2 号以上的，只留 0/1/2。
fn open_fds() -> Vec<RawFd> {
    std::fs::read_dir("/dev/fd")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .filter(|&fd: &RawFd| fd > 2)
                .collect()
        })
        .unwrap_or_default()
}

pub fn spawn(spec: &SpawnSpec) -> io::Result<Spawned> {
    let (user, home, shell) = passwd()?;
    let hush = Path::new(&home).join(".hushlogin").exists();
    let argv = spec
        .command
        .clone()
        .unwrap_or_else(|| login_argv(&user, &shell, hush));
    let env = build_env(spec.base_env.clone(), &shell);

    // fork 之后不再分配内存，参数全部提前转好。
    let c_argv: Vec<CString> = argv.iter().map(|a| cstr(a)).collect::<io::Result<_>>()?;
    let c_env: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstr(&format!("{k}={v}")))
        .collect::<io::Result<_>>()?;
    let mut p_argv: Vec<*const libc::c_char> = c_argv.iter().map(|c| c.as_ptr()).collect();
    p_argv.push(std::ptr::null());
    let mut p_env: Vec<*const libc::c_char> = c_env.iter().map(|c| c.as_ptr()).collect();
    p_env.push(std::ptr::null());
    let c_cwd = cstr(&spec.cwd)?;
    let c_home = cstr(&home)?;

    let mut ws = libc::winsize {
        ws_row: spec.rows.max(1),
        ws_col: spec.cols.max(1),
        ws_xpixel: (spec.cols as u32 * spec.cell_w).min(u16::MAX as u32) as u16,
        ws_ypixel: (spec.rows as u32 * spec.cell_h).min(u16::MAX as u32) as u16,
    };
    let (mut master, mut slave) = (-1, -1);
    let mut report = [-1; 2];
    unsafe {
        if libc::pipe(report.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in report {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        if libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) < 0 {
            let e = io::Error::last_os_error();
            libc::close(report[0]);
            libc::close(report[1]);
            return Err(e);
        }
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        let mut attrs: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(master, &mut attrs) == 0 {
            attrs.c_iflag |= libc::IUTF8;
            libc::tcsetattr(master, libc::TCSANOW, &attrs);
        }
        let (report_w, inherited) = (report[1], open_fds());

        let pid = libc::fork();
        if pid < 0 {
            let e = io::Error::last_os_error();
            for fd in [master, slave, report[0], report[1]] {
                libc::close(fd);
            }
            return Err(e);
        }
        if pid == 0 {
            for sig in [
                libc::SIGABRT,
                libc::SIGALRM,
                libc::SIGBUS,
                libc::SIGCHLD,
                libc::SIGFPE,
                libc::SIGHUP,
                libc::SIGILL,
                libc::SIGINT,
                libc::SIGPIPE,
                libc::SIGSEGV,
                libc::SIGTRAP,
                libc::SIGTERM,
                libc::SIGQUIT,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let empty: libc::sigset_t = 0;
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            // 后台的 socket、数据库、日志、锁文件等一律不留给 Shell；错误管道是 CLOEXEC，exec 时自动关闭。
            for &fd in &inherited {
                if fd != report_w {
                    libc::close(fd);
                }
            }
            for fd in [slave, master] {
                if fd > 2 {
                    libc::close(fd);
                }
            }
            if libc::chdir(c_cwd.as_ptr()) != 0 {
                let missing = matches!(*libc::__error(), libc::ENOENT | libc::ENOTDIR);
                child_report(report_w, REPORT_CHDIR);
                // 启动目录不存在：退回主目录（docs/design.md 10.5 节决策 12「不在则退回启动目录，再退回主目录并提示」），
                // 否则留在后台自己的目录「/」（2026-11-13 长跑实测）。无权限等其他原因照旧（风险 14）。
                if missing {
                    libc::chdir(c_home.as_ptr());
                }
            }
            libc::execve(p_argv[0], p_argv.as_ptr(), p_env.as_ptr());
            child_report(report_w, REPORT_EXEC);
            libc::_exit(127);
        }
        libc::close(slave);
        libc::close(report_w);
        Ok(Spawned { master, pid, report: report[0] })
    }
}

/// 子进程里（fork 之后、exec 之前）把 errno 写进错误管道：1 字节种类 + 4 字节小端 errno。只用异步信号安全的调用。
unsafe fn child_report(fd: RawFd, kind: u8) {
    let e = *libc::__error();
    let b = e.to_le_bytes();
    let msg = [kind, b[0], b[1], b[2], b[3]];
    libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len());
}

/// 读错误管道直到 EOF（exec 成功或子进程退出）。返回其中报告的问题（种类、errno）。会阻塞，在单独线程里调用。
pub fn read_report(fd: RawFd) -> Vec<(u8, i32)> {
    let mut all = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            break;
        }
        all.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { libc::close(fd) };
    all.chunks_exact(5)
        .map(|c| (c[0], i32::from_le_bytes([c[1], c[2], c[3], c[4]])))
        .collect()
}

pub fn set_size(master: RawFd, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: (cols as u32 * cell_w).min(u16::MAX as u32) as u16,
        ws_ypixel: (rows as u32 * cell_h).min(u16::MAX as u32) as u16,
    };
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
}

/// 前台进程组（M3 下 `ghostty_surface_foreground_pid` 读到的是 attach，改由后台对主端读取，docs/design.md 4.2 节）。
pub fn foreground_pgid(master: RawFd) -> Option<i32> {
    let pgid = unsafe { libc::tcgetpgrp(master) };
    (pgid > 0).then_some(pgid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }

    fn attach_env() -> Vec<(String, String)> {
        // libghostty 为 attach 构造的环境（App 包在 /A），外加包装它的 bash 设置的变量。
        [
            ("PATH", "/usr/bin:/bin:/A/ScyllaUI.app/Contents/MacOS"),
            ("GHOSTTY_RESOURCES_DIR", "/A/ScyllaUI.app/Contents/Resources/ghostty"),
            ("GHOSTTY_BIN_DIR", "/A/ScyllaUI.app/Contents/MacOS"),
            ("TERM", "xterm-ghostty"),
            ("XDG_DATA_DIRS", "/usr/local/share:/usr/share:/A/ScyllaUI.app/Contents/Resources/ghostty/.."),
            ("MANPATH", ":/A/ScyllaUI.app/Contents/Resources/ghostty/../man"),
            ("GHOSTTY_SHELL_FEATURES", "cursor:blink,path,sudo,title"),
            ("GHOSTTY_SURFACE_ID", "7"),
            ("SHLVL", "1"),
            ("_", "/A/ScyllaUI.app/Contents/Helpers/scyllaui"),
            ("PWD", "/tmp"),
            ("VTE_VERSION", "1"),
            ("SCYLLAUI_PANE_ID", "p1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn env_from_attach_follows_ghostty_without_duplicates() {
        let env = build_env(attach_env(), "/bin/zsh");
        assert_eq!(get(&env, "PATH"), Some("/usr/bin:/bin:/A/ScyllaUI.app/Contents/MacOS"));
        assert_eq!(
            get(&env, "XDG_DATA_DIRS"),
            Some("/usr/local/share:/usr/share:/A/ScyllaUI.app/Contents/Resources/ghostty/..")
        );
        assert_eq!(get(&env, "MANPATH"), Some(":/A/ScyllaUI.app/Contents/Resources/ghostty/../man"));
        assert_eq!(get(&env, "TERMINFO"), Some("/A/ScyllaUI.app/Contents/Resources/terminfo"));
        assert_eq!(get(&env, "TERM_PROGRAM"), Some("ghostty"));
        // libghostty 按用户配置设的值保留。
        assert_eq!(get(&env, "GHOSTTY_SHELL_FEATURES"), Some("cursor:blink,path,sudo,title"));
        for gone in ["GHOSTTY_SURFACE_ID", "SHLVL", "_", "PWD", "VTE_VERSION"] {
            assert_eq!(get(&env, gone), None, "{gone} 应去掉");
        }
        assert_eq!(get(&env, "SCYLLAUI_PANE_ID"), Some("p1"));
        // 集成目录不存在（测试路径），不注入 ZDOTDIR。
        assert_eq!(get(&env, "ZDOTDIR"), None);
        // 再处理一遍结果不变（接回、接管后重算时不会越加越长）。
        assert_eq!(build_env(env.clone(), "/bin/zsh"), env);
    }

    #[test]
    fn env_without_ghostty_resources_falls_back() {
        let env = build_env(vec![("PATH".into(), "/usr/bin".into())], "/bin/zsh");
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "GHOSTTY_SHELL_FEATURES"), Some(SHELL_FEATURES));
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
    }

    /// 搭一个与 App 包相同的 Resources：Ghostty 的 zsh 集成取 vendor 里的真实文件，`own` 为真时加上 ScyllaUI 的入口。
    fn resources(name: &str, own: bool) -> std::path::PathBuf {
        let res = crate::store::test_dir(name).join("Resources");
        let src = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ghostty = res.join("ghostty/shell-integration/zsh");
        std::fs::create_dir_all(&ghostty).unwrap();
        let vendor = src.join("../../../vendor/ghostty/zig-out/share/ghostty/shell-integration/zsh");
        for f in [".zshenv", "ghostty-integration"] {
            std::fs::copy(vendor.join(f), ghostty.join(f)).unwrap();
        }
        if own {
            let dir = res.join(OWN_ZSH_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(src.join("shell/zsh/.zshenv"), dir.join(".zshenv")).unwrap();
        }
        res
    }

    #[test]
    fn zdotdir_points_to_own_entry_when_bundled() {
        for own in [false, true] {
            let res = resources(&format!("zdotdir-{own}"), own);
            let base = vec![
                ("GHOSTTY_RESOURCES_DIR".to_string(), res.join("ghostty").display().to_string()),
                ("ZDOTDIR".to_string(), "/old".to_string()),
            ];
            let env = build_env(base, "/bin/zsh");
            let want = if own { res.join(OWN_ZSH_DIR) } else { res.join("ghostty/shell-integration/zsh") };
            assert_eq!(get(&env, "ZDOTDIR"), Some(want.display().to_string().as_str()));
            assert_eq!(get(&env, "GHOSTTY_ZSH_ZDOTDIR"), Some("/old"));
            // 再处理一遍不变：集成目录不会被当成用户原来的 ZDOTDIR。
            assert_eq!(build_env(env.clone(), "/bin/zsh"), env);
            let _ = std::fs::remove_dir_all(res.parent().unwrap());
        }
    }

    /// 用真实 zsh 跑入口：`zdotdir` 为 None 时是不经过 ScyllaUI 入口的普通 zsh。返回 `-c` 命令的输出。
    fn zsh(res: &Path, zdotdir: Option<&Path>, user: &Path, cmd: &str) -> String {
        let mut c = std::process::Command::new("/bin/zsh");
        c.env_clear()
            .env("HOME", user)
            .env("PATH", format!("{}:/usr/bin:/bin", user.join("bin").display()))
            .env("GHOSTTY_RESOURCES_DIR", res.join("ghostty"))
            .args(["-i", "-c", cmd])
            .stdin(std::process::Stdio::null());
        match zdotdir {
            Some(z) => c.env("ZDOTDIR", z).env("GHOSTTY_ZSH_ZDOTDIR", user),
            None => c.env("ZDOTDIR", user),
        };
        let out = c.output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn zsh_entry_adds_no_daemon_to_codex_only_where_codex_accepts_it() {
        let res = resources("zsh-codex", true);
        let user = res.parent().unwrap().join("user");
        std::fs::create_dir_all(user.join("bin")).unwrap();
        let fake = user.join("bin/codex");
        std::fs::write(&fake, "#!/bin/sh\necho \"codex $*\"\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let own = res.join(OWN_ZSH_DIR);
        let cases = [
            ("codex", "codex --no-daemon"),
            ("codex '修一下测试'", "codex --no-daemon 修一下测试"),
            ("codex -m o3", "codex --no-daemon -m o3"),
            ("codex resume --last", "codex --no-daemon resume --last"),
            ("codex fork", "codex --no-daemon fork"),
            ("codex exec x", "codex --no-daemon exec x"),
            ("codex agents", "codex agents"),
            ("codex queue a b", "codex queue a b"),
            ("codex --remote ws://x", "codex --remote ws://x"),
            ("codex --remote=ws://x", "codex --remote=ws://x"),
            ("codex app-server daemon start", "codex app-server daemon start"),
            ("codex --no-daemon", "codex --no-daemon"),
        ];
        for (cmd, want) in cases {
            assert_eq!(zsh(&res, Some(&own), &user, cmd).trim(), want, "{cmd}");
        }
        // Ghostty 的流程照旧：ZDOTDIR 还原成用户的，集成已加载。
        assert_eq!(
            zsh(&res, Some(&own), &user, "print -r -- $ZDOTDIR ${+GHOSTTY_ZSH_ZDOTDIR} ${+functions[_ghostty_deferred_init]}").trim(),
            format!("{} 0 1", user.display())
        );
        // 不经过 ScyllaUI 入口的普通 zsh 没有这个函数。
        assert_eq!(zsh(&res, None, &user, "whence -w codex").trim(), "codex: command");
        // 用户自己的别名（在 .zshenv 里，早于入口定义函数）与函数（在 .zshrc 里）都以用户的为准。
        std::fs::write(user.join(".zshenv"), "alias codex='echo user-alias'\n").unwrap();
        assert_eq!(zsh(&res, Some(&own), &user, "codex x").trim(), "user-alias x");
        std::fs::remove_file(user.join(".zshenv")).unwrap();
        std::fs::write(user.join(".zshrc"), "codex() { echo user-fn \"$@\"; }\n").unwrap();
        assert_eq!(zsh(&res, Some(&own), &user, "codex x").trim(), "user-fn x");
        let _ = std::fs::remove_dir_all(res.parent().unwrap());
    }

    /// OpenCode 2 在 ScyllaUI 的终端里用私有服务（2026-10-31）：交互界面、`run`、`mini` 补 `--standalone`，其余子命令与
    /// OpenCode 1 原样。
    #[test]
    fn zsh_entry_adds_standalone_to_opencode_2_only() {
        let res = resources("zsh-opencode", true);
        let user = res.parent().unwrap().join("user");
        std::fs::create_dir_all(user.join("bin")).unwrap();
        std::fs::create_dir_all(user.join("proj")).unwrap();
        let fake = user.join("bin/opencode");
        let write = |version: &str| {
            std::fs::write(&fake, format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '{version}' && exit 0\necho \"opencode $*\"\n")).unwrap();
            std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        };
        let own = res.join(OWN_ZSH_DIR);
        write("opencode v2.2.4");
        let proj = user.join("proj").display().to_string();
        let cases = [
            ("opencode".to_owned(), "opencode --standalone".to_owned()),
            ("opencode -s ses_abc".to_owned(), "opencode --standalone -s ses_abc".to_owned()),
            (format!("opencode {proj}"), format!("opencode --standalone {proj}")),
            ("opencode run 只回复".to_owned(), "opencode run --standalone 只回复".to_owned()),
            ("opencode mini".to_owned(), "opencode mini --standalone".to_owned()),
            ("opencode service status".to_owned(), "opencode service status".to_owned()),
            ("opencode upgrade".to_owned(), "opencode upgrade".to_owned()),
            ("opencode --server http://x".to_owned(), "opencode --server http://x".to_owned()),
            ("opencode --standalone -c".to_owned(), "opencode --standalone -c".to_owned()),
        ];
        for (cmd, want) in &cases {
            assert_eq!(zsh(&res, Some(&own), &user, cmd).trim(), want, "{cmd}");
        }
        write("1.19.5");
        assert_eq!(zsh(&res, Some(&own), &user, "opencode -s ses_abc").trim(), "opencode -s ses_abc", "OpenCode 1 没有 --standalone");
        let _ = std::fs::remove_dir_all(res.parent().unwrap());
    }

    #[test]
    fn login_command_matches_ghostty() {
        assert_eq!(
            login_argv("u", "/bin/zsh", false).join(" "),
            "/usr/bin/login -flp u /bin/bash --noprofile --norc -c exec -l /bin/zsh"
        );
        assert_eq!(login_argv("u", "/bin/zsh", true)[1], "-q");
    }
}
