//! 换版本接管（M3，docs/design.md 风险 15，docs/protocol.md 第 1 节「接管」）：旧进程把每个会话的主端描述符号、
//! Shell 进程号、尺寸与 VT 快照写进数据目录下 0600 的接管文件，只给主端与单实例锁去掉 CLOEXEC，然后 execv 新版本；
//! 新进程读完即删，沿用这些描述符。进程号不变，Shell 仍是子进程。屏幕状态用 VT 快照（文本序列）传，
//! 不用 libghostty 的二进制快照：升级 Ghostty 后仍能读。格式 `format` 只增不改。

use serde_json::{json, Value};
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

pub const FORMAT: u64 = 1;

pub struct Entry {
    pub terminal_id: String,
    pub master_fd: RawFd,
    pub pid: i32,
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
    pub snapshot: Vec<u8>,
    pub at_prompt: bool,
    /// 核心线程里只在内存中的终端状态，随接管带到新进程（2026-10-23 起；可选字段，格式号不变：旧文件没有即为空，
    /// 旧版本读新文件时忽略）。`notice` 为终端提示（`cwd_denied`），`current_dir` 为 Shell 最近上报的当前目录。
    pub notice: Option<String>,
    pub current_dir: Option<String>,
    /// 这个终端的 Agent 状态（2026-11-14 起；可选字段，格式号不变）：内容由核心线程填写与解读
    /// （`Core::upgrade`、`Core::adopt`），这里只原样带过去。旧文件没有即为 None，状态回到「未知」。
    pub agent: Option<Value>,
}

/// 接管后交给核心还原的会话信息。
pub struct Adopted {
    pub terminal_id: String,
    pub notice: Option<String>,
    pub current_dir: Option<String>,
    pub agent: Option<Value>,
}

pub struct Handoff {
    pub lock_fd: RawFd,
    pub entries: Vec<Entry>,
    pub appearance: super::Appearance,
    /// 监听 socket 的描述符（2026-11-14 起，可选）：新进程沿用，exec 期间连进来的连接在内核里排队、不丢。
    /// 旧文件没有即为 None，新进程重新绑定。
    pub listen_fd: Option<RawFd>,
}

pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("takeover.json")
}

fn rgb_json(c: Option<[u8; 3]>) -> Value {
    c.map_or(Value::Null, |c| json!(c))
}

fn rgb_of(v: &Value) -> Option<[u8; 3]> {
    let a = v.as_array()?;
    let n = |i: usize| a.get(i)?.as_u64().filter(|&x| x <= 255).map(|x| x as u8);
    Some([n(0)?, n(1)?, n(2)?])
}

/// 写接管文件（先写临时文件再改名，权限 0600）。
pub fn write(path: &Path, h: &Handoff) -> io::Result<()> {
    let sessions: Vec<Value> = h
        .entries
        .iter()
        .map(|e| {
            json!({
                "terminal_id": e.terminal_id, "master_fd": e.master_fd, "pid": e.pid,
                "cols": e.cols, "rows": e.rows, "cell_w": e.cell_w, "cell_h": e.cell_h,
                "snapshot": base64(&e.snapshot), "at_prompt": e.at_prompt,
                "notice": e.notice, "current_dir": e.current_dir, "agent": e.agent,
            })
        })
        .collect();
    let a = &h.appearance;
    let doc = json!({
        "format": FORMAT, "lock_fd": h.lock_fd, "listen_fd": h.listen_fd, "sessions": sessions,
        "appearance": {"color_scheme": a.color_scheme, "foreground": rgb_json(a.foreground), "background": rgb_json(a.background)},
    });
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(doc.to_string().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 读接管文件并删除它。格式不认识时返回错误（文件同样删除）。
pub fn read(path: &Path) -> Result<Handoff, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读接管文件失败：{e}"));
    let _ = std::fs::remove_file(path);
    let v: Value = serde_json::from_str(&text?).map_err(|e| format!("接管文件不是 JSON：{e}"))?;
    if v["format"] != FORMAT {
        return Err(format!("不认识的接管文件格式 {}", v["format"]));
    }
    let lock_fd = v["lock_fd"].as_i64().ok_or("缺少 lock_fd")? as RawFd;
    let mut entries = Vec::new();
    for s in v["sessions"].as_array().ok_or("缺少 sessions")? {
        let n = |k: &str| s[k].as_u64().ok_or(format!("会话缺少 {k}"));
        entries.push(Entry {
            terminal_id: s["terminal_id"].as_str().ok_or("会话缺少 terminal_id")?.to_owned(),
            master_fd: n("master_fd")? as RawFd,
            pid: n("pid")? as i32,
            cols: n("cols")? as u16,
            rows: n("rows")? as u16,
            cell_w: n("cell_w")? as u32,
            cell_h: n("cell_h")? as u32,
            snapshot: unbase64(s["snapshot"].as_str().unwrap_or("")).ok_or("快照不是 base64")?,
            at_prompt: s["at_prompt"].as_bool().unwrap_or(false),
            notice: s["notice"].as_str().map(str::to_owned),
            current_dir: s["current_dir"].as_str().map(str::to_owned),
            agent: s.get("agent").filter(|a| a.is_object()).cloned(),
        });
    }
    let a = &v["appearance"];
    let appearance = super::Appearance {
        color_scheme: a["color_scheme"].as_u64().unwrap_or(0) as u8,
        foreground: rgb_of(&a["foreground"]),
        background: rgb_of(&a["background"]),
    };
    let listen_fd = v["listen_fd"].as_i64().filter(|fd| *fd > 2).map(|fd| fd as RawFd);
    Ok(Handoff { lock_fd, entries, appearance, listen_fd })
}

/// exec 之前：`keep` 中的描述符去掉 CLOEXEC，其余打开的一律设上（socket、数据库、kqueue、管道等不带进新进程）。
pub fn prepare_exec(keep: &[RawFd]) {
    let fds: Vec<RawFd> = std::fs::read_dir("/dev/fd")
        .map(|d| d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect())
        .unwrap_or_default();
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        set_cloexec(fd, !keep.contains(&fd));
    }
}

