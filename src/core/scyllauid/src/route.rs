//! CLI 守护进程托管的会话的 Hook 归属（docs/design.md 4.4 节，2026-10-12 Claude Code 2.1.296 取证）。
//! 这类会话继承的是拉起守护进程那个终端的环境，`SCYLLAUI_PANE_ID` 不可信；改为反查哪个终端的前台进程组
//! 正连着它（进程参数与 CLI 的进程登记文件）。读进程参数与登记文件在 `RouteWorker` 线程里做，核心线程只收结果。
//! 刚转入后台时登记文件可能晚于 SessionStart 写好，找不到时隔 0.5 秒、2 秒各重试一次，仍找不到即不归属。

use crate::core::Cmd;
use scyllaui_proto::adapters::adapter;
use scyllaui_proto::Tool;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// 一份等待归属的 Hook 帧。
pub struct RouteJob {
    pub frame: Value,
    pub recv_us: u64,
    pub tool: Tool,
    /// 连接方用来指称该会话的任务 ID。
    pub job: String,
    /// 前台识别为同一款 CLI 的存活终端：（终端 ID，前台进程组号）。
    pub candidates: Vec<(String, i32)>,
}

/// 找不到时的重试间隔（第一次立即找）；只对适配器标明需要重试的工具（Claude）生效。
const RETRIES: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];

pub struct RouteWorker {
    tx: Sender<RouteJob>,
}

impl RouteWorker {
    pub fn new(core: Sender<Cmd>) -> RouteWorker {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        RouteWorker::with_resolver(core, move |tool, pgid| attached_job(&home, tool, pgid))
    }

