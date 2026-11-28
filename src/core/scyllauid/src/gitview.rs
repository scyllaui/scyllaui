//! 右侧栏的 Diff 与内容搜索（docs/design.md 5.12 节，docs/protocol.md 第 4 节 `git.changes`、`git.diff`、`fs.grep`）：
//! HEAD 对工作树的改动列表、单个文件的 diff、`git grep` 搜内容。都在 `FsWorker` 起的线程里跑，安全规则同改动数
//! （git.rs）：没装命令行工具不调用 git，仓库配置了非 git-lfs 的过滤器就不读，每条命令 3 秒超时，git 不写 `.git`。

use crate::files::Fail;
use crate::git::{self, run_git_ex};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

/// 改动列表最多这么多项，超了给 `more`。
const CHANGES_MAX: usize = 2000;
/// 未跟踪的新文件不超过这么大才数行数。
const COUNT_MAX: u64 = 1 << 20;
/// 单个文件的 diff 最多读这么多。
const DIFF_MAX: usize = 2 << 20;
/// 内容搜索：输出上限、条数上限、每行最多字符数、每个文件最多条数。
const GREP_MAX_BYTES: usize = 1 << 20;
const GREP_MAX_ITEMS: usize = 1000;
const GREP_LINE_CHARS: usize = 300;
const GREP_PER_FILE: usize = 50;
/// 列表类命令（name-status 等）的输出上限：足够 2000 项以上，防止异常大的仓库占满内存。
const LIST_MAX_BYTES: usize = 16 << 20;
/// diff 的公共参数：不检测改名（只有 M、A、D）、不执行外部 diff 与 textconv、不进子模块。
const DIFF_ARGS: [&str; 4] = ["--no-renames", "--no-ext-diff", "--no-textconv", "--ignore-submodules=all"];

fn git_ok() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(git::git_available)
}

/// 新命令统一的前置检查：没装命令行工具 → `no_git`；要求仓库而不在仓库里 → `not_repo`；仓库配置了非 git-lfs 的过滤器 →
/// `unsafe_filter`（只读配置，不执行）。返回是否在仓库里。
fn check(dir: &Path, need_repo: bool) -> Result<bool, Fail> {
    if !git_ok() {
        return Err(("no_git", "没有可用的 git（未安装 Xcode 命令行工具）".to_owned()));
    }
    let Some((root, _)) = git::repo(dir) else {
        return if need_repo { Err(("not_repo", format!("{} 不在 Git 仓库里", dir.display()))) } else { Ok(false) };
    };
    // 没有匹配项时 git config 以 1 退出，即没有过滤器。
    if let Some(o) = run_git_ex(&root, &["config", "--get-regexp", r"^filter\..*\.(clean|smudge|process)$"], &[], 1 << 20) {
        if o.code == 0 && git::has_untrusted_filter(&o.out) {
            return Err(("unsafe_filter", format!("{} 配置了非 git-lfs 的过滤器", root.display())));
        }
    }
    Ok(true)
}

fn run(dir: &Path, args: &[&str], max: usize) -> Result<git::GitOut, Fail> {
    let o = run_git_ex(dir, args, &[], max).ok_or(("internal", "启动 git 失败".to_owned()))?;
    if o.timed_out {
        return Err(("timeout", format!("git {} 超时", args.first().unwrap_or(&""))));
    }
    Ok(o)
}

/// 比较的基准：有提交时为 HEAD，还没有提交的仓库与空树比（全部按新增）。
fn base(dir: &Path) -> &'static str {
    match run_git_ex(dir, &["rev-parse", "--verify", "-q", "HEAD"], &[], 4096) {
        Some(o) if o.code == 0 && !o.timed_out => "HEAD",
        _ => "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    }
}

// ---- git.changes ----

/// `diff --name-status -z`：状态与路径交替出现。关掉改名检测后只有 M、A、D（T、U 等其他字母按 M）。
pub fn parse_name_status(out: &[u8]) -> Vec<(char, String)> {
    let mut it = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut items = Vec::new();
    while let (Some(st), Some(path)) = (it.next(), it.next()) {
        let c = match st.first() {
            Some(b'A') => 'A',
            Some(b'D') => 'D',
            _ => 'M',
        };
        items.push((c, String::from_utf8_lossy(path).into_owned()));
    }
    items
}

