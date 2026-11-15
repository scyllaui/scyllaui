//! Claude 的 statusline 桥（M2 第 3 块，docs/design.md 第 5 节「实现（M2 第 3 块）」）：把用户原来的
//! `statusLine.command` 包起来，在 ScyllaUI 终端里先经 `scyllaui statusline` 再交给原命令，其他终端直接运行原命令。
//! 只有生成改动的纯函数；写入、备份、冲突检查与 Hook 配置共用 `hookcfg::apply`。

use crate::hookcfg::{FileChange, Plan};
use crate::Tool;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 包装的标记（首行开头）；模板改动时升版本号，旧版本仍按旧模板撤销。
const MARK: &str = ": scyllaui-statusline-bridge-v1";
const HEAD: &str = "; if [ -n \"${SCYLLAUI_SOCK:-}\" ] && [ -x \"${SCYLLAUI_BIN:-}\" ]; then \"${SCYLLAUI_BIN}\" statusline | {\n";
const MID: &str = "\n}; else {\n";
const TAIL: &str = "\n}; fi";
/// 原来没有 statusline 时包装里的原命令：不输出。
const NONE: &str = ":";

/// 包装原命令：原命令原样出现两次，不转义（各自单独成行，末尾的注释也不会吞掉后面的括号）。
pub fn wrap(orig: &str) -> String {
    format!("{MARK}{HEAD}{orig}{MID}{orig}{TAIL}")
}

/// 从包装取回原命令：不是本项目的包装为 `Ok(None)`；带标记但与模板不符（被改过、两处原命令不一致）为 `Err`。
pub fn unwrap(cmd: &str) -> Result<Option<String>, String> {
    if !cmd.starts_with(MARK) {
        return Ok(None);
    }
    let fixed = MARK.len() + HEAD.len() + MID.len() + TAIL.len();
    let n = cmd.len().checked_sub(fixed).filter(|n| n % 2 == 0).map(|n| n / 2);
    let start = MARK.len() + HEAD.len();
    let orig = n.and_then(|n| cmd.get(start..start + n));
    match orig {
        Some(o) if wrap(o) == cmd => Ok(Some(o.to_owned())),
        _ => Err("statusLine.command 带有 ScyllaUI 标记，但与包装模板不符（被改过，或两处原命令不一致），不自动撤销".into()),
    }
}

/// 当前状态。
#[derive(Debug, PartialEq)]
pub enum State {
    /// 已装桥；`orig` 为原命令（`:` 表示原来没有 statusline）。
    Installed { orig: String },
    /// 有用户自己的 statusline，未装桥。
    Plain { command: String },
    /// 没有配置 statusline。
    None,
}

pub struct Bridge {
    pub path: PathBuf,
    pub text: Option<String>,
}

