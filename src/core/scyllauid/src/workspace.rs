//! 重启后恢复终端（docs/design.md 10.5 节决策 12）：存活终端此刻所在的目录记在 `<数据目录>/workspace.json`，
//! 后台被结束或重启电脑后，下次同步时按它把终端开回原来的目录。只存「终端 ID → 目录、前台 AI」，不改表结构；
//! 先写临时文件再改名，中途失败不留下半个文件。
//! 前台 AI（M4b 块 5 P2，10.6 节）：终端前台正在跑的那款 AI，重建时据此说明没能接回的原因。文件里每个终端是
//! `{"dir", "agent", "ended"}`；读取兼容旧的「终端 ID → 目录字符串」写法与没有 `ended` 的写法。
//! 待确认的结束（2026-11-15，缺陷 B）：终端还开着时对话结束了，记下对话 ID，后台活过确认期再清掉；真实关机时 Agent
//! 先于后台被结束，后台随即消失、来不及清，重建时据此接回这段对话（docs/design.md「重启恢复」）。

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Default, Clone, PartialEq)]
struct Entry {
    dir: Option<String>,
    agent: Option<String>,
    /// 待确认的结束：刚结束的对话 ID。
    ended: Option<i64>,
}

#[derive(Default)]
pub struct WorkspaceDirs {
    /// None：不落盘（测试）。
    path: Option<PathBuf>,
    entries: BTreeMap<String, Entry>,
}

