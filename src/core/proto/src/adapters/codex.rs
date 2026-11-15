//! Codex 适配器。依据：S0 第 2、3 项实测（Codex 0.160.0 / 0.161.0）与 2026-10-10 本机安装路径（0.162.0）。

use super::{
    notification, valid_session_id, Adapter, Fields, Hosted, HostedMarker, Process, TitleSource,
    VersionSource,
};
use serde_json::Value;
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::Codex,
    command: "codex",
    // 不用共享守护进程：Hook 在终端里的 codex 进程内执行，带本终端的环境（2026-10-12 用户决定，docs/design.md 4.4 节）。
    launch_args: "--no-daemon",
    preassign_id: false,
    title_source: TitleSource::CodexSessionIndex,
    reply_source: super::reply::ReplySource::CodexRollout,
    model_source: super::usage::ModelSource::None,
    usage_source: super::usage::UsageSource::CodexRollout,
    effort_source: super::usage::EffortSource::CodexRollout,
    // 0.157 起 Hook 由共享守护进程 `codex app-server` 执行，带第一次拉起它的终端的环境（2026-10-12 取证，
    // docs/s0-hook-compat.md）。没有登记文件，只能从 `resume <对话>` 参数直接认出，其余靠推断。
    hosted: Some(Hosted {
        marker: HostedMarker::AncestorArg("app-server"),
        registry_dir: None,
        job_of: |id| valid_session_id(id).then(|| id.to_owned()),
        attached,
        retry: false,
        infer: true,
        hosts_job: |_| None,
        reattach_args: None,
        jobs_dir: None,
        job_session: |_| None,
    }),
    // 拒绝子代理的审批不发任何 Hook：待审批期间读这次审批指向的 rollout 记录里的 `turn_aborted`（`waiting::codex`，2026-11-14）。
    waiting_scan: Some(super::waiting::Scan::Lines(super::waiting::codex)),
    scan_while_running: false,
    session_record: None,
    record_lookup: None,
    turns_by_record: false,
    pid_status: None,
    child_records: false,
    record_from_approval: true,
    subagent_approvals: super::SubagentApprovals { resolved_event: false, outlive_turn: false },
    matches,
    version,
    // 恢复由 ScyllaUI 代输，有新版时 Codex 会停在自己的升级选择上、等人选（2026-10-29 长跑）；用户自己输入
    // `codex` 时照常检查更新。其余各款的更新都在后台进行、不拦界面，不需要这一项（docs/design.md 第 3 节工具表下「恢复时的升级提示」）。
    resume_args: |id| format!("resume --no-daemon -c check_for_update_on_startup=false {id}"),
    map_event,
    fields: Fields {
        session: &["session_id"],
        turn: &["turn_id"],
        notification: &["notification_type"],
        title: &[],
        transcript: &["transcript_path"],
        subagent: &["agent_id"],
        turn_scoped_end: false,
        forked: None,
        model: &["model"],
        cwd: &["cwd"],
        end_keeps_error: false,
        title_placeholder: None,
        prompt: &["prompt"],
        subagent_empty: None,
        injected_prompt: &[],
        approval_id: &[],
        approval_command: &["tool_input.command"],
    },
};

/// `codex` 命令是 Node 脚本 `@openai/codex/bin/codex.js`，它不带 detached 启动原生 `codex`，两者同在前台进程组：
/// 首进程为 `node`（按脚本的真实路径识别），或组内的原生子进程。官方独立安装包另认，见 `standalone`。
fn matches(p: &Process) -> bool {
    p.path
        .ends_with("/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex")
        || p.args
            .get(1)
            .is_some_and(|script| script.ends_with("/@openai/codex/bin/codex.js"))
        || standalone(p.path).is_some()
}

const NATIVE: &str = "/vendor/aarch64-apple-darwin/bin/codex";

/// 官方独立安装包（2026-10-31 本机 0.166.0）：`~/.local/bin/codex` 指向 `~/.codex/packages/standalone/current/bin/codex`，
/// 系统报告的是 `releases/<版本>-<平台>/bin/codex`；版本在同一目录的 `codex-package.json`。返回这个目录。
/// ChatGPT 等共享后台的 `packages/app-server-daemon/…/bin/codex` 不是终端前台的 Codex，不认。
fn standalone(path: &str) -> Option<&str> {
    path.strip_suffix("/bin/codex").filter(|root| root.contains("/packages/standalone/"))
}

/// Node 包装进程：`@openai/codex/package.json`；原生子进程：所在平台包的 `package.json`（版本带 `-darwin-arm64` 后缀）。
fn version(p: &Process) -> Option<VersionSource> {
    if let Some(script) = p.args.get(1).filter(|s| s.ends_with("/@openai/codex/bin/codex.js")) {
        let root = script.strip_suffix("bin/codex.js")?;
        return Some(VersionSource::PackageJson { path: format!("{root}package.json"), strip: "" });
    }
    if let Some(root) = standalone(p.path) {
        return Some(VersionSource::PackageJson { path: format!("{root}/codex-package.json"), strip: "" });
    }
    let root = p.path.strip_suffix(NATIVE)?;
    Some(VersionSource::PackageJson { path: format!("{root}/package.json"), strip: "-darwin-arm64" })
}

fn map_event(name: &str, notification_type: Option<&str>) -> Option<Kind> {
    Some(match name {
        "SessionStart" => Kind::SessionStart,
        "UserPromptSubmit" => Kind::PromptSubmit,
        "PreToolUse" | "PostToolUse" => Kind::ToolActivity,
        "PermissionRequest" => Kind::PermissionWait,
        "Notification" => notification(notification_type),
        "Stop" => Kind::TurnEnd,
        "StopFailure" => Kind::TurnFail,
        "Interrupt" => Kind::TurnInterrupt,
        "SessionEnd" => Kind::SessionEnd,
        _ => return None,
    })
}

/// 终端前台的 `codex resume [选项] <对话>`（node 包装进程与原生子进程的参数里都有）。对话 ID 是 UUID；
/// `resume` 之后取第一个 UUID 形的参数，跳过 `--no-daemon`、`-m <模型>` 等选项与提示词。
fn attached(args: &[String], _registry: Option<&Value>) -> Option<String> {
    let i = args.iter().position(|a| a == "resume")?;
    args[i + 1..].iter().find(|a| is_uuid(a)).cloned()
}

/// 8-4-4-4-12 位十六进制。
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}