    /// 测试用：注入「某个前台进程组连着哪个任务」的查询。
    pub fn with_resolver(
        core: Sender<Cmd>,
        mut attached: impl FnMut(Tool, i32) -> Option<String> + Send + 'static,
    ) -> RouteWorker {
        let (tx, rx) = mpsc::channel::<RouteJob>();
        std::thread::spawn(move || {
            // （到期时间，第几次重试，任务）
            let mut pending: Vec<(Instant, usize, RouteJob)> = Vec::new();
            loop {
                let next = pending.iter().map(|(t, _, _)| *t).min();
                let got = match next {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
                };
                match got {
                    Ok(job) => pending.push((Instant::now(), 0, job)),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let now = Instant::now();
                let (due, rest): (Vec<_>, Vec<_>) =
                    pending.drain(..).partition(|(t, _, _)| *t <= now);
                pending = rest;
                for (_, tries, job) in due {
                    // 出错时按找不到终端处理，Hook 帧照常交回核心线程。
                    let pane = crate::guarded("查守护进程会话所在终端", || {
                        job.candidates
                            .iter()
                            .find(|(_, pgid)| attached(job.tool, *pgid).as_deref() == Some(&job.job))
                            .map(|(id, _)| id.clone())
                    })
                    .flatten();
                    let retry = adapter(job.tool).hosted.as_ref().is_some_and(|h| h.retry);
                    if pane.is_none() && retry && tries < RETRIES.len() {
                        pending.push((Instant::now() + RETRIES[tries], tries + 1, job));
                        continue;
                    }
                    let cmd = Cmd::HookRouted {
                        frame: job.frame,
                        recv_us: job.recv_us,
                        pane,
                    };
                    if core.send(cmd).is_err() {
                        return;
                    }
                }
            }
        });
        RouteWorker { tx }
    }

    pub fn request(&self, job: RouteJob) {
        let _ = self.tx.send(job);
    }
}

/// 一键恢复用到的探测（docs/protocol.md 第 7 节「一键恢复」）。只读进程参数、CLI 的进程登记文件（几个小文件）
/// 与文件是否存在，都不在受保护目录下，在核心线程里同步做；测试里换成假的，不读用户主目录。
pub struct Probe {
    /// 前台进程组连着哪个后台任务（与归属同一规则）。
    pub attached: Box<dyn Fn(Tool, i32) -> Option<String> + Send>,
    /// 这条会话是不是后台任务（承载它的进程还活着，或停止了但任务记录还在）：参数为任务 ID 与会话 ID。
    pub is_job: Box<dyn Fn(Tool, &str, &str) -> bool + Send>,
    pub exists: Box<dyn Fn(&str) -> bool + Send>,
}

impl Probe {
    pub fn system() -> Probe {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let h = home.clone();
        Probe {
            attached: Box::new(move |tool, pgid| attached_job(&home, tool, pgid)),
            is_job: Box::new(move |tool, job, session| job_alive(&h, tool, job) || job_recorded(&h, tool, job, session)),
            exists: Box::new(|path| Path::new(path).exists()),
        }
    }
}

/// 登记目录里有进程承载着这个后台任务、且该进程还在（Claude：`~/.claude/sessions/<pid>.json` 的 `jobId`；
/// 2026-10-13 取证见 docs/s0-hook-compat.md「恢复命令与失败」）。读不到即按不在处理。
fn job_alive(home: &Path, tool: Tool, job: &str) -> bool {
    let Some(hosted) = adapter(tool).hosted.as_ref() else {
        return false;
    };
    let Some(Ok(entries)) = hosted.registry_dir.map(|d| std::fs::read_dir(home.join(d))) else {
        return false;
    };
    entries.flatten().any(|e| {
        let pid = e.file_name().to_str().and_then(|n| n.strip_suffix(".json")?.parse::<i32>().ok());
        let Some(pid) = pid.filter(|p| *p > 0) else {
            return false;
        };
        let registry: Option<Value> = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        registry.as_ref().and_then(hosted.hosts_job).as_deref() == Some(job)
            && crate::procinfo::process_exists(pid)
    })
}

/// 任务记录目录里有这个任务，且记录的会话 ID 就是这条会话（停止了的后台任务也算；2026-10-15 实测）。
fn job_recorded(home: &Path, tool: Tool, job: &str, session: &str) -> bool {
    let Some(hosted) = adapter(tool).hosted.as_ref() else {
        return false;
    };
    let Some(dir) = hosted.jobs_dir else {
        return false;
    };
    // 任务 ID 直接拼进路径：只允许会话 ID 的字符，不能带路径分隔符。
    if !scyllaui_proto::adapters::valid_session_id(job) || job.contains('.') {
        return false;
    }
    std::fs::read(home.join(dir).join(job).join("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| (hosted.job_session)(&v))
        .as_deref()
        == Some(session)
}

/// 前台进程组里任一进程连着的任务：按适配器的规则看进程参数与登记文件 `<家目录>/<registry_dir>/<pid>.json`。
fn attached_job(home: &Path, tool: Tool, pgid: i32) -> Option<String> {
    let hosted = adapter(tool).hosted.as_ref()?;
    crate::procinfo::group_args(pgid)
        .into_iter()
        .find_map(|(pid, args)| {
            let registry: Option<Value> = hosted.registry_dir.and_then(|dir| {
                std::fs::read(home.join(dir).join(format!("{pid}.json")))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
            });
            (hosted.attached)(&args, registry.as_ref())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn job(candidates: Vec<(String, i32)>) -> RouteJob {
        RouteJob {
            frame: json!({}),
            recv_us: 0,
            tool: Tool::Claude,
            job: "ee4afd71".into(),
            candidates,
        }
    }

    /// 登记文件晚于 SessionStart 写好：第一次找不到，重试时找到。
    #[test]
    fn retries_until_the_terminal_shows_the_job() {
        let (tx, rx) = mpsc::channel();
        let ready = Arc::new(Mutex::new(false));
        let r = ready.clone();
        let w = RouteWorker::with_resolver(tx, move |_, pgid| {
            (pgid == 2 && *r.lock().unwrap()).then(|| "ee4afd71".to_owned())
        });
        let start = Instant::now();
        w.request(job(vec![("t1".into(), 1), ("t2".into(), 2)]));
        std::thread::sleep(Duration::from_millis(100));
        *ready.lock().unwrap() = true;
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Cmd::HookRouted { pane, .. }) => assert_eq!(pane.as_deref(), Some("t2")),
            _ => panic!("应归到 t2"),
        }
        assert!(
            start.elapsed() >= Duration::from_millis(500),
            "第一次没找到，0.5 秒后重试"
        );
    }

    /// 没有终端连着（空闲的备用会话）：重试两次后不归属。
    #[test]
    fn gives_up_when_no_terminal_shows_the_job() {
        let (tx, rx) = mpsc::channel();
        let w = RouteWorker::with_resolver(tx, |_, _| Some("05da4a74".to_owned()));
        let start = Instant::now();
        w.request(job(vec![("t1".into(), 1)]));
        match rx.recv_timeout(Duration::from_secs(4)) {
            Ok(Cmd::HookRouted { pane, .. }) => assert_eq!(pane, None),
            _ => panic!("应送回不归属的结果"),
        }
        assert!(
            start.elapsed() >= Duration::from_millis(2500),
            "重试两次后才放弃"
        );
    }
    /// 后台任务是否还活着：登记文件的 jobId 对得上且进程存在；备用会话、已退出的进程、没有登记目录的工具都不算。
    #[test]
    fn live_job_needs_registry_entry_and_running_process() {
        let home = crate::store::test_dir("route-live-job");
        let reg = home.join(".claude/sessions");
        std::fs::create_dir_all(&reg).unwrap();
        let me = std::process::id();
        std::fs::write(reg.join(format!("{me}.json")), r#"{"sessionId":"ee4afd71-c942","jobId":"ee4afd71"}"#).unwrap();
        let mut gone = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        std::fs::write(reg.join(format!("{dead}.json")), r#"{"jobId":"58ef57b6"}"#).unwrap();
        std::fs::write(reg.join("1.json"), r#"{"jobId":"05da4a74","spare":true}"#).unwrap();
        assert!(job_alive(&home, Tool::Claude, "ee4afd71"));
        assert!(!job_alive(&home, Tool::Claude, "58ef57b6"), "进程已退出");
        assert!(!job_alive(&home, Tool::Claude, "05da4a74"), "备用会话");
        assert!(!job_alive(&home, Tool::Claude, "11375847"), "没有登记");
        assert!(!job_alive(&home, Tool::Codex, "ee4afd71"), "Codex 没有登记文件");
        // 停止了的后台任务：没有存活进程，但任务记录还在且会话 ID 对得上。
        let jobs = home.join(".claude/jobs/af9864ad");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(jobs.join("state.json"), r#"{"state":"stopped","sessionId":"af9864ad-fd8e-4a8b"}"#).unwrap();
        assert!(job_recorded(&home, Tool::Claude, "af9864ad", "af9864ad-fd8e-4a8b"));
        assert!(!job_recorded(&home, Tool::Claude, "af9864ad", "af9864ad-other"), "会话 ID 对不上");
        assert!(!job_recorded(&home, Tool::Claude, "..", "x"), "任务 ID 不能跳出目录");
        assert!(!job_recorded(&home, Tool::Codex, "af9864ad", "af9864ad-fd8e-4a8b"), "Codex 没有任务记录");
        let _ = std::fs::remove_dir_all(&home);
    }
}