/// `diff --numstat -z`：每条 `增\t删\t路径`；二进制写成 `-\t-`，为 None。
pub fn parse_numstat_items(out: &[u8]) -> HashMap<String, Option<(u64, u64)>> {
    out.split(|b| *b == 0)
        .filter_map(|rec| {
            let s = String::from_utf8_lossy(rec);
            let mut p = s.splitn(3, '\t');
            let (a, d, path) = (p.next()?, p.next()?, p.next()?);
            Some((path.to_owned(), a.parse().ok().zip(d.parse().ok())))
        })
        .collect()
}

/// 未跟踪文件的行数：不超过 1 MB 且不是二进制才数；数不了为 None（界面写「二进制」或只写新增）。
fn count_lines(path: &Path) -> (Option<u64>, bool) {
    let Ok(meta) = fs::metadata(path) else { return (None, false) };
    if meta.len() > COUNT_MAX {
        return (None, false);
    }
    let Ok(bytes) = fs::read(path) else { return (None, false) };
    if bytes[..bytes.len().min(8 << 10)].contains(&0) {
        return (None, true);
    }
    let n = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    (Some(n + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))), false)
}

/// HEAD 对工作树的改动列表（已暂存与未暂存合在一起），加上未跟踪的新文件；按路径排，最多 2000 项。
pub fn changes(root: &Path) -> Result<Value, Fail> {
    check(root, true)?;
    let base = base(root);
    let mut args = vec!["diff", base, "--name-status", "-z"];
    args.extend(DIFF_ARGS);
    let names = run(root, &args, LIST_MAX_BYTES)?;
    args[2] = "--numstat";
    let nums = run(root, &args, LIST_MAX_BYTES)?;
    let others = run(root, &["ls-files", "--others", "--exclude-standard", "-z"], LIST_MAX_BYTES)?;
    if names.code != 0 || nums.code != 0 || others.code != 0 {
        return Err(("internal", "git 读改动失败".to_owned()));
    }
    let stats = parse_numstat_items(&nums.out);
    let mut items: Vec<Value> = parse_name_status(&names.out)
        .into_iter()
        .map(|(st, path)| {
            let n = stats.get(&path).copied().flatten();
            json!({"path": path, "st": st.to_string(), "add": n.map(|x| x.0), "del": n.map(|x| x.1),
                   "binary": stats.get(&path).is_some_and(Option::is_none), "untracked": false})
        })
        .collect();
    for raw in others.out.split(|b| *b == 0).filter(|t| !t.is_empty()) {
        let path = String::from_utf8_lossy(raw).into_owned();
        let (lines, binary) = count_lines(&root.join(&path));
        items.push(json!({"path": path, "st": "A", "add": lines, "del": lines.map(|_| 0), "binary": binary, "untracked": true}));
    }
    items.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let more = items.len().saturating_sub(CHANGES_MAX);
    items.truncate(CHANGES_MAX);
    Ok(json!({"items": items, "more": more, "base": if base == "HEAD" { "HEAD" } else { "empty" },
              "truncated": names.truncated || others.truncated}))
}

// ---- git.diff ----

/// 一行 diff：符号（` `、`-`、`+`）、旧行号、新行号（没有为 null）、内容。
fn line(sign: &str, old: Option<u64>, new: Option<u64>, text: &str) -> Value {
    json!([sign, old, new, text])
}

/// 解析统一 diff：`@@ -a,b +c,d @@` 起一段，之前的文件头不要；`\ No newline at end of file` 不显示；`truncated` 时
/// 最后一行可能不完整，丢掉。返回各段与是否二进制。
pub fn parse_diff(out: &[u8], truncated: bool) -> (Vec<Value>, bool) {
    let text = String::from_utf8_lossy(out);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if truncated || lines.last() == Some(&"") {
        lines.pop();
    }
    let (mut hunks, mut binary) = (Vec::<Value>::new(), false);
    let (mut old, mut new) = (0u64, 0u64);
    for l in lines {
        if l.starts_with("Binary files ") && l.ends_with(" differ") {
            binary = true;
        } else if let Some(rest) = l.strip_prefix("@@ -") {
            let mut nums = rest.split([' ', ',']);
            old = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            new = rest.split_once(" +").and_then(|(_, r)| r.split([' ', ',']).next()).and_then(|n| n.parse().ok()).unwrap_or(0);
            hunks.push(json!({"head": l, "lines": []}));
        } else if let Some(h) = hunks.last_mut() {
            let v = match l.as_bytes().first() {
                Some(b' ') => {
                    let v = line(" ", Some(old), Some(new), &l[1..]);
                    old += 1;
                    new += 1;
                    v
                }
                Some(b'-') => {
                    let v = line("-", Some(old), None, &l[1..]);
                    old += 1;
                    v
                }
                Some(b'+') => {
                    let v = line("+", None, Some(new), &l[1..]);
                    new += 1;
                    v
                }
                _ => continue,
            };
            h["lines"].as_array_mut().unwrap().push(v);
        }
    }
    (hunks, binary)
}

