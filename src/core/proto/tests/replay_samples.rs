//! 回放 tests/hook-samples/ 中按 CLI 版本存档的真实 Hook 样本（2026-10-08 S0 第 3 项在 ScyllaUI 窗口中采集），
//! 经各自的适配器规范化后核对状态机；样本中的每个事件都必须被适配器识别。新增 CLI 版本样本时在 CASES 中补一行。

use scyllaui_proto::adapters::adapter;
use scyllaui_proto::{normalize, AgentState, Applied, Kind, PaneState, Tool};
use serde_json::Value;
use std::path::PathBuf;

struct Case {
    file: &'static str,
    /// 回放结束时的状态与绑定的会话 ID。
    end: AgentState,
    session: &'static str,
    /// 样本中含审批事件，回放时要核对进入 waiting。
    approval: bool,
}

const CASES: &[Case] = &[
    // 依次：启动、两轮提交、审批后拒绝、退出、恢复、退出、以 --session-id 启动、退出。
    Case {
        file: "claude-2.1.292",
        end: AgentState::Ended,
        session: "11111111-2222-4333-8444-555555555555",
        approval: true,
    },
    Case {
        file: "codex-0.160.0",
        end: AgentState::Ended,
        session: "01a11794-654c-7283-90aa-3eae0502766a",
        approval: true,
    },
    // 含后台记忆整理会话（transcript_path 为 null）的事件。
    Case {
        file: "codex-0.161.0",
        end: AgentState::Ended,
        session: "01a11923-448f-7a90-9a41-daf5e08da12f",
        approval: true,
    },
    Case {
        file: "codex-0.161.0-resume",
        end: AgentState::Ended,
        session: "01a11923-448f-7a90-9a41-daf5e08da12f",
        approval: false,
    },
    Case {
        file: "kimi-2.1.1",
        end: AgentState::Ended,
        session: "session_61f5a8db-b5a2-4364-aa70-88af1ee7e675",
        approval: true,
    },
    // 含从 ~/.claude/settings.json 触发、已标记丢弃的重复事件，以及 SessionEnd 之后的会话级 Stop。
    Case {
        file: "grok-1.0.46",
        end: AgentState::Ended,
        session: "22222222-3333-4444-8555-666666666666",
        approval: true,
    },
    // 2026-10-10 M1 第 2 块真机采集（ScyllaUI 窗口，均为「发一句话后退出」，不含审批）。
    // Claude 两段会话；Grok 未退出，进程被 kill -9（无 SessionEnd），停在 idle_prompt 之后的 ready。
    Case {
        file: "claude-2.1.296",
        end: AgentState::Ended,
        session: "e67a6458-1174-423f-b52f-ccf2d61b3ef9",
        approval: false,
    },
    Case {
        file: "codex-0.162.0",
        end: AgentState::Ended,
        session: "01a12497-e0af-78a2-b4d4-cbae64a70f82",
        approval: false,
    },
    Case {
        file: "grok-1.0.50",
        end: AgentState::Ready,
        session: "01a1249b-39d4-7da1-bf3b-e444de5ded6d",
        approval: false,
    },
    // 2026-10-29 OpenCode 1.19.5 经 ScyllaUI 插件采集（服务端模式驱动）：同意、拒绝、回答 AI 的提问、中断（出错事件后紧跟
    // 轮次结束）、派子代理（子会话的事件带子代理标记）。认得但不改状态的事件（`session.updated` 只带标题）照常回放。
    Case {
        file: "opencode-1.19.5",
        end: AgentState::Ready,
        session: "ses_e77e6b1c1ffej0usMZwsmn6d9L",
        approval: true,
    },
];

fn samples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/hook-samples")
}

#[test]
fn replay_recorded_samples() {
    for case in CASES {
        let path = samples_dir().join(format!("{}.jsonl", case.file));
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut pane = PaneState::default();
        let mut waiting_checks = 0;
        for (n, line) in text.lines().enumerate() {
            let row: Value = serde_json::from_str(line).unwrap();
            let tool = Tool::parse(row["source"].as_str().unwrap()).unwrap();
            let ev = normalize(tool, &row["payload"])
                .unwrap_or_else(|| panic!("{} 第 {} 行无法规范化", case.file, n + 1));
            assert_eq!(
                adapter(tool).anomaly(&row["payload"]),
                None,
                "{} 第 {} 行 {} 未被适配器识别",
                case.file,
                n + 1,
                ev.name
            );
            if row["dropped"].as_bool().unwrap() {
                continue;
            }
            let applied = pane.apply(tool, &ev);
            let asks = ev.kind == Kind::PermissionWait;
            if asks && applied == Applied::Changed {
                assert_eq!(
                    pane.state,
                    AgentState::Waiting,
                    "{} 第 {} 行审批事件后应为 waiting",
                    case.file,
                    n + 1
                );
                waiting_checks += 1;
            }
        }
        assert_eq!(pane.state, case.end, "{} 结束状态", case.file);
        assert_eq!(
            pane.session_id.as_deref(),
            Some(case.session),
            "{} 绑定的会话",
            case.file
        );
        assert_eq!(waiting_checks > 0, case.approval, "{} 审批事件", case.file);
    }
}

