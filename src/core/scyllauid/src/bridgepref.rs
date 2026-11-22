//! 用户要不要 Claude 的 statusline 桥（2026-11-22，docs/design.md 3.1 节「statusline 桥」）：存在
//! `<数据目录>/statusline-bridge.json`（`{"wanted": true}`），后台重启后还在。经「安装状态同步」或设置装桥时记 true、撤桥时记
//! false；第一次看到桥已经装着（如用户以前手动装过）也记 true。要过桥、现在却不在了（被 `tt setup` 之类的工具改掉），
//! 才报 `replaced` 提示用户修复；从没要过的不提示。

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
pub struct BridgePref {
    /// None：不落盘（测试）。
    path: Option<PathBuf>,
    pub wanted: bool,
}

impl BridgePref {
    /// 读已有的文件；缺失或内容非法时为「没要过」。
    pub fn load(path: PathBuf) -> BridgePref {
        let wanted = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["wanted"].as_bool())
            .unwrap_or(false);
        BridgePref { path: Some(path), wanted }
    }

    /// 记下新的选择；与原来相同不写。
    pub fn set(&mut self, wanted: bool) {
        if self.wanted == wanted {
            return;
        }
        self.wanted = wanted;
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let text = json!({"wanted": wanted}).to_string();
        if fs::write(&tmp, text + "\n").and_then(|_| fs::rename(&tmp, path)).is_err() {
            let _ = fs::remove_file(&tmp);
            log!("写 {} 失败", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_and_set_round_trip() {
        let dir = std::env::temp_dir().join(format!("scyllaui-bridgepref-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("statusline-bridge.json");
        let _ = fs::remove_file(&path);
        let mut p = BridgePref::load(path.clone());
        assert!(!p.wanted, "文件缺失时为没要过");
        p.set(true);
        assert!(BridgePref::load(path.clone()).wanted, "重启后还在");
        p.set(false);
        assert!(!BridgePref::load(path.clone()).wanted);
        fs::write(&path, "not json").unwrap();
        assert!(!BridgePref::load(path).wanted, "内容非法时为没要过");
        let _ = fs::remove_dir_all(&dir);
    }
}