/// 单个文件的改动：已跟踪的用 `git diff`（上限 2 MB），未跟踪的读文件、全部按新增。
pub fn file_diff(root: &Path, path: &str, untracked: bool) -> Result<Value, Fail> {
    check(root, true)?;
    if untracked {
        let full = root.join(path);
        let mut buf = Vec::new();
        fs::File::open(&full)
            .and_then(|f| f.take(DIFF_MAX as u64 + 1).read_to_end(&mut buf))
            .map_err(|e| (if e.kind() == std::io::ErrorKind::NotFound { "not_found" } else { "internal" }, e.to_string()))?;
        if buf[..buf.len().min(8 << 10)].contains(&0) {
            return Ok(json!({"binary": true, "truncated": false, "hunks": []}));
        }
        let truncated = buf.len() > DIFF_MAX;
        buf.truncate(DIFF_MAX);
        let (text, _) = crate::files::decode(&buf, truncated);
        let mut rows: Vec<&str> = text.split('\n').collect();
        if rows.last() == Some(&"") {
            rows.pop();
        }
        let lines: Vec<Value> = rows.iter().enumerate().map(|(i, l)| line("+", None, Some(i as u64 + 1), l)).collect();
        let head = format!("@@ -0,0 +1,{} @@", lines.len());
        let hunks = if lines.is_empty() { vec![] } else { vec![json!({"head": head, "lines": lines})] };
        return Ok(json!({"binary": false, "truncated": truncated, "hunks": hunks}));
    }
    let base = base(root);
    let mut args = vec!["diff", base, "--no-color", "-U3"];
    args.extend(DIFF_ARGS);
    args.extend(["--", path]);
    let o = run(root, &args, DIFF_MAX)?;
    let (hunks, binary) = parse_diff(&o.out, o.truncated);
    Ok(json!({"binary": binary, "truncated": o.truncated, "hunks": hunks}))
}

// ---- fs.grep ----

/// `git grep -z -n` 的输出：每行 `路径\0行号\0内容`；`truncated` 时丢掉最后一行（可能不完整）。内容超过 300 字符时围绕
/// `query` 的第一处匹配取（见 `grep_snippet`）。git 每个文件多给一条（`-m 51`，同一个文件的行连在一起）：每个文件只留前 50 条，
/// 有第 51 条的文件记进第二个返回值（`capped`）。
pub fn parse_grep(out: &[u8], truncated: bool, query: &str) -> (Vec<Value>, Vec<String>) {
    let mut rows: Vec<&[u8]> = out.split(|b| *b == b'\n').collect();
    if truncated || rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    let (mut items, mut capped, mut last, mut n) = (Vec::new(), Vec::<String>::new(), Vec::<u8>::new(), 0usize);
    for r in rows {
        if items.len() >= GREP_MAX_ITEMS {
            break;
        }
        let mut p = r.splitn(3, |b| *b == 0);
        let (Some(path), Some(no), Some(text)) = (p.next(), p.next(), p.next()) else { continue };
        let Some(no) = std::str::from_utf8(no).ok().and_then(|s| s.parse::<u64>().ok()) else { continue };
        if path != last.as_slice() {
            (last, n) = (path.to_vec(), 0);
        }
        n += 1;
        if n > GREP_PER_FILE {
            if n == GREP_PER_FILE + 1 {
                capped.push(String::from_utf8_lossy(path).into_owned());
            }
            continue;
        }
        let text = grep_snippet(&String::from_utf8_lossy(text), query);
        items.push(json!({"path": String::from_utf8_lossy(path), "line": no, "text": text}));
    }
    (items, capped)
}