impl Bridge {
    pub fn read(home: &Path) -> Result<Bridge, String> {
        let path = home.join(".claude/settings.json");
        let text = match std::fs::read(&path) {
            Ok(b) => Some(String::from_utf8(b).map_err(|_| format!("{}：不是 UTF-8 文本", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}：{e}", path.display())),
        };
        Ok(Bridge { path, text })
    }

    fn data(&self) -> Result<Value, String> {
        let v = match &self.text {
            None => json!({}),
            Some(t) => serde_json::from_str(t).map_err(|e| format!("{}：不是合法 JSON（{e}）", self.path.display()))?,
        };
        if !v.is_object() {
            return Err(format!("{}：顶层不是对象", self.path.display()));
        }
        Ok(v)
    }

    pub fn state(&self) -> Result<State, String> {
        let data = self.data()?;
        let Some(sl) = data.get("statusLine") else { return Ok(State::None) };
        let command = match (sl["type"].as_str(), sl["command"].as_str()) {
            (Some("command"), Some(c)) => c,
            _ => return Err("statusLine 不是 {\"type\": \"command\", \"command\": …} 写法，不处理".into()),
        };
        Ok(match unwrap(command)? {
            Some(orig) => State::Installed { orig },
            None => State::Plain { command: command.to_owned() },
        })
    }

    /// 安装（`install = true`）或撤销的改动；不需要改为 `Ok(None)`。`statusLine` 的其他字段不动。
    pub fn plan(&self, install: bool) -> Result<Option<Plan>, String> {
        let mut data = self.data()?;
        let state = self.state()?;
        let action = match (install, &state) {
            (true, State::Installed { .. }) | (false, State::Plain { .. }) | (false, State::None) => return Ok(None),
            (true, State::Plain { command }) => {
                data["statusLine"]["command"] = json!(wrap(command));
                "把 statusLine.command 包进 ScyllaUI 的 statusline 桥（原命令不变，输出照旧）"
            }
            (true, State::None) => {
                data["statusLine"] = json!({"type": "command", "command": wrap(NONE)});
                "新增 statusLine（原来没有，桥的原命令为 `:`，状态栏不显示内容）"
            }
            (false, State::Installed { orig }) => {
                let only_ours = data["statusLine"].as_object().is_some_and(|o| o.keys().all(|k| k == "type" || k == "command"));
                if orig == NONE && only_ours {
                    data.as_object_mut().map(|o| o.shift_remove("statusLine"));
                    "删除 statusLine（安装前没有）"
                } else {
                    data["statusLine"]["command"] = json!(orig);
                    "statusLine.command 还原为原命令"
                }
            }
        };
        let eol = if self.text.as_deref().is_some_and(|t| !t.ends_with('\n')) { "" } else { "\n" };
        let new = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())? + eol;
        Ok(Some(Plan {
            tool: Tool::Claude,
            files: vec![FileChange { path: self.path.clone(), old: self.text.clone(), new: Some(new), backup: true, remove_empty_parent: false }],
            actions: vec![action.to_owned()],
            steps: vec![],
            other_hooks_enabled: 0,
            trust_needed: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hookcfg::apply;

    #[test]
    fn wrap_and_unwrap() {
        for orig in [":", "\"/a b/python\" \"/c/statusline.py\"", "echo hi # 注释", "a\nb", "{ x; }"] {
            assert_eq!(unwrap(&wrap(orig)), Ok(Some(orig.to_owned())));
        }
        assert_eq!(unwrap("echo hi"), Ok(None));
        let w = wrap("echo a");
        assert!(unwrap(&w.replacen("echo a", "echo b", 1)).is_err(), "两处不一致");
        assert!(unwrap(&(w.clone() + " ")).is_err(), "被改过");
        assert!(unwrap(&w.replace("statusline |", "statusline 2>/dev/null |")).is_err());
    }

    /// 包装在 sh 里的行为：ScyllaUI 终端外直接运行原命令；里面先经 `$SCYLLAUI_BIN statusline`。退出码是原命令的。
    #[test]
    fn wrapper_runs_in_sh() {
        let dir = std::env::temp_dir().join(format!("scyllaui-statusline-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("scyllaui");
        // 假的 scyllaui：原样转发 stdin，并记一笔。
        std::fs::write(&fake, format!("#!/bin/sh\ncat\necho called > '{}'\n", dir.join("called").display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |cmd: &str, env: &[(&str, &str)]| {
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(cmd).env_clear().env("PATH", "/usr/bin:/bin");
            for (k, v) in env {
                c.env(k, v);
            }
            c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
            let mut child = c.spawn().unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(b"{\"x\":1}").unwrap();
            let out = child.wait_with_output().unwrap();
            (String::from_utf8(out.stdout).unwrap(), out.status.code())
        };
        let orig = "tr x y # 注释\nexit 3";
        let bin = fake.to_str().unwrap();
        assert_eq!(run(&wrap(orig), &[]), run(orig, &[]), "终端外与原命令相同");
        assert!(!dir.join("called").exists());
        assert_eq!(run(&wrap(orig), &[("SCYLLAUI_SOCK", "/s"), ("SCYLLAUI_BIN", bin)]), ("{\"y\":1}".into(), Some(3)));
        assert!(dir.join("called").exists());
        assert_eq!(run(&wrap(NONE), &[("SCYLLAUI_SOCK", "/s"), ("SCYLLAUI_BIN", bin)]), (String::new(), Some(0)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 安装再撤销：文件与安装前逐字节一致；已装再装、未装撤销都无改动；原来没有 statusline 的撤销后删掉该键；
    /// 包装被改过时拒绝撤销；其他字段不动。
    #[test]
    fn install_then_uninstall_round_trips() {
        let dir = std::env::temp_dir().join(format!("scyllaui-statusline-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let path = dir.join(".claude/settings.json");
        let original = "{\n  \"model\": \"opus\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"\\\"/a b/py\\\" x.py\",\n    \"padding\": 0\n  },\n  \"hooks\": {}\n}\n";
        std::fs::write(&path, original).unwrap();
        let b = Bridge::read(&dir).unwrap();
        assert_eq!(b.state(), Ok(State::Plain { command: "\"/a b/py\" x.py".into() }));
        assert_eq!(b.plan(false).unwrap(), None, "未装：撤销无改动");
        let p = b.plan(true).unwrap().unwrap();
        assert!(p.diff().contains("scyllaui-statusline-bridge-v1"));
        assert_eq!(apply(&[p], "20261016-000000").unwrap().len(), 1, "写入前备份");
        let b = Bridge::read(&dir).unwrap();
        assert_eq!(b.state(), Ok(State::Installed { orig: "\"/a b/py\" x.py".into() }));
        let v: Value = serde_json::from_str(b.text.as_deref().unwrap()).unwrap();
        assert_eq!((&v["statusLine"]["padding"], &v["model"]), (&json!(0), &json!("opus")), "其他字段不动");
        assert_eq!(b.plan(true).unwrap(), None, "已装：再装无改动");
        apply(&[b.plan(false).unwrap().unwrap()], "20261016-000001").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "撤销后逐字节一致");
        // 原来没有 statusline、没有换行结尾。
        std::fs::write(&path, "{\n  \"model\": \"opus\"\n}").unwrap();
        let b = Bridge::read(&dir).unwrap();
        apply(&[b.plan(true).unwrap().unwrap()], "20261016-000002").unwrap();
        let b = Bridge::read(&dir).unwrap();
        assert_eq!(b.state(), Ok(State::Installed { orig: ":".into() }));
        apply(&[b.plan(false).unwrap().unwrap()], "20261016-000003").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\n  \"model\": \"opus\"\n}");
        // 包装被改过。
        let bad = json!({"statusLine": {"type": "command", "command": wrap("a") + "x"}});
        std::fs::write(&path, bad.to_string()).unwrap();
        assert!(Bridge::read(&dir).unwrap().plan(false).is_err());
        // 不认识的写法、不是 JSON。
        std::fs::write(&path, "{\"statusLine\": {\"type\": \"static\"}}").unwrap();
        assert!(Bridge::read(&dir).unwrap().plan(true).is_err());
        std::fs::write(&path, "{").unwrap();
        assert!(Bridge::read(&dir).unwrap().plan(true).is_err());
        // 文件不存在：安装时新建。
        std::fs::remove_file(&path).unwrap();
        let p = Bridge::read(&dir).unwrap().plan(true).unwrap().unwrap();
        assert_eq!(p.files[0].old, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
