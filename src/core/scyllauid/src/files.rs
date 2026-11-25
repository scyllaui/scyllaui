//! 右侧栏的文件请求（模块名避开 std::fs）（docs/design.md 5.12 节，docs/protocol.md 第 4 节 `fs.list`、`fs.read`、`fs.find`）：读一层目录、
//! 只读查看文件、按名称找文件。每个请求起一个线程，做完把应答交回核心线程（`Cmd::FsDone`），线程结束；不建常驻 worker。
//! 同时在途的上限是 8 个，超了回 `busy`。只读，不跟随符号链接（读文件除外：查看链接到的文件）。

use crate::core::Cmd;
use crate::git;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 同时在途的请求上限（docs/protocol.md 第 4 节）。
pub const MAX_INFLIGHT: usize = 8;
/// 一层最多列出的项数。
const LIST_MAX: usize = 2000;
/// 查看器最多读这么多字节。
const READ_MAX: usize = 1 << 20;
/// 前这么多字节里有 NUL 就算二进制。
const BINARY_PROBE: usize = 8 << 10;
/// 按名称找：最多遍历的项数、时长与返回的条数。
const FIND_VISIT_MAX: usize = 50_000;
const FIND_TIME: Duration = Duration::from_secs(2);
const FIND_RESULTS: usize = 200;
/// 按名称找时不进的目录（名单只在这一处）。
pub const HEAVY_DIRS: [&str; 8] = [".git", "node_modules", "vendor", "target", "output", ".tools", "DerivedData", ".venv"];

/// 请求失败：错误码与说明（日志与排查用，界面按错误码写说明）。
pub type Fail = (&'static str, String);

pub struct FsWorker {
    core: Sender<Cmd>,
    inflight: Arc<AtomicUsize>,
}

impl FsWorker {
    pub fn new(core: Sender<Cmd>) -> FsWorker {
        FsWorker { core, inflight: Arc::new(AtomicUsize::new(0)) }
    }

    /// 起一个线程做 `job`，应答发回 `conn`。在途已满时不起线程，返回 false（调用方回 `busy`）。
    pub fn spawn(&self, conn: u64, req_id: Value, what: &'static str, job: impl FnOnce() -> Result<Value, Fail> + Send + 'static) -> bool {
        if self.inflight.fetch_add(1, Ordering::AcqRel) >= MAX_INFLIGHT {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        let (core, inflight) = (self.core.clone(), self.inflight.clone());
        std::thread::spawn(move || {
            let reply = match crate::guarded(what, job) {
                Some(Ok(mut v)) => {
                    v["type"] = json!("reply");
                    v["id"] = req_id;
                    v["ok"] = json!(true);
                    v
                }
                Some(Err((code, message))) => json!({"type": "reply", "id": req_id, "ok": false, "error": {"code": code, "message": message}}),
                None => json!({"type": "reply", "id": req_id, "ok": false, "error": {"code": "internal", "message": format!("{what}出错")}}),
            };
            inflight.fetch_sub(1, Ordering::AcqRel);
            let _ = core.send(Cmd::FsDone { conn, reply });
        });
        true
    }
}

fn io_fail(e: io::Error, path: &Path) -> Fail {
    let code = match e.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "denied",
        io::ErrorKind::NotADirectory => "not_dir",
        _ => "internal",
    };
    (code, format!("{}：{e}", path.display()))
}

/// 本机有没有可用的 git（没装命令行工具时 `/usr/bin/git` 只是个提示安装的壳）；只查一次。
fn git_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(git::git_available)
}

// ---- fs.list ----