/// 命中行超过 `GREP_LINE_CHARS` 个字符时，围绕第一处匹配取这么多：匹配前留 60 个，前后被省掉处加 `…`（2026-11-25 起；
/// 原来只取行首，匹配在后面时 App 拿不到、显示不出）。匹配同 `git grep -F -i`：按字面、不分大小写（逐字符比小写）。
/// 找不到（大小写规则与 git 不同的少数字符）时取行首。
fn grep_snippet(line: &str, query: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= GREP_LINE_CHARS {
        return line.to_owned();
    }
    let low = |c: &char| c.to_lowercase().next().unwrap_or(*c);
    let q: Vec<char> = query.chars().map(|c| low(&c)).collect();
    let hit = (!q.is_empty())
        .then(|| (0..=chars.len().saturating_sub(q.len())).find(|&i| chars[i..i + q.len()].iter().map(low).eq(q.iter().copied())))
        .flatten();
    let start = hit.map_or(0, |i| i.saturating_sub(60)).min(chars.len() - GREP_LINE_CHARS);
    let end = start + GREP_LINE_CHARS;
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(&chars[start..end]);
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// 搜文件内容（不区分大小写、按字面）：仓库里搜已跟踪与未跟踪的（遵守 `.gitignore`），不在仓库里用 `--no-index`。跳过二进制，
/// 每个文件最多 50 条（超过的文件列在 `capped`），总共最多 1000 条、1 MB 输出；超时时返回已找到的并标 `truncated`。
pub fn grep(root: &Path, query: &str) -> Result<Value, Fail> {
    if query.trim().is_empty() {
        return Err(("bad_request", "query 为空".to_owned()));
    }
    let in_repo = check(root, false)?;
    let mut args = vec!["grep"];
    if !in_repo {
        args.extend(["--no-index", "--exclude-standard"]);
    }
    args.extend(["-n", "-I", "-F", "-i"]);
    if in_repo {
        args.push("--untracked");
    }
    let per_file = (GREP_PER_FILE + 1).to_string();
    args.extend(["-m", &per_file, "-z", "-e", query]);
    let o = run_git_ex(root, &args, &[], GREP_MAX_BYTES).ok_or(("internal", "启动 git 失败".to_owned()))?;
    // 找到为 0，一条也没有为 1；超时与读到上限时退出码不可信，按已读到的算。
    if !o.timed_out && !o.truncated && o.code != 0 && o.code != 1 {
        return Err(("internal", format!("git grep 以 {} 退出", o.code)));
    }
    let cut = o.truncated || o.timed_out;
    let (items, capped) = parse_grep(&o.out, cut, query);
    let truncated = cut || items.len() >= GREP_MAX_ITEMS;
    Ok(json!({"items": items, "truncated": truncated, "capped": capped}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use std::process::{Command, Stdio};

    fn git(d: &Path, args: &[&str]) {
        assert!(Command::new("/usr/bin/git").args(args).current_dir(d).stdout(Stdio::null()).stderr(Stdio::null())
            .status().unwrap().success(), "{args:?}");
    }

    fn commit(d: &Path) {
        git(d, &["add", "-A"]);
        git(d, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "x"]);
    }

    fn by_path(v: &Value) -> HashMap<String, Value> {
        v["items"].as_array().unwrap().iter().map(|i| (i["path"].as_str().unwrap().to_owned(), i.clone())).collect()
    }

    #[test]
    fn parses_outputs() {
        assert_eq!(parse_name_status(b"M\0a.txt\0A\0b\0D\0c\0T\0d\0"), [('M', "a.txt".into()), ('A', "b".into()), ('D', "c".into()), ('M', "d".into())]);
        let n = parse_numstat_items(b"2\t1\ta.txt\0-\t-\tpic.png\0");
        assert_eq!(n["a.txt"], Some((2, 1)));
        assert_eq!(n["pic.png"], None, "二进制");
        let (h, bin) = parse_diff(b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@ fn x\n ctx\n-old\n+new\n\\ No newline at end of file\n", false);
        assert!(!bin);
        assert_eq!(h[0]["head"], "@@ -1,2 +1,2 @@ fn x");
        assert_eq!(h[0]["lines"], json!([[" ", 1, 1, "ctx"], ["-", 2, null, "old"], ["+", null, 2, "new"]]));
        assert!(parse_diff(b"Binary files a/x and b/x differ\n", false).1);
        let (h, _) = parse_diff(b"@@ -5 +5,2 @@\n a\n+b\n+partial", true);
        assert_eq!(h[0]["lines"].as_array().unwrap().len(), 2, "截断时最后一行不要");
        assert_eq!(h[0]["lines"][0], json!([" ", 5, 5, "a"]), "`-5` 不带行数也认");
        let (g, _) = parse_grep(b"a.txt\x001\x00Hello\nsub/b.md\x0012\x00x\x00y\n", false, "x");
        assert_eq!(g, vec![json!({"path": "a.txt", "line": 1, "text": "Hello"}), json!({"path": "sub/b.md", "line": 12, "text": "x\u{0}y"})]);
        // 长行：围绕第一处匹配（不分大小写）取 300 字符，匹配前留 60 个，前后被省掉的加「…」；匹配在开头附近时从行首取、只有后面加「…」。
        let long = format!("{}P95 23.1 ms{}", "前".repeat(500), "后".repeat(500));
        let t = grep_snippet(&long, "p95 23.1");
        assert!(t.starts_with('…') && t.ends_with('…'), "{t}");
        assert_eq!(t.chars().count(), GREP_LINE_CHARS + 2);
        assert_eq!(t.find("P95 23.1").map(|b| t[..b].chars().count()), Some(61), "匹配前留 60 个（加开头的「…」）");
        let head = format!("ab NEEDLE {}", "x".repeat(400));
        let t = grep_snippet(&head, "needle");
        assert!(t.starts_with("ab NEEDLE") && t.ends_with('…'));
        let tail = format!("{}needle", "y".repeat(400));
        let t = grep_snippet(&tail, "NEEDLE");
        assert!(t.starts_with('…') && t.ends_with("needle") && t.chars().count() == GREP_LINE_CHARS + 1, "匹配在行尾：取最后 300 个");
        assert_eq!(grep_snippet("短行 needle", "needle"), "短行 needle", "不超过 300 字符原样给");
        assert_eq!(grep_snippet(&"z".repeat(400), "没有").chars().count(), GREP_LINE_CHARS + 1, "找不到时取行首");
    }

    /// 真实仓库（同 git.rs 的写法）：M、A、D、未跟踪、二进制、没有提交、改名显示为 D + A、非 LFS 过滤器拒读、diff 与搜内容。
    #[test]
    fn changes_diff_and_grep_of_a_real_repo() {
        if !git_ok() {
            return;
        }
        let d = test_dir("gitview");
        git(&d, &["init", "-q"]);
        fs::write(d.join("a.txt"), "1\n2\n").unwrap();
        let v = changes(&d).unwrap();
        assert_eq!(v["base"], "empty");
        assert_eq!(by_path(&v)["a.txt"]["untracked"], true, "还没有提交：未跟踪的也列出");
        git(&d, &["add", "a.txt"]);
        let v = changes(&d).unwrap();
        assert_eq!((by_path(&v)["a.txt"]["st"].clone(), by_path(&v)["a.txt"]["add"].clone()), (json!("A"), json!(2)), "和空树比全部按新增");
        fs::write(d.join("gone.txt"), "g\n").unwrap();
        fs::write(d.join("old-name.txt"), "r\nr\nr\n").unwrap();
        fs::write(d.join("pic.bin"), [0u8, 1, 2]).unwrap();
        commit(&d);
        assert!(changes(&d).unwrap()["items"].as_array().unwrap().is_empty(), "干净");
        fs::write(d.join("a.txt"), "1\nzwei\n3\n").unwrap();
        fs::remove_file(d.join("gone.txt")).unwrap();
        fs::rename(d.join("old-name.txt"), d.join("new-name.txt")).unwrap();
        git(&d, &["add", "-A", "new-name.txt", "old-name.txt"]);
        fs::write(d.join("pic.bin"), [0u8, 9, 9, 9]).unwrap();
        fs::write(d.join("fresh.md"), "x\ny").unwrap();
        fs::write(d.join(".gitignore"), "*.log\n").unwrap();
        fs::write(d.join("skip.log"), "ignored\n").unwrap();
        let v = changes(&d).unwrap();
        let m = by_path(&v);
        assert_eq!((m["a.txt"]["st"].clone(), m["a.txt"]["add"].clone(), m["a.txt"]["del"].clone()), (json!("M"), json!(2), json!(1)));
        assert_eq!(m["gone.txt"]["st"], "D");
        assert_eq!((m["old-name.txt"]["st"].clone(), m["new-name.txt"]["st"].clone()), (json!("D"), json!("A")), "改名显示为删除 + 新增");
        assert_eq!(m["pic.bin"]["binary"], true);
        assert_eq!((m["fresh.md"]["untracked"].clone(), m["fresh.md"]["add"].clone()), (json!(true), json!(2)), "未跟踪的数行数，末行没换行也算");
        assert!(!m.contains_key("skip.log"), "忽略的不列");
        let diff = file_diff(&d, "a.txt", false).unwrap();
        assert_eq!(diff["hunks"][0]["lines"], json!([[" ", 1, 1, "1"], ["-", 2, null, "2"], ["+", null, 2, "zwei"], ["+", null, 3, "3"]]));
        assert_eq!(file_diff(&d, "pic.bin", false).unwrap()["binary"], true);
        let fresh = file_diff(&d, "fresh.md", true).unwrap();
        assert_eq!(fresh["hunks"][0]["lines"], json!([["+", null, 1, "x"], ["+", null, 2, "y"]]));
        let g = grep(&d, "ZWEI").unwrap();
        assert_eq!(g["items"], json!([{"path": "a.txt", "line": 2, "text": "zwei"}]), "不区分大小写");
        assert_eq!(grep(&d, "ignored").unwrap()["items"], json!([]), "遵守 .gitignore");
        assert_eq!(grep(&d, "y").unwrap()["items"].as_array().unwrap().iter().filter(|i| i["path"] == "fresh.md").count(), 1, "未跟踪的也搜");
        // 仓库配置里的 clean 过滤器会被 diff 执行：配置了非 git-lfs 的过滤器时不读，过滤器没有被执行。
        let marker = d.join("FILTER_RAN");
        fs::write(d.join(".gitattributes"), "*.txt filter=probe\n").unwrap();
        git(&d, &["config", "filter.probe.clean", &format!("sh -c 'touch {}; cat'", marker.display())]);
        assert_eq!(changes(&d).unwrap_err().0, "unsafe_filter");
        assert_eq!(file_diff(&d, "a.txt", false).unwrap_err().0, "unsafe_filter");
        assert!(!marker.exists(), "过滤器不应被执行");
        let plain = test_dir("gitview-plain");
        fs::write(plain.join("n.txt"), "needle\n").unwrap();
        assert_eq!(changes(&plain).unwrap_err().0, "not_repo");
        assert_eq!(grep(&plain, "NEEDLE").unwrap()["items"], json!([{"path": "n.txt", "line": 1, "text": "needle"}]), "不在仓库里也能搜");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&plain);
    }

    /// 补测问题 1（2026-11-28）：某个文件命中超过 50 条，只给前 50 条，并在 `capped` 里写明是哪个文件；正好 50 条的不算。
    #[test]
    fn grep_marks_files_over_per_file_limit() {
        if !git_ok() {
            return;
        }
        let d = test_dir("gitview-capped");
        let hits = |n: usize| (1..=n).map(|i| format!("zebra_marker {i}\n")).collect::<String>();
        fs::write(d.join("many.txt"), hits(120)).unwrap();
        fs::write(d.join("fifty.txt"), hits(50)).unwrap();
        fs::write(d.join("few.txt"), hits(3)).unwrap();
        let g = grep(&d, "zebra_marker").unwrap();
        let count = |p: &str| g["items"].as_array().unwrap().iter().filter(|i| i["path"] == p).count();
        assert_eq!((count("many.txt"), count("fifty.txt"), count("few.txt")), (50, 50, 3), "每个文件最多 50 条");
        assert_eq!(g["capped"], json!(["many.txt"]), "超过 50 条的文件写明");
        assert_eq!(g["truncated"], false, "总数没到上限");
        let _ = fs::remove_dir_all(&d);
    }

    /// 带上限的运行函数：读到上限就停，标 `truncated`。
    #[test]
    fn capped_run_truncates() {
        if !git_ok() {
            return;
        }
        let d = test_dir("gitview-cap");
        git(&d, &["init", "-q"]);
        fs::write(d.join("big.txt"), "line\n".repeat(200_000)).unwrap();
        let o = run_git_ex(&d, &["grep", "--untracked", "-n", "-e", "line"], &[], 1000).unwrap();
        assert!(o.truncated && !o.timed_out);
        assert_eq!(o.out.len(), 1000);
        let _ = fs::remove_dir_all(&d);
    }
}