pub fn set_cloexec(fd: RawFd, on: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
            libc::fcntl(fd, libc::F_SETFD, flags);
        }
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for q in b.chunks(4) {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for s in [&b""[..], b"a", b"ab", b"abc", b"\x1b[31m\xe4\xb8\xad\x00\xff"] {
            assert_eq!(unbase64(&base64(s)).unwrap(), s);
        }
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(unbase64("TQ==").unwrap(), b"M");
        assert!(unbase64("T*==").is_none());
    }

    #[test]
    fn handoff_file_roundtrip_and_is_removed() {
        let dir = std::env::temp_dir().join(format!("pw-takeover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = file_path(&dir);
        let h = Handoff {
            lock_fd: 7,
            entries: vec![Entry {
                terminal_id: "t".into(), master_fd: 9, pid: 123, cols: 80, rows: 24, cell_w: 8, cell_h: 16,
                snapshot: b"\x1bc hi".to_vec(), at_prompt: true,
                notice: Some("cwd_denied".into()), current_dir: Some("/w/当前".into()),
                agent: Some(json!({"state": {"tool": "kimi", "state": "waiting"}, "fg_pid": 42})),
            }],
            appearance: super::super::Appearance { color_scheme: 2, foreground: Some([1, 2, 3]), background: None },
            listen_fd: Some(11),
        };
        write(&path, &h).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let r = read(&path).unwrap();
        assert!(!path.exists(), "读完应删除");
        assert_eq!((r.lock_fd, r.entries.len(), r.entries[0].pid, r.entries[0].master_fd), (7, 1, 123, 9));
        assert_eq!(r.entries[0].snapshot, b"\x1bc hi");
        assert_eq!((r.entries[0].notice.as_deref(), r.entries[0].current_dir.as_deref()), (Some("cwd_denied"), Some("/w/当前")));
        assert_eq!(r.listen_fd, Some(11));
        assert_eq!(r.entries[0].agent.as_ref().unwrap()["fg_pid"], 42);
        // 旧版本写的文件没有这两个字段：读作空。
        let old = r#"{"format":1,"lock_fd":7,"sessions":[{"terminal_id":"t","master_fd":9,"pid":1,"cols":80,"rows":24,"cell_w":8,"cell_h":16,"snapshot":""}],"appearance":{}}"#;
        std::fs::write(&path, old).unwrap();
        let o = read(&path).unwrap();
        assert_eq!((o.entries[0].notice.clone(), o.entries[0].current_dir.clone()), (None, None));
        assert!(o.listen_fd.is_none() && o.entries[0].agent.is_none(), "旧文件没有 Agent 状态与监听 socket");
        assert_eq!((r.appearance.color_scheme, r.appearance.foreground, r.appearance.background), (2, Some([1, 2, 3]), None));
        std::fs::write(&path, r#"{"format":99}"#).unwrap();
        assert!(read(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
