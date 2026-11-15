//! hookcfg 与 S0 的 src/scripts/s0-hooks.py（2026-10-16 删除，最后版本见提交 459fc2a）逐字节兼容（docs/design.md 4.7 节「一键修复与撤销」）。
//! 期望结果由 tests/hook-config/make-goldens.py 用 s0-hooks.py 生成：每个用例的 before 上执行 install / uninstall，
//! 以及 install 结果上再 uninstall（roundtrip）。这里在临时目录里用 Rust 做同样的操作，逐个文件比对。
//! 唯一有意的差异：s0-hooks.py 对没有本项目条目的文件执行 uninstall 时也会改写（不存在的文件会被新建为 `{}` 或空文件），
//! Rust 版没有要删的条目就不写。

use scyllaui_proto::hookcfg::{self, Action, TOOLS};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/hook-config")
}

/// 目录下所有文件（相对路径 → 内容），不含备份。
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if !p.to_string_lossy().contains(".scyllaui-bak-") {
                out.insert(p.strip_prefix(base).unwrap().display().to_string(), std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn copy_tree(from: &Path, to: &Path) {
    for (rel, bytes) in tree(from) {
        let p = to.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}

fn temp_home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("scyllaui-hookcfg-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 对四款依次生成改动并写入（与 s0-hooks.py 一次处理四款相同），返回写入时新建的备份。
fn run(home: &Path, action: Action) -> Vec<PathBuf> {
    let mut backups = vec![];
    for tool in TOOLS {
        let snap = hookcfg::read(tool, home.to_str().unwrap());
        if let Some(plan) = hookcfg::plan(&snap, action).unwrap() {
            backups.extend(hookcfg::apply(&[plan], "20261015-000000").unwrap());
        }
    }
    backups
}

#[test]
fn matches_s0_hooks_goldens() {
    let mut cases = 0;
    for case in std::fs::read_dir(root()).unwrap().flatten().map(|e| e.path()).filter(|p| p.join("before").is_dir()) {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        for (scenario, start, action) in [
            ("install", "before", Action::Repair),
            ("uninstall", "before", Action::Uninstall),
            ("roundtrip", "install", Action::Uninstall),
        ] {
            let home = temp_home(&format!("{name}-{scenario}"));
            copy_tree(&case.join(start), &home);
            let original = tree(&home);
            let backups = run(&home, action);
            let got = tree(&home);
            let mut want = tree(&case.join(scenario));
            if action == Action::Uninstall {
                // 有意的差异：s0-hooks.py 在 uninstall 时会新建原来不存在的文件，Rust 版不写。
                want.retain(|rel, _| original.contains_key(rel) || got.contains_key(rel));
            }
            assert_eq!(
                got.keys().collect::<Vec<_>>(),
                want.keys().collect::<Vec<_>>(),
                "{name}/{scenario}：文件清单不同"
            );
            for (rel, bytes) in &want {
                assert!(
                    got[rel] == *bytes,
                    "{name}/{scenario}/{rel} 与 s0-hooks.py 的结果不同：\n--- 期望\n{}\n--- 实际\n{}",
                    String::from_utf8_lossy(bytes),
                    String::from_utf8_lossy(&got[rel])
                );
            }
            // 每个被改写的原有文件都有备份，内容为改写前的原文（本项目自建的 Grok 文件除外）。
            for (rel, before) in &original {
                let changed = got.get(rel) != Some(before);
                let ours = rel.ends_with(".grok/hooks/scyllaui.json");
                let bak = backups.iter().find(|b| b.to_string_lossy().starts_with(&home.join(rel).display().to_string()));
                match (changed && !ours, bak) {
                    (true, Some(b)) => assert_eq!(&std::fs::read(b).unwrap(), before, "{name}/{scenario}/{rel} 的备份"),
                    (true, None) => panic!("{name}/{scenario}/{rel} 被改写但没有备份"),
                    (false, Some(b)) => panic!("{name}/{scenario}/{rel} 没有改写却有备份 {}", b.display()),
                    (false, None) => {}
                }
            }
            let _ = std::fs::remove_dir_all(&home);
        }
        cases += 1;
    }
    assert!(cases >= 2, "tests/hook-config 下的用例不全");
}

/// 修复后再分析为 ok，再修复一次没有改动（幂等）。
#[test]
fn repair_then_status_ok_and_idempotent() {
    for case in ["fresh", "mixed"] {
        let home = temp_home(&format!("{case}-idem"));
        copy_tree(&root().join(case).join("before"), &home);
        run(&home, Action::Repair);
        for tool in TOOLS {
            let snap = hookcfg::read(tool, home.to_str().unwrap());
            let st = hookcfg::analyze(&snap);
            // Codex 没有信任记录：配置本身已修好，只差用户在 /hooks 里信任。
            // 用例里没有 OpenCode、Copilot、Antigravity 的目录：视为没装，修复不建（M4b 块 9a、9b、9c）。
            let want = match tool {
                scyllaui_proto::Tool::Codex => hookcfg::State::Untrusted,
                scyllaui_proto::Tool::OpenCode | scyllaui_proto::Tool::Copilot | scyllaui_proto::Tool::Antigravity => hookcfg::State::Absent,
                _ => hookcfg::State::Ok,
            };
            assert_eq!(st.state, want, "{case}/{}：{:?}", tool.as_str(), st.problems);
            let again = hookcfg::plan(&snap, Action::Repair).unwrap();
            assert!(again.as_ref().is_none_or(|p| p.files.is_empty()), "{case}/{}：再修复仍有改动", tool.as_str());
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
