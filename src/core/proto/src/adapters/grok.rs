//! Grok Build 适配器。依据：S0 第 2、3 项实测（Grok 1.0.46）、Grok 官方用户指南，与 2026-10-10 本机安装路径（1.0.50）。

use super::{file_name, notification, Adapter, Fields, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Grok,
    command: "grok",
    launch_args: "",
    preassign_id: true,
    title_source: TitleSource::GrokSummary,
    reply_source: super::reply::ReplySource::GrokUpdates,
    model_source: super::usage::ModelSource::GrokSummary,
    usage_source: super::usage::UsageSource::GrokUsageJson,
    effort_source: super::usage::EffortSource::GrokSummary,
    hosted: None,
    // 批准审批后、命令跑完前不发 Hook：待审批期间读会话记录的 `permission_resolved`（`waiting::grok`，2026-11-14）。
    waiting_scan: Some(super::waiting::Scan::Lines(super::waiting::grok)),
    scan_while_running: false,
    session_record: None,
    record_lookup: Some(super::RecordLookup::GrokSessions),
    turns_by_record: false,
    pid_status: None,
    child_records: false,
    record_from_approval: false,
    subagent_approvals: super::SubagentApprovals { resolved_event: false, outlive_turn: false },
    // 1.0.77：会话开始时读，`/new` 或重开后生效。（2026-11-18 实测，docs/s0-hook-compat.md）
    hook_load: super::HookLoad::Session,
    submit_record: true,
    guide: super::Guide { new_session: Some("/new"), quit: Some("/quit"), resume_last: Some("grok --continue") },
    matches,
    version,
    // 默认沿用原 ID（加 `--fork-session` 才生成新 ID）。
    resume_args: |id| format!("--resume {id}"),
    map_event,
    fields: Fields {
        session: &["sessionId", "session_id"],
        turn: &["promptId", "prompt_id"],
        notification: &["notificationType", "notification_type"],
        title: &[],
        transcript: &["transcriptPath", "transcript_path"],
        subagent: &["agentId", "agent_id", "subagentType", "subagent_type"],
        // 官方说明：只看 Stop 会误判空闲，按每轮 promptId 去重并结合 idle_prompt。
        turn_scoped_end: true,
        forked: None,
        model: &[],
        cwd: &["cwd"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["prompt"],
        subagent_empty: None,
        injected_prompt: &[],
        approval_id: &[],
        approval_command: &[],
    },
};

/// 官方安装器把可执行文件放在 `~/.grok/downloads/`：1.0.46 为 `grok-macos-aarch64`，
/// 1.0.50 起为带版本号的 `grok-<版本>-macos-aarch64`。`grok`、`agent` 两个命令都指向它，不按命令名判断。
fn matches(p: &Process) -> bool {
    let name = file_name(p.path);
    p.path.contains("/.grok/downloads/")
        && name.starts_with("grok-")
        && name.ends_with("-macos-aarch64")
}

/// 1.0.50 起文件名带版本（`grok-1.0.52-macos-aarch64`）；1.0.46 的 `grok-macos-aarch64` 没有，执行 `--version`。
fn version(p: &Process) -> Option<VersionSource> {
    let name = file_name(p.path);
    let ver = name.strip_prefix("grok-").and_then(|n| n.strip_suffix("-macos-aarch64")).unwrap_or("");
    if ver.starts_with(|c: char| c.is_ascii_digit()) {
        Some(VersionSource::Known { version: ver.to_owned(), file: p.path.to_owned() })
    } else {
        Some(VersionSource::Command { path: p.path.to_owned() })
    }
}

fn map_event(name: &str, notification_type: Option<&str>) -> Option<Kind> {
    Some(match name {
        "SessionStart" => Kind::SessionStart,
        "UserPromptSubmit" => Kind::PromptSubmit,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Kind::ToolActivity,
        // AI 提问也算需确认（docs/design.md 3.1 节「AI 提问算『需确认』」）：Grok 弹出 AskQuestion 选项框时发
        // `elicitation_dialog`（2026-11-05 实测 1.0.70，官方文档的类型清单里没列）。
        "Notification" if notification_type == Some("elicitation_dialog") => Kind::PermissionWait,
        "Notification" => notification(notification_type),
        "Stop" => Kind::TurnEnd,
        "StopFailure" => Kind::TurnFail,
        "StopCancelled" => Kind::TurnInterrupt,
        "SessionEnd" => Kind::SessionEnd,
        _ => return None,
    })
}
