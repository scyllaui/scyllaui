//! Claude Code 适配器。依据：S0 第 2、3 项实测（docs/s0-hook-compat.md）与 2026-10-10 本机安装路径。

use super::{notification, Adapter, Fields, Hosted, HostedMarker, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};
use serde_json::Value;

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Claude,
    command: "claude",
    launch_args: "",
    preassign_id: true,
    title_source: TitleSource::TranscriptRecords,
    reply_source: super::reply::ReplySource::ClaudeTranscript,
    model_source: super::usage::ModelSource::ClaudeTranscript,
    usage_source: super::usage::UsageSource::ClaudeTranscript,
    effort_source: super::usage::EffortSource::ClaudeTranscript,
    hosted: Some(Hosted {
        marker: HostedMarker::Env("CLAUDE_JOB_DIR"),
        registry_dir: Some(".claude/sessions"),
        job_of,
        attached,
        retry: true,
        infer: false,
        hosts_job,
        reattach_args: Some(|job| format!("attach {job}")),
        jobs_dir: Some(".claude/jobs"),
        job_session: |state| state["sessionId"].as_str().map(str::to_owned),
    }),
    waiting_scan: Some(super::waiting::Scan::Lines(super::waiting::claude)),
    scan_while_running: false,
    session_record: None,
    record_lookup: None,
    turn_end_by_record: false,
    pid_status: Some((".claude/sessions", super::waiting::claude_status)),
    child_records: false,
    record_from_approval: false,
    subagent_approvals: super::SubagentApprovals { resolved_event: false, outlive_turn: false },
    matches,
    version: |p| {
        if let Some(v) = native_version(p.path) {
            return Some(VersionSource::Known { version: v.to_owned(), file: p.path.to_owned() });
        }
        let root = p.path.strip_suffix("bin/claude.exe")?;
        Some(VersionSource::PackageJson { path: format!("{root}package.json"), strip: "" })
    },
    resume_args: |id| format!("--resume {id}"),
    map_event,
    fields: Fields {
        session: &["session_id"],
        turn: &["prompt_id"],
        notification: &["notification_type"],
        title: &[],
        transcript: &["transcript_path"],
        subagent: &["agent_id"],
        turn_scoped_end: false,
        forked: Some(("source", "fork")),
        model: &["model"],
        cwd: &["cwd"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["prompt"],
        subagent_empty: None,
        subagent_when: None,
        // 两种都是 Claude 自己排队投递的消息（transcript 里为 `queued_command` 附件，用户输入的那条带 `origin.kind: human`）：
        // 子代理交回结果 2026-11-05 实测会触发 UserPromptSubmit；后台任务完成通知同一机制，尚未实测到触发，一并排除。
        injected_prompt: &["<agent-message from=\"", "<task-notification>"],
        prompt_origin: None,
        approval_id: &[],
        approval_command: &[],
    },
};

/// 后台会话的任务 ID 是会话 ID 的前 8 位（2026-10-12 实测：`sessions/<pid>.json` 中 `jobId` 与 `sessionId` 对照）。
fn job_of(session: &str) -> Option<String> {
    let id: String = session.chars().take(8).collect();
    (id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// 连着哪个后台任务：进程参数 `claude attach <任务>`，或交互进程登记文件中的 `parkedJobId`
/// （按左方向键转入后台后，原终端里的交互进程记下它停靠的任务）。
fn attached(args: &[String], registry: Option<&Value>) -> Option<String> {
    let from_args = args
        .windows(2)
        .find(|w| w[0] == "attach")
        .map(|w| w[1].clone());
    from_args.or_else(|| registry?["parkedJobId"].as_str().map(str::to_owned))
}

/// 登记文件 `~/.claude/sessions/<pid>.json` 中承载后台任务的进程带 `jobId`；预启动的备用会话（`spare: true`）
/// 还没有对话，不算（2026-10-12 取证，docs/s0-hook-compat.md）。
fn hosts_job(registry: &Value) -> Option<String> {
    if registry["spare"].as_bool() == Some(true) {
        return None;
    }
    registry["jobId"].as_str().map(str::to_owned)
}

/// npm 全局安装（含 nvm）：`…/@anthropic-ai/claude-code/bin/claude.exe`。
fn matches(p: &Process) -> bool {
    p.path
        .ends_with("/@anthropic-ai/claude-code/bin/claude.exe")
        || native_version(p.path).is_some()
}

/// 官方原生安装 `https://claude.ai/install.sh`（2026-11-08 虚拟机实测）：`~/.local/bin/claude` 指向
/// `~/.local/share/claude/versions/<版本>`，文件名就是版本号。
fn native_version(path: &str) -> Option<&str> {
    let (dir, name) = path.rsplit_once('/')?;
    (dir.ends_with("/.local/share/claude/versions") && name.starts_with(|c: char| c.is_ascii_digit()) && name.contains('.'))
        .then_some(name)
}

fn map_event(name: &str, notification_type: Option<&str>) -> Option<Kind> {
    Some(match name {
        "SessionStart" => Kind::SessionStart,
        "UserPromptSubmit" => Kind::PromptSubmit,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Kind::ToolActivity,
        "PermissionRequest" => Kind::PermissionWait,
        "Notification" => notification(notification_type),
        "Stop" => Kind::TurnEnd,
        "StopFailure" => Kind::TurnFail,
        "SessionEnd" => Kind::SessionEnd,
        _ => return None,
    })
}
