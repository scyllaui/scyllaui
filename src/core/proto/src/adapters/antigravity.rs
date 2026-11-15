//! Antigravity CLI 适配器（M4b 块 9b）。依据：官方 Hooks 文档（<https://antigravity.google/docs/hooks?tab=cli>）与
//! 2026-11-01 本机实测 1.4.12（`tests/hook-samples/antigravity-1.4.12*.jsonl`）。载荷没有事件名，由 Hook 命令以 `--event`
//! 带上。没有提交提示词与审批事件：每轮第一次调用模型（PreInvocation 的 `invocationNum` 为 0）即提交提示词，载荷的
//! `lastUserInput` 是提示词原文；等待审批、拒绝审批与 Esc 中断都不发 Hook，从对话库认出（`waiting::antigravity`，运行中与
//! 待审批期间都看）。

use super::{Adapter, Fields, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Antigravity,
    command: "agy",
    launch_args: "",
    preassign_id: false,
    // agy 为每个对话生成标题，记在 `conversation_summaries.db`；生成之前用第一条提示词。
    title_source: TitleSource::AntigravitySummaries,
    reply_source: super::reply::ReplySource::AntigravityTranscript,
    // 每个 Hook 都带 `modelName`。
    model_source: super::usage::ModelSource::None,
    usage_source: super::usage::UsageSource::AntigravityTranscript,
    effort_source: super::usage::EffortSource::None,
    hosted: None,
    waiting_scan: Some(super::waiting::Scan::AntigravityDb),
    scan_while_running: true,
    session_record: Some(|id| format!(".gemini/antigravity-cli/conversations/{id}.db")),
    record_lookup: None,
    turn_end_by_record: false,
    pid_status: None,
    child_records: true,
    record_from_approval: false,
    // 子代理比主会话这一轮活得久（2026-11-14 实测 1.5.8：主会话 Stop 时子代理还在等审批），审批的出现与消失都从
    // 子代理的对话库认（`child_records`），主会话一轮结束不清掉还在等的审批。
    subagent_approvals: super::SubagentApprovals { resolved_event: false, outlive_turn: true },
    matches,
    version,
    // 退出时提示的恢复命令（`agy --conversation=<ID>`，2026-11-01 实测）。
    resume_args: |id| format!("--conversation={id}"),
    map_event,
    fields: Fields {
        session: &["conversationId"],
        turn: &[],
        // 细分值：PreInvocation 的调用序号（0 为一轮的第一次）、Stop 的出错信息（正常结束为空字符串）。
        notification: &["invocationNum", "error"],
        title: &[],
        transcript: &["transcriptPath"],
        subagent: &[],
        turn_scoped_end: false,
        forked: None,
        model: &["modelName"],
        cwd: &["workspacePaths.0"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["lastUserInput"],
        // 子代理与主对话在同一进程里各有会话 ID，子代理的事件 `lastUserInput` 为空字符串（2026-11-01 实测）。
        subagent_empty: Some("lastUserInput"),
        subagent_when: None,
        injected_prompt: &[],
        prompt_origin: None,
        approval_id: &[],
        approval_command: &[],
    },
};

/// 官方安装脚本放在 `~/.local/bin/agy`，单个原生进程（2026-11-01 实测 1.4.12，后台自更新时原地替换）。
fn matches(p: &Process) -> bool {
    p.path.ends_with("/.local/bin/agy")
}

/// 执行 `agy --version`，输出只有版本号（`1.4.12`，约 70 ms）。
fn version(p: &Process) -> Option<VersionSource> {
    Some(VersionSource::Command { path: p.path.to_owned() })
}

fn map_event(name: &str, detail: Option<&str>) -> Option<Kind> {
    Some(match name {
        "SessionStart" => Kind::SessionStart,
        // 每轮第一次调用模型为 0，之后每次调用模型递增（工具调用之后）。后台命令跑完后 agy 自己开一轮，同样从 0 起，
        // `lastUserInput` 仍是上一条提示词（2026-11-01 实测）。
        "PreInvocation" => match detail {
            Some("0") => Kind::PromptSubmit,
            _ => Kind::ToolActivity,
        },
        // 这三个不装（`hookcfg::events`）：PostToolUse 在后台命令跑完时才发，会把已结束的一轮改回运行中，所以不改状态。
        "PreToolUse" | "PostToolUse" | "PostInvocation" => Kind::Other,
        // 正常结束 `error` 为空字符串；`fullyIdle` 为假（后台命令还在跑）时 agy 同样回到输入框，按一轮结束处理。
        "Stop" => match detail {
            Some(_) => Kind::TurnFail,
            None => Kind::TurnEnd,
        },
        _ => return None,
    })
}
