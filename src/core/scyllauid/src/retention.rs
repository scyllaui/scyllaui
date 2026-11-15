//! 已结束对话的保留设置（M4a，docs/design.md 10.4 节决策 7，docs/protocol.md 第 7 节「保留上限」）：
//! `<数据目录>/retention.json`，由后台持有。只允许设置页里的几档，文件缺失时用默认值，内容非法时用默认值并记日志。

use crate::store::{KEEP_CONVERSATIONS, KEEP_DAYS};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;

/// 天数与条数的可选档位（上限 500 条与 M2 搜索实测的规模一致）。
pub const DAY_CHOICES: [i64; 5] = [7, 14, 30, 60, 90];
pub const COUNT_CHOICES: [i64; 4] = [50, 100, 200, 500];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    pub keep_days: i64,
    pub keep_conversations: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { keep_days: KEEP_DAYS, keep_conversations: KEEP_CONVERSATIONS }
    }
}

impl Retention {
    /// 从请求或文件内容取两个值；缺字段或不在档位里为 None。
    pub fn parse(v: &Value) -> Option<Retention> {
        let days = v["keep_days"].as_i64().filter(|d| DAY_CHOICES.contains(d))?;
        let count = v["keep_conversations"].as_i64().filter(|n| COUNT_CHOICES.contains(n))?;
        Some(Retention { keep_days: days, keep_conversations: count })
    }

    pub fn to_json(self) -> Value {
        json!({"keep_days": self.keep_days, "keep_conversations": self.keep_conversations})
    }

    pub fn load(path: &Path) -> Retention {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Retention::default(),
            Err(e) => {
                log!("读取 {} 失败，保留设置用默认值：{e}", path.display());
                return Retention::default();
            }
        };
        match serde_json::from_str::<Value>(&text).ok().as_ref().and_then(Retention::parse) {
            Some(r) => r,
            None => {
                log!("{} 内容非法，保留设置用默认值", path.display());
                Retention::default()
            }
        }
    }

    /// 先写临时文件再改名，中途失败不留下半个文件。
    pub fn save(self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, format!("{}\n", self.to_json()))?;
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_only_choices() {
        assert_eq!(
            Retention::parse(&json!({"keep_days": 7, "keep_conversations": 50})),
            Some(Retention { keep_days: 7, keep_conversations: 50 })
        );
        for bad in [
            json!({"keep_days": 8, "keep_conversations": 50}),
            json!({"keep_days": 7, "keep_conversations": 1000}),
            json!({"keep_days": "7", "keep_conversations": 50}),
            json!({"keep_days": 7}),
            json!(null),
        ] {
            assert_eq!(Retention::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn load_and_save_round_trip() {
        let dir = std::env::temp_dir().join(format!("scyllaui-retention-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("retention.json");
        assert_eq!(Retention::load(&path), Retention::default(), "文件缺失用默认值");
        let r = Retention { keep_days: 90, keep_conversations: 500 };
        r.save(&path).unwrap();
        assert_eq!(Retention::load(&path), r);
        assert!(!dir.join("retention.json.tmp").exists());
        fs::write(&path, "{\"keep_days\": 3, \"keep_conversations\": 100}").unwrap();
        assert_eq!(Retention::load(&path), Retention::default(), "档位外的值用默认值");
        fs::write(&path, "不是 JSON").unwrap();
        assert_eq!(Retention::load(&path), Retention::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