/// 冒烟测试工具（src/scripts/smoke-cli.py，M1 第 7 块）存下的 `*-smoke.jsonl` 不必逐个登记：每个事件都要被适配器
/// 识别，主会话只有一个（恢复后 ID 不变），回放后窗格绑定它且状态不是「未知」。
#[test]
fn replay_smoke_samples() {
    let mut n = 0;
    for entry in std::fs::read_dir(samples_dir()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with("-smoke.jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let mut pane = PaneState::default();
        let mut sessions = std::collections::BTreeSet::new();
        for (i, line) in text.lines().enumerate() {
            let row: Value = serde_json::from_str(line).unwrap();
            let tool = Tool::parse(row["source"].as_str().unwrap()).unwrap();
            let ev = normalize(tool, &row["payload"]).unwrap_or_else(|| panic!("{name} 第 {} 行无法规范化", i + 1));
            assert_eq!(adapter(tool).anomaly(&row["payload"]), None, "{name} 第 {} 行 {} 未被适配器识别", i + 1, ev.name);
            if row["dropped"].as_bool().unwrap() || ev.background || ev.subagent {
                continue;
            }
            sessions.extend(ev.session_id.clone());
            pane.apply(tool, &ev);
        }
        assert_eq!(sessions.len(), 1, "{name} 主会话应只有一个（恢复后 ID 不变）：{sessions:?}");
        assert_eq!(pane.session_id.as_ref(), sessions.iter().next(), "{name} 绑定的会话");
        assert_ne!(pane.state, AgentState::Unknown, "{name} 回放后状态");
        n += 1;
    }
    assert!(n >= 4, "冒烟样本不全：{n}");
}

/// 「状态可能不准」不误报（M4b 块 6）：全部历史样本（含守护进程与后台会话）的每条事件都不算异常。
#[test]
fn no_anomaly_in_any_sample() {
    let mut n = 0;
    for entry in std::fs::read_dir(samples_dir()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        for (i, line) in std::fs::read_to_string(entry.path()).unwrap().lines().enumerate() {
            let row: Value = serde_json::from_str(line).unwrap();
            let tool = Tool::parse(row["source"].as_str().unwrap()).unwrap();
            assert_eq!(adapter(tool).anomaly(&row["payload"]), None, "{name} 第 {} 行", i + 1);
            n += 1;
        }
    }
    assert!(n > 100, "样本事件太少：{n}");
}

/// 回归（2026-11-13 长跑缺陷 B）：Copilot 1.0.105 后台子代理结束时以 `<system_notification>` 再发一次 userPromptSubmitted，
/// 侧栏副标题曾显示这段通知。样本取自 Copilot 会话记录 `events.jsonl` 里 hook.start 记下的原样载荷：用户提问、主代理派给
/// 子代理的任务（带 `agentId`）、子代理结束的通知。
#[test]
fn copilot_injected_prompts_are_not_user_prompts() {
    let text = std::fs::read_to_string(samples_dir().join("copilot-1.0.105-injected.jsonl")).unwrap();
    let evs: Vec<_> = text
        .lines()
        .map(|l| normalize(Tool::Copilot, &serde_json::from_str::<Value>(l).unwrap()["payload"]).unwrap())
        .collect();
    assert!(evs.iter().all(|e| e.kind == Kind::PromptSubmit));
    assert!(evs[0].prompt.as_deref().is_some_and(|p| p.starts_with("Use a subagent")) && !evs[0].subagent);
    assert!(evs[1].subagent, "子代理的任务不改窗格状态");
    assert_eq!((evs[2].prompt.as_deref(), evs[2].subagent), (None, false), "通知改状态，但不算提示词");
    let mut pane = PaneState::default();
    pane.apply(Tool::Copilot, &evs[0]);
    assert_eq!(pane.apply(Tool::Copilot, &evs[1]), Applied::Ignored);
    pane.apply(Tool::Copilot, &evs[2]);
    assert_eq!((pane.state, pane.session_id.as_deref()), (AgentState::Running, Some("5845f27b-42f1-4f01-aef3-ae7e9ad825cf")));
}
