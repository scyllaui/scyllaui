//! Kimi Code 适配器。依据：S0 第 2、3 项实测（Kimi 2.1.1）。Kimi 不设置自己的环境变量，来源只能按触发它的配置判定。

use super::{notification, Adapter, Fields, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Kimi,
    command: "kimi",
    launch_args: "",
    preassign_id: false,
    title_source: TitleSource::HookField,
    reply_source: super::reply::ReplySource::KimiWire,
    model_source: super::usage::ModelSource::KimiWire,
    usage_source: super::usage::UsageSource::KimiWire,
    effort_source: super::usage::EffortSource::KimiWire,
    hosted: None,
    // 2.6.1 起一轮正常结束不发 Stop（包内 `registerLoopHooks` 的判断永远成立），失败、取消的 Hook 又分不出主、子代理：
    // 轮次结束读会话记录的 `turn.ended`（`waiting::kimi`，2026-11-14）。
    waiting_scan: Some(super::waiting::Scan::Lines(super::waiting::kimi)),
    scan_while_running: true,
    session_record: None,
    record_lookup: Some(super::RecordLookup::KimiIndex),
    turn_end_by_record: true,
    pid_status: None,
    child_records: false,
    record_from_approval: false,
    // PermissionResult 带与 PermissionRequest 相同的 `id`、`agent_id`（2.6.1 源码；子代理会话记录 interaction.resolved 为证）。
    // 子代理跑在主代理的一次工具调用里，不比这一轮活得久。
    subagent_approvals: super::SubagentApprovals { resolved_event: true, outlive_turn: false },
    matches,
    // 单个可执行文件，没有随包的版本文件（`updates/install.json` 记的是最近一次安装，不一定是正在运行的那个）。
    version: |p| Some(VersionSource::Command { path: p.path.to_owned() }),
    // 退出时提示的命令（`--help` 中为 `-S/--session`）。
    resume_args: |id| format!("-r {id}"),
    map_event,
    fields: Fields {
        session: &["session_id"],
        turn: &["turn_id"],
        notification: &["notification_type"],
        title: &["session_title"],
        transcript: &["transcript_path"],
        // 主代理事件也带 `agent_id`，值为 "main"，不算子代理（S0 实测）。
        subagent: &["agent_id"],
        turn_scoped_end: false,
        forked: None,
        model: &["model"],
        cwd: &["cwd"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["prompt"],
        subagent_empty: None,
        subagent_when: Some(("origin_name", "subagent")),
        injected_prompt: &[],
        // TurnStarted 带 `origin_kind`：用户提交为 `user`；子代理收到的任务为 `system_trigger`（`origin_name` 为 `subagent`，
        // 正文以 `<git-context>` 开头，且不带 `agent_id`），2026-11-13 长跑核对 2.5.2 的会话记录 `agents/<代理>/wire.jsonl`。
        prompt_origin: Some(("origin_kind", "user")),
        approval_id: &["id"],
        approval_command: &[],
    },
};

/// 官方安装器：`~/.kimi-code/bin/kimi`。
fn matches(p: &Process) -> bool {
    p.path.ends_with("/.kimi-code/bin/kimi")
}

fn map_event(name: &str, notification_type: Option<&str>) -> Option<Kind> {
    Some(match name {
        "SessionStart" => Kind::SessionStart,
        "UserPromptSubmit" | "TurnStarted" => Kind::PromptSubmit,
        "PreToolUse" | "PostToolUse" => Kind::ToolActivity,
        "PermissionRequest" => Kind::PermissionWait,
        "PermissionResult" => Kind::PermissionDone,
        "Notification" => notification(notification_type),
        "Stop" => Kind::TurnEnd,
        "StopFailure" => Kind::TurnFail,
        // 取消一轮时发（2.6.1 源码 `notifyTurnEnded`）。
        "Interrupt" => Kind::TurnInterrupt,
        "SessionEnd" => Kind::SessionEnd,
        _ => return None,
    })
}