/// 读一层目录：每项 `name`、`kind`（`dir` / `file` / `link`，不跟随符号链接）、`ignored`；超过 2000 项时文件夹优先、
/// 再按名字取前 2000，另给 `more`。有 `git_root` 时用 `git check-ignore` 判定忽略，失败就全部当作没忽略。
pub fn list(dir: &Path, git_root: Option<&Path>) -> Result<Value, Fail> {
    let meta = fs::symlink_metadata(dir).map_err(|e| io_fail(e, dir))?;
    if !meta.is_dir() {
        return Err(("not_dir", format!("{} 不是目录", dir.display())));
    }
    let mut items: Vec<(String, &'static str)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_fail(e, dir))? {
        let Ok(entry) = entry else { continue };
        let kind = match entry.file_type() {
            Ok(t) if t.is_symlink() => "link",
            Ok(t) if t.is_dir() => "dir",
            _ => "file",
        };
        items.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    items.sort_by(|a, b| (a.1 != "dir").cmp(&(b.1 != "dir")).then_with(|| a.0.cmp(&b.0)));
    let more = items.len().saturating_sub(LIST_MAX);
    items.truncate(LIST_MAX);
    let ignored = git_root.filter(|_| git_ok()).map(|root| check_ignored(root, dir, &items)).unwrap_or_default();
    let out: Vec<Value> = items
        .iter()
        .map(|(name, kind)| json!({"name": name, "kind": kind, "ignored": ignored.contains(name)}))
        .collect();
    Ok(json!({"items": out, "more": more}))
}

/// 这一层里被 `.gitignore` 忽略的名字。`dir` 不在 `root` 下、在 `.git` 里，或 git 失败、超时，都返回空集合。
fn check_ignored(root: &Path, dir: &Path, items: &[(String, &'static str)]) -> HashSet<String> {
    let Ok(rel) = dir.strip_prefix(root) else { return HashSet::new() };
    if rel.components().next().is_some_and(|c| c.as_os_str() == ".git") {
        return HashSet::new();
    }
    let prefix = if rel.as_os_str().is_empty() { String::new() } else { format!("{}/", rel.to_string_lossy()) };
    let mut input = Vec::new();
    for (name, kind) in items.iter().filter(|(n, _)| n != ".git") {
        // 目录带 `/`：`vendor/` 这类只匹配目录的规则才认得出。
        input.extend_from_slice(format!("{prefix}{name}{}", if *kind == "dir" { "/" } else { "" }).as_bytes());
        input.push(0);
    }
    if input.is_empty() {
        return HashSet::new();
    }
    // 有忽略项以 0 退出，一项也没有以 1 退出；其余（不是仓库等）都当作没忽略。
    match git::run_git_ex(root, &["check-ignore", "-z", "--stdin"], &input, 4 << 20) {
        Some(o) if o.code == 0 && !o.truncated && !o.timed_out => o
            .out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                let p = String::from_utf8_lossy(p);
                p.trim_end_matches('/').rsplit('/').next().map(str::to_owned)
            })
            .collect(),
        _ => HashSet::new(),
    }
}

// ---- fs.read ----

/// 读文件给查看器：前 1 MB；前 8 KB 有 NUL 算二进制，只回 `size`。截断处退到完整的 UTF-8 字符边界，无法解码的字节换成
/// U+FFFD 并标 `lossy`。
pub fn read(path: &Path) -> Result<Value, Fail> {
    let meta = fs::metadata(path).map_err(|e| io_fail(e, path))?;
    if meta.is_dir() {
        return Err(("is_dir", format!("{} 是目录", path.display())));
    }
    let file = fs::File::open(path).map_err(|e| io_fail(e, path))?;
    let mut buf = Vec::new();
    file.take(READ_MAX as u64 + 1).read_to_end(&mut buf).map_err(|e| io_fail(e, path))?;
    let size = meta.len().max(buf.len() as u64);
    if buf[..buf.len().min(BINARY_PROBE)].contains(&0) {
        return Ok(json!({"size": size, "binary": true}));
    }
    let truncated = buf.len() > READ_MAX;
    buf.truncate(READ_MAX);
    let (text, lossy) = decode(&buf, truncated);
    Ok(json!({"size": size, "binary": false, "text": text, "truncated": truncated, "lossy": lossy}))
}

/// 按 UTF-8 解码；`cut` 为 true（读到上限截断）时先去掉末尾不完整的那个字符。
pub fn decode(bytes: &[u8], cut: bool) -> (String, bool) {
    let mut end = bytes.len();
    if cut {
        // 从末尾往回找最后一个字符的首字节（不是 10xxxxxx），它要的字节数不够就从它前面截断。
        if let Some(i) = (end.saturating_sub(4)..end).rev().find(|&i| bytes[i] & 0xC0 != 0x80) {
            let need = match bytes[i] {
                b if b < 0x80 => 1,
                b if b >= 0xF0 => 4,
                b if b >= 0xE0 => 3,
                b if b >= 0xC0 => 2,
                _ => 1,
            };
            if end - i < need {
                end = i;
            }
        }
    }
    let b = &bytes[..end];
    match std::str::from_utf8(b) {
        Ok(s) => (s.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(b).into_owned(), true),
    }
}

// ---- fs.find ----

/// 按名称找文件和文件夹：名字含 `query`（不区分大小写）；广度优先，浅的在前；不进 `HEAVY_DIRS`、不跟随符号链接。
/// 最多遍历 50000 项或 2 秒，最多 200 条，没找完标 `truncated`。
pub fn find(root: &Path, query: &str) -> Result<Value, Fail> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Err(("bad_request", "query 为空".to_owned()));
    }
    let meta = fs::symlink_metadata(root).map_err(|e| io_fail(e, root))?;
    if !meta.is_dir() {
        return Err(("not_dir", format!("{} 不是目录", root.display())));
    }
    let deadline = Instant::now() + FIND_TIME;
    let (mut queue, mut items, mut visited, mut truncated) = (VecDeque::from([root.to_path_buf()]), Vec::new(), 0usize, false);
    'outer: while let Some(dir) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > FIND_VISIT_MAX || items.len() >= FIND_RESULTS || (visited % 256 == 0 && Instant::now() > deadline) {
                truncated = true;
                break 'outer;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => "link",
                Ok(t) if t.is_dir() => "dir",
                _ => "file",
            };
            let path = entry.path();
            if name.to_lowercase().contains(&q) {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
                items.push(json!({"path": rel, "kind": kind}));
            }
            if kind == "dir" && !HEAVY_DIRS.contains(&name.as_str()) {
                queue.push_back(path);
            }
        }
    }
    Ok(json!({"items": items, "truncated": truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    fn names(v: &Value) -> Vec<String> {
        v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_owned()).collect()
    }

    /// 文件夹在前、不跟随符号链接、超过 2000 项时给 `more`；错误码。
    #[test]
    fn lists_one_level() {
        let d = test_dir("fs-list");
        fs::create_dir(d.join("zdir")).unwrap();
        fs::write(d.join("a.txt"), "a").unwrap();
        fs::write(d.join(".hidden"), "h").unwrap();
        symlink(d.join("zdir"), d.join("link")).unwrap();
        let v = list(&d, None).unwrap();
        assert_eq!(names(&v), ["zdir", ".hidden", "a.txt", "link"], "文件夹在前，其余按字节排");
        let kinds: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["dir", "file", "file", "link"], "链接到目录也标 link，不跟进");
        assert_eq!(v["more"], 0);
        assert_eq!(list(&d.join("nope"), None).unwrap_err().0, "not_found");
        assert_eq!(list(&d.join("a.txt"), None).unwrap_err().0, "not_dir");
        let big = d.join("big");
        fs::create_dir(&big).unwrap();
        for i in 0..2005 {
            fs::write(big.join(format!("f{i:04}")), "").unwrap();
        }
        fs::create_dir(big.join("zz-dir")).unwrap();
        let v = list(&big, None).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 2000);
        assert_eq!(v["more"], 6);
        assert_eq!(names(&v)[0], "zz-dir", "截断时文件夹优先保留");
        let _ = fs::remove_dir_all(&d);
    }

    /// `.gitignore` 忽略的灰显（含只匹配目录的规则）；不是仓库或 git 失败时全部当作没忽略。
    #[test]
    fn marks_ignored_entries() {
        if !git_ok() {
            return;
        }
        let d = test_dir("fs-ignore");
        assert!(Command::new("/usr/bin/git").args(["init", "-q"]).current_dir(&d).stdout(Stdio::null()).status().unwrap().success());
        fs::write(d.join(".gitignore"), "vendor/\n*.log\n").unwrap();
        fs::create_dir_all(d.join("vendor/x")).unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("build.log"), "").unwrap();
        fs::write(d.join("src/a.log"), "").unwrap();
        fs::write(d.join("src/a.rs"), "").unwrap();
        let ign = |v: &Value| -> Vec<String> {
            v["items"].as_array().unwrap().iter().filter(|i| i["ignored"] == true).map(|i| i["name"].as_str().unwrap().to_owned()).collect()
        };
        assert_eq!(ign(&list(&d, Some(&d)).unwrap()), ["vendor", "build.log"]);
        assert_eq!(ign(&list(&d.join("src"), Some(&d)).unwrap()), ["a.log"], "子目录按相对仓库根的路径判定");
        assert!(ign(&list(&d.join(".git"), Some(&d)).unwrap()).is_empty(), ".git 里不判定");
        let other = test_dir("fs-ignore-none");
        fs::write(other.join("x.log"), "").unwrap();
        assert!(ign(&list(&other, Some(&other)).unwrap()).is_empty(), "不是仓库时全部当作没忽略");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&other);
    }

    /// 二进制、1 MB 截断落在字符边界、无法解码的字节、目录与不存在。
    #[test]
    fn reads_text_binary_and_large() {
        let d = test_dir("fs-read");
        fs::write(d.join("t.txt"), "你好\n").unwrap();
        let v = read(&d.join("t.txt")).unwrap();
        assert_eq!((v["text"].as_str(), v["binary"].as_bool(), v["truncated"].as_bool(), v["lossy"].as_bool()),
                   (Some("你好\n"), Some(false), Some(false), Some(false)));
        fs::write(d.join("b.bin"), [1u8, 0, 2]).unwrap();
        assert_eq!(read(&d.join("b.bin")).unwrap()["binary"], true);
        // 1 MB 处正好切在「中」（3 字节）的中间：退到它前面。
        let mut big = vec![b'a'; READ_MAX - 1];
        big.extend_from_slice("中文".as_bytes());
        fs::write(d.join("big.txt"), &big).unwrap();
        let v = read(&d.join("big.txt")).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["lossy"], false, "截断处的半个字符不算无法解码");
        assert_eq!(v["text"].as_str().unwrap().len(), READ_MAX - 1);
        assert_eq!(v["size"], big.len() as u64);
        fs::write(d.join("gbk.txt"), [0xC4u8, 0xE3, b'\n']).unwrap();
        let v = read(&d.join("gbk.txt")).unwrap();
        assert_eq!(v["lossy"], true);
        assert!(v["text"].as_str().unwrap().contains('\u{FFFD}'));
        assert_eq!(read(&d).unwrap_err().0, "is_dir");
        assert_eq!(read(&d.join("nope")).unwrap_err().0, "not_found");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn decode_cuts_only_incomplete_tail() {
        assert_eq!(decode("ab中".as_bytes(), true), ("ab中".to_owned(), false), "完整的字符不截");
        assert_eq!(decode(&"ab中".as_bytes()[..4], true), ("ab".to_owned(), false));
        assert_eq!(decode(&"😀".as_bytes()[..3], true), (String::new(), false));
        assert_eq!(decode(&"ab中".as_bytes()[..4], false).1, true, "没截断时末尾半个字符算无法解码");
    }

    /// 按名称找：不区分大小写、浅的在前、不进大目录、不跟随链接、上限。
    #[test]
    fn finds_by_name() {
        let d = test_dir("fs-find");
        fs::create_dir_all(d.join("src/deep")).unwrap();
        fs::create_dir_all(d.join("node_modules/panel")).unwrap();
        fs::write(d.join("Panel.swift"), "").unwrap();
        fs::write(d.join("src/deep/panel.rs"), "").unwrap();
        fs::create_dir(d.join("src/PanelDir")).unwrap();
        symlink(&d, d.join("src/loop")).unwrap();
        let v = find(&d, "PANEL").unwrap();
        let paths: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["Panel.swift", "src/PanelDir", "src/deep/panel.rs"], "广度优先，不进 node_modules，链接不跟进");
        assert_eq!(v["truncated"], false);
        assert_eq!(find(&d, "  ").unwrap_err().0, "bad_request");
        for i in 0..250 {
            fs::write(d.join(format!("hit{i}")), "").unwrap();
        }
        let v = find(&d, "hit").unwrap();
        assert_eq!((v["items"].as_array().unwrap().len(), v["truncated"].as_bool()), (FIND_RESULTS, Some(true)));
        let _ = fs::remove_dir_all(&d);
    }

    /// 在途上限：满了不起线程；做完一个就能再起。
    #[test]
    fn inflight_limit() {
        let (tx, rx) = mpsc::channel();
        let w = FsWorker::new(tx);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(gate_rx));
        for i in 0..MAX_INFLIGHT {
            let g = gate.clone();
            assert!(w.spawn(1, json!(i), "测试", move || {
                let _ = g.lock().unwrap().recv();
                Ok(json!({}))
            }));
        }
        assert!(!w.spawn(1, json!(99), "测试", || Ok(json!({}))), "第 9 个回 busy");
        gate_tx.send(()).unwrap();
        let Cmd::FsDone { reply, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else { panic!() };
        assert_eq!(reply["ok"], true);
        assert!(w.spawn(1, json!(100), "测试", || Err(("not_found", "x".to_owned()))));
        for _ in 0..MAX_INFLIGHT {
            gate_tx.send(()).unwrap();
        }
        let mut codes = Vec::new();
        while let Ok(Cmd::FsDone { reply, .. }) = rx.recv_timeout(Duration::from_secs(5)) {
            codes.push(reply["error"]["code"].as_str().unwrap_or("ok").to_owned());
            if codes.len() == MAX_INFLIGHT { break; }
        }
        assert!(codes.contains(&"not_found".to_owned()));
    }
}
