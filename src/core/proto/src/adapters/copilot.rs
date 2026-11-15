//! GitHub Copilot CLI 适配器（M4b 块 9c）。依据：官方 Hooks 参考与 2026-10-30 本机实测 1.0.105
//! （`tests/hook-samples/copilot-1.0.105*.jsonl`）。载荷没有事件名，由 Hook 命令以 `--event` 带上。
//! 批准、拒绝审批与 Esc 中断都不发 Hook，从会话记录补（`waiting::copilot`，运行中与待审批期间都看）。

use super::{Adapter, Fields, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Copilot,
    command: "copilot",
    launch_args: "",
    preassign_id: false,
    // Hook 不带标题，第一条提示词即任务标题。
    title_source: TitleSource::HookField,
    reply_source: super::reply::ReplySource::CopilotEvents,
    model_source: super::usage::ModelSource::CopilotEvents,
    usage_source: super::usage::UsageSource::CopilotEvents,
    effort_source: super::usage::EffortSource::None,
    hosted: None,
    waiting_scan: Some(super::waiting::Scan::Lines(super::waiting::copilot)),
    scan_while_running: true,
    session_record: Some(|id| format!(".copilot/session-state/{id}/events.jsonl")),
    record_lookup: None,
    turns_by_record: false,
    pid_status: None,
    child_records: false,
    record_from_approval: false,
    subagent_approvals: super::SubagentApprovals { resolved_event: false, outlive_turn: false },
    matches,
    version,
    // 退出时提示的恢复命令（`Resume copilot --resume=<ID>`，2026-10-30 实测）。
    resume_args: |id| format!("--resume={id}"),
    map_event,
    fields: Fields {
        session: &["sessionId"],
        turn: &[],
        // 细分值：notification 的类型、errorOccurred 能否恢复。
        notification: &["notification_type", "recoverable"],
        title: &[],
        transcript: &[],
        // 子代理事件带 agentId（官方 Hooks 参考）。
        subagent: &["agentId"],
        turn_scoped_end: false,
        forked: None,
        model: &[],
        cwd: &["cwd"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["prompt"],
        subagent_empty: None,
        // 后台子代理结束时 Copilot 以 `<system_notification>` 包着的通知再发一次 userPromptSubmitted（2026-11-13 长跑，
        // 1.0.105 会话记录 `events.jsonl` 的 hook.start 记下了这份载荷）。
        injected_prompt: &["<system_notification>"],
        approval_id: &[],
        approval_command: &[],
    },
};

/// npm 包：`copilot` 命令是 Node 脚本 `@github/copilot/npm-loader.js`，它拉起平台包里的原生 `copilot`，两者同在前台
/// 进程组（2026-10-30 实测）。
fn matches(p: &Process) -> bool {
    p.path.ends_with(NATIVE) || p.args.get(1).is_some_and(|s| s.ends_with(LOADER)) || p.path.ends_with(SCRIPT)
}

/// 官方安装脚本 `https://gh.io/copilot-install`（2026-11-08 虚拟机实测）：普通用户装到 `~/.local/bin/copilot`，单个原生程序。
const SCRIPT: &str = "/.local/bin/copilot";

const NATIVE: &str = "/@github/copilot-darwin-arm64/copilot";
const LOADER: &str = "/@github/copilot/npm-loader.js";

/// Node 包装进程：`@github/copilot/package.json`；原生子进程：所在平台包的 `package.json`（两者版本相同）。
fn version(p: &Process) -> Option<VersionSource> {
    if p.path.ends_with(SCRIPT) {
        return Some(VersionSource::Command { path: p.path.to_owned() });
    }
    if let Some(script) = p.args.get(1).filter(|s| s.ends_with(LOADER)) {
        let root = script.strip_suffix("npm-loader.js")?;
        return Some(VersionSource::PackageJson { path: format!("{root}package.json"), strip: "" });
    }
    let root = p.path.strip_suffix("/copilot")?;
    Some(VersionSource::PackageJson { path: format!("{root}/package.json"), strip: "" })
}

fn map_event(name: &str, detail: Option<&str>) -> Option<Kind> {
    Some(match name {
        "sessionStart" => Kind::SessionStart,
        "userPromptSubmitted" => Kind::PromptSubmit,
        "preToolUse" | "postToolUse" | "postToolUseFailure" => Kind::ToolActivity,
        // 命令被自动放行时也发（2026-10-30 实测 echo），不能当作「需确认」；真正弹出审批框时另有 notification。
        "permissionRequest" => Kind::Other,
        "notification" => match detail {
            // 弹出审批框；AI 提问也算需确认（docs/design.md 3.1 节「AI 提问算『需确认』」）。
            Some("permission_prompt" | "elicitation_dialog") => Kind::PermissionWait,
            Some("agent_idle") => Kind::Idle,
            _ => Kind::Other,
        },
        "agentStop" => Kind::TurnEnd,
        "errorOccurred" => match detail {
            Some("false") => Kind::TurnFail,
            _ => Kind::Other,
        },
        "sessionEnd" => Kind::SessionEnd,
        "userPromptTransformed" | "subagentStart" | "subagentStop" | "preCompact" => Kind::Other,
        _ => return None,
    })
}
