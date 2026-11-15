//! CLI 版本识别（M1 第 7 块，docs/design.md 4.7 节「版本识别」）：按适配器给出的来源读版本，在单独线程里做
//! （Kimi 要执行 `--version`，约 0.6 s），核心线程只收结果。按（文件、修改时间、大小）缓存，同一文件只取一次；没读成（超时等）不缓存。
//! 版本来源文件或可执行文件的修改时间晚于进程启动时间时，说明进程启动后文件被替换（CLI 运行中自动升级），
//! 读到的不是正在运行的版本，记为未知。

use crate::core::Cmd;
use scyllaui_proto::adapters::{parse_package_version, parse_version_output, VersionSource};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

/// 执行 `--version` 的超时。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

pub struct VersionJob {
    pub terminal_id: String,
    pub pid: i32,
    /// 进程启动时间（Unix 毫秒）；读不到为 None（此时不做替换判断）。
    pub start_ms: Option<i64>,
    pub source: VersionSource,
    /// 被识别出的那个进程的可执行文件。
    pub exe: String,
}

pub struct VersionWorker {
    tx: Sender<VersionJob>,
}

impl VersionWorker {
    pub fn new(core: Sender<Cmd>) -> VersionWorker {
        let mut cache = HashMap::new();
        VersionWorker::with_resolver(core, move |job| resolve(&mut cache, job))
    }

    /// 测试用：注入解析函数。
    pub fn with_resolver(
        core: Sender<Cmd>,
        mut resolve: impl FnMut(&VersionJob) -> Option<String> + Send + 'static,
    ) -> VersionWorker {
        let (tx, rx) = mpsc::channel::<VersionJob>();
        std::thread::spawn(move || {
            for job in rx {
                let version = crate::guarded("识别 CLI 版本", || resolve(&job)).flatten();
                let cmd = Cmd::VersionRead { terminal_id: job.terminal_id, pid: job.pid, version };
                if core.send(cmd).is_err() {
                    return;
                }
            }
        });
        VersionWorker { tx }
    }

    pub fn request(&self, job: VersionJob) {
        let _ = self.tx.send(job);
    }
}

/// 缓存键：来源文件、修改时间（纳秒）、大小。
type Key = (PathBuf, i64, u64);

fn mtime_ms(path: &str) -> Option<(i64, Key)> {
    let m = std::fs::metadata(path).ok()?;
    let ns = m.mtime() * 1_000_000_000 + m.mtime_nsec();
    Some((ns / 1_000_000, (PathBuf::from(path), ns, m.size())))
}

pub fn resolve(cache: &mut HashMap<Key, Option<String>>, job: &VersionJob) -> Option<String> {
    let file = match &job.source {
        VersionSource::PackageJson { path, .. } => path,
        VersionSource::Known { file, .. } => file,
        VersionSource::Command { path } => path,
    };
    let (src_ms, key) = mtime_ms(file)?;
    let exe_ms = mtime_ms(&job.exe).map(|(ms, _)| ms).unwrap_or(0);
    if let Some(start) = job.start_ms {
        if src_ms.max(exe_ms) > start {
            log!(
                "版本未知：{} 在进程启动后被替换（pid={}，文件 {} > 启动 {}）",
                file,
                job.pid,
                src_ms.max(exe_ms),
                start
            );
            return None;
        }
    }
    if let Some(v) = cache.get(&key) {
        return v.clone();
    }
    // 外层 None：没读成（文件读不出、命令起不来或超时），不缓存，下次再读；命令超时立即重试一次
    // （2026-11-11 macOS 26 虚拟机：第一次运行 OpenCode 时同时执行的 `--version` 超过 3 秒，之后 0.1 秒）。
    let read = match &job.source {
        VersionSource::PackageJson { path, strip } => {
            std::fs::read_to_string(path).ok().map(|t| parse_package_version(&t, strip))
        }
        VersionSource::Known { version, .. } => Some(Some(version.clone())),
        VersionSource::Command { path } => run_version(path).or_else(|| run_version(path)),
    };
    let version = read.clone().flatten();
    log!("读取版本：{file} → {}", version.as_deref().unwrap_or("未知"));
    if read.is_some() {
        cache.insert(key, version.clone());
    }
    version
}