impl WorkspaceDirs {
    /// 读已有的文件；缺失或内容非法时从空开始。
    pub fn load(path: PathBuf) -> WorkspaceDirs {
        let entries = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| {
                        let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
                        let e = match v.as_str() {
                            Some(dir) => Entry { dir: Some(dir.to_owned()), ..Entry::default() },
                            None => Entry { dir: text("dir"), agent: text("agent"), ended: v.get("ended").and_then(Value::as_i64) },
                        };
                        (e != Entry::default()).then_some((k, e))
                    })
                    .collect()
            })
            .unwrap_or_default();
        WorkspaceDirs { path: Some(path), entries }
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.entries.get(id).and_then(|e| e.dir.as_deref())
    }

    pub fn set(&mut self, id: &str, dir: &str) {
        if self.get(id) != Some(dir) {
            self.entries.entry(id.to_owned()).or_default().dir = Some(dir.to_owned());
            self.save();
        }
    }

    /// 终端前台正在跑的 AI（工具键）。
    pub fn agent(&self, id: &str) -> Option<&str> {
        self.entries.get(id).and_then(|e| e.agent.as_deref())
    }

    /// 记下或清除前台 AI；没变化时不写文件。
    pub fn set_agent(&mut self, id: &str, agent: Option<&str>) {
        if self.agent(id) == agent {
            return;
        }
        let e = self.entries.entry(id.to_owned()).or_default();
        e.agent = agent.map(str::to_owned);
        if *e == Entry::default() {
            self.entries.remove(id);
        }
        self.save();
    }

    /// 这个终端待确认的结束（对话 ID）。
    pub fn ended(&self, id: &str) -> Option<i64> {
        self.entries.get(id).and_then(|e| e.ended)
    }

    /// 记下或清除待确认的结束；没变化时不写文件。
    pub fn set_ended(&mut self, id: &str, conversation: Option<i64>) {
        if self.ended(id) == conversation {
            return;
        }
        let e = self.entries.entry(id.to_owned()).or_default();
        e.ended = conversation;
        if *e == Entry::default() {
            self.entries.remove(id);
        }
        self.save();
    }

    pub fn remove(&mut self, id: &str) {
        if self.entries.remove(id).is_some() {
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let map: Map<String, Value> = self
            .entries
            .iter()
            .map(|(k, e)| {
                let mut v = json!({});
                if let Some(d) = &e.dir {
                    v["dir"] = json!(d);
                }
                if let Some(a) = &e.agent {
                    v["agent"] = json!(a);
                }
                if let Some(c) = e.ended {
                    v["ended"] = json!(c);
                }
                (k.clone(), v)
            })
            .collect();
        let text = serde_json::to_string(&map).unwrap_or_default();
        if fs::write(&tmp, text + "\n").and_then(|_| fs::rename(&tmp, path)).is_err() {
            let _ = fs::remove_file(&tmp);
            log!("写 {} 失败", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;

    #[test]
    fn dirs_survive_reload_and_bad_files_start_empty() {
        let d = test_dir("workspace");
        let path = d.join("workspace.json");
        let mut w = WorkspaceDirs::load(path.clone());
        w.set("t1", "/w/a");
        w.set("t2", "/w/b");
        w.remove("t2");
        let again = WorkspaceDirs::load(path.clone());
        assert_eq!((again.get("t1"), again.get("t2")), (Some("/w/a"), None));
        fs::write(&path, "半个{").unwrap();
        assert_eq!(WorkspaceDirs::load(path).get("t1"), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn agents_survive_reload_and_old_string_files_still_load() {
        let d = test_dir("workspace-agent");
        let path = d.join("workspace.json");
        // 旧版只存目录字符串（0.4.8 及以前）。
        fs::write(&path, r#"{"t1":"/w/a","t2":"/w/b"}"#).unwrap();
        let mut w = WorkspaceDirs::load(path.clone());
        assert_eq!((w.get("t1"), w.agent("t1")), (Some("/w/a"), None));
        w.set_agent("t1", Some("codex"));
        w.set_agent("t3", Some("kimi"));
        let again = WorkspaceDirs::load(path.clone());
        assert_eq!((again.get("t1"), again.agent("t1")), (Some("/w/a"), Some("codex")));
        assert_eq!((again.get("t3"), again.agent("t3")), (None, Some("kimi")));
        assert_eq!(again.get("t2"), Some("/w/b"));
        // 清除前台 AI：只剩目录的保留，什么都不剩的整条删掉。
        let mut w = again;
        w.set_agent("t1", None);
        w.set_agent("t3", None);
        let last = WorkspaceDirs::load(path.clone());
        assert_eq!((last.get("t1"), last.agent("t1")), (Some("/w/a"), None));
        assert!(!fs::read_to_string(&path).unwrap().contains("t3"), "空条目不写进文件");
        let _ = fs::remove_dir_all(&d);
    }

    /// 待确认的结束（缺陷 B）：随文件保存与读回，清除后空条目不留，终端删除时一起去掉；没有这个字段的旧文件照读。
    #[test]
    fn ended_conversation_survives_reload_until_cleared() {
        let d = test_dir("workspace-ended");
        let path = d.join("workspace.json");
        fs::write(&path, r#"{"t1":{"dir":"/w/a","agent":"kimi"}}"#).unwrap();
        let mut w = WorkspaceDirs::load(path.clone());
        assert_eq!(w.ended("t1"), None);
        w.set_ended("t1", Some(42));
        w.set_ended("t2", Some(7));
        let mut again = WorkspaceDirs::load(path.clone());
        assert_eq!((again.ended("t1"), again.get("t1"), again.ended("t2")), (Some(42), Some("/w/a"), Some(7)));
        again.set_ended("t1", None);
        again.set_ended("t2", None);
        let last = WorkspaceDirs::load(path.clone());
        assert_eq!((last.ended("t1"), last.get("t1")), (None, Some("/w/a")));
        assert!(!fs::read_to_string(&path).unwrap().contains("t2"), "只剩待确认结束的条目清除后整条删掉");
        let mut w = last;
        w.set_ended("t1", Some(9));
        w.remove("t1");
        assert_eq!(WorkspaceDirs::load(path).ended("t1"), None, "终端关闭时一起删掉");
        let _ = fs::remove_dir_all(&d);
    }
}