/// 执行 `<path> --version`：环境只留 HOME 与最小 PATH，标准输入为空，超时即结束进程。
/// 起不来或超时返回 None；跑完了返回解析结果（解析不出为 `Some(None)`）。
fn run_version(path: &str) -> Option<Option<String>> {
    let mut child = Command::new(path)
        .arg("--version")
        .env_clear()
        .env("HOME", std::env::var_os("HOME")?)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                log!("执行 {path} --version 超时");
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(parse_version_output(&out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use std::os::unix::fs::PermissionsExt;

    fn job(source: VersionSource, exe: &str, start_ms: Option<i64>) -> VersionJob {
        VersionJob { terminal_id: "t".into(), pid: 1, start_ms, source, exe: exe.into() }
    }

    /// 一次任务 panic 不让工作线程结束，下一个任务照常处理（docs/design.md 10.5 节决策 21）。
    #[test]
    fn worker_survives_a_panicking_job() {
        let (tx, rx) = std::sync::mpsc::channel();
        let w = VersionWorker::with_resolver(tx, |j| if j.pid == 1 { panic!("坏数据") } else { Some("1.0".into()) });
        let src = || VersionSource::Command { path: "/bin/x".into() };
        w.request(job(src(), "/bin/x", None));
        w.request(VersionJob { pid: 2, ..job(src(), "/bin/x", None) });
        let got: Vec<_> = (0..2)
            .map(|_| match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Cmd::VersionRead { pid, version, .. } => (pid, version),
                _ => panic!("意外的命令"),
            })
            .collect();
        assert_eq!(got, [(1, None), (2, Some("1.0".to_owned()))]);
    }

    #[test]
    fn reads_package_json_and_caches_by_mtime() {
        let dir = test_dir("version-pkg");
        let pkg = dir.join("package.json");
        std::fs::write(&pkg, r#"{"name":"x","version":"0.162.1-darwin-arm64"}"#).unwrap();
        let src = VersionSource::PackageJson { path: pkg.display().to_string(), strip: "-darwin-arm64" };
        let mut cache = HashMap::new();
        let exe = dir.join("codex");
        std::fs::write(&exe, "").unwrap();
        let now = scyllaui_proto::now_ms() + 10_000;
        assert_eq!(resolve(&mut cache, &job(src.clone(), &exe.display().to_string(), Some(now))).as_deref(), Some("0.162.1"));
        assert_eq!(cache.len(), 1);
        // 文件被替换（修改时间晚于进程启动）：版本未知，不显示新版本号。
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&pkg, r#"{"version":"0.163.0"}"#).unwrap();
        let old_start = scyllaui_proto::now_ms() - 60_000;
        assert_eq!(resolve(&mut cache, &job(src.clone(), &exe.display().to_string(), Some(old_start))), None);
        // 升级后新启动的进程读到新版本。
        let later = scyllaui_proto::now_ms() + 10_000;
        assert_eq!(resolve(&mut cache, &job(src, &exe.display().to_string(), Some(later))).as_deref(), Some("0.163.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实进程复现「启动后可执行文件被替换」：读到的版本记为未知（与第 6 块的回归测试同一思路）。
    #[test]
    fn replaced_executable_gives_unknown_version() {
        let dir = test_dir("version-replaced");
        let exe = dir.join("grok-9.9.9-macos-aarch64");
        // 休眠程序见 procinfo::testbin（复制的 /bin/sleep 会被系统的启动约束杀掉）；写入而不是复制，修改时间为现在。
        crate::procinfo::testbin::install(&exe);
        let bin = std::fs::read(&exe).unwrap();
        let mut child = crate::procinfo::testbin::sleep(&exe, 5).spawn().unwrap();
        let pid = child.id() as i32;
        let start = crate::procinfo::start_ms(pid).unwrap();
        let src = VersionSource::Known { version: "9.9.9".into(), file: exe.display().to_string() };
        let mut cache = HashMap::new();
        assert_eq!(resolve(&mut cache, &job(src.clone(), &exe.display().to_string(), Some(start))).as_deref(), Some("9.9.9"));
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::remove_file(&exe).unwrap();
        std::fs::write(&exe, &bin).unwrap();
        assert_eq!(resolve(&mut cache, &job(src, &exe.display().to_string(), Some(start))), None);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_version_command_with_timeout() {
        let dir = test_dir("version-cmd");
        let ok = dir.join("kimi");
        std::fs::write(&ok, "#!/bin/sh\necho 2.2.0\n").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cache = HashMap::new();
        let p = ok.display().to_string();
        assert_eq!(resolve(&mut cache, &job(VersionSource::Command { path: p.clone() }, &p, None)).as_deref(), Some("2.2.0"));
        let slow = dir.join("slow");
        std::fs::write(&slow, "#!/bin/sh\nsleep 10\necho 1.0\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Instant::now();
        let p = slow.display().to_string();
        assert_eq!(resolve(&mut cache, &job(VersionSource::Command { path: p.clone() }, &p, None)), None);
        assert!(t.elapsed() < Duration::from_secs(8), "超时结束进程（含重试一次）");
        assert_eq!(cache.len(), 1, "超时得到的未知不缓存，下次再读");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-11-11 macOS 26 虚拟机：第一次运行 OpenCode 时同时执行的 `--version` 超时，「未知」被缓存，同一次运行里
    /// 设置一直没有版本号。第一次超时后应立即重试，读到版本。
    #[test]
    fn retries_once_after_version_command_timeout() {
        let dir = test_dir("version-retry");
        let marker = dir.join("first-run-done");
        let exe = dir.join("opencode");
        let script = format!("#!/bin/sh\nif [ -e '{m}' ]; then echo 'opencode v2.2.7'; else touch '{m}'; sleep 10; fi\n", m = marker.display());
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cache = HashMap::new();
        let p = exe.display().to_string();
        assert_eq!(resolve(&mut cache, &job(VersionSource::Command { path: p.clone() }, &p, None)).as_deref(), Some("2.2.7"));
        assert_eq!(cache.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
