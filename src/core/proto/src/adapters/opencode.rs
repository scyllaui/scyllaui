//! OpenCode 适配器（M4b 块 9a）。依据：npm `@opencode-ai/sdk`、`@opencode-ai/plugin` 1.19.5 的类型定义，与 2026-10-29
//! 本机实测 1.19.5（`tests/hook-samples/opencode-1.19.5-smoke.jsonl`）。OpenCode 没有命令式 Hook，事件由 ScyllaUI 生成的
//! 插件（`hookcfg::opencode_plugin`）原样转发：`hook_event_name` 是事件名，其余是事件的 `properties`（OpenCode 1）或
//! `data`（OpenCode 2）。OpenCode 2（2026-10-31 起，依据 npm `@opencode/plugin`、`@opencode/schema` 2.2.4 的类型定义与
//! 本机实测 2.2.4）的事件名不同，同一张表里两版都认；它默认连共享后台，ScyllaUI 的终端里自动加 `--standalone`
//! （`scyllauid/shell/zsh/.zshenv`），插件才在这个终端的私有服务里运行、拿得到 `SCYLLAUI_*`。

use super::{Adapter, Fields, Process, TitleSource, VersionSource};
use crate::{Kind, Tool};

pub static ADAPTER: Adapter = Adapter {
    tool: Tool::OpenCode,
    command: "opencode",
    launch_args: "",
    preassign_id: false,
    // 插件转发的 `session.updated` 带 `info.title`。
    title_source: TitleSource::HookField,
    reply_source: super::reply::ReplySource::OpenCodeDb,
    model_source: super::usage::ModelSource::OpenCodeDb,
    usage_source: super::usage::UsageSource::OpenCodeDb,
    effort_source: super::usage::EffortSource::None,
    hosted: None,
    waiting_scan: None,
    scan_while_running: false,
    session_record: None,
    record_lookup: None,
    turn_end_by_record: false,
    pid_status: None,
    child_records: false,
    record_from_approval: false,
    // `permission.replied`（拒绝也是它，`reply: reject`）、`form.replied` / `form.cancelled`；2.3.0 子代理默认在后台跑，
    // 主会话一轮结束后子会话还可能要审批（2026-11-13 长跑）。
    subagent_approvals: super::SubagentApprovals { resolved_event: true, outlive_turn: true },
    matches,
    version,
    // `--help`：`-s, --session  session id to continue`。
    resume_args: |id| format!("-s {id}"),
    map_event,
    fields: Fields {
        session: &["sessionID"],
        turn: &[],
        // 细分值：`session.status` 的状态、`session.error` 的错误名。
        notification: &["status.type", "error.name"],
        // OpenCode 1 `session.updated` 的 `info.title`；OpenCode 2 `session.renamed` 的 `title`。
        title: &["info.title", "title"],
        transcript: &[],
        // 插件给子会话（`info.parentID` 非空）的事件加上的标记。
        subagent: &["scyllaui_subagent"],
        turn_scoped_end: false,
        forked: None,
        // OpenCode 1 `chat.message` 的 `model`；OpenCode 2 的提示词钩子不带模型，从它的库里读（`ModelSource::OpenCodeDb`）。
        model: &["model.modelID"],
        cwd: &["info.directory", "directory"],
        // 出错或中断时先发 `session.error`，紧跟着发 `session.idle`（2026-10-29 实测）。
        end_keeps_error: true,
        title_placeholder: Some("New session - "),
        prompt: &["prompt"],
        subagent_empty: None,
        subagent_when: None,
        injected_prompt: &[],
        prompt_origin: None,
        approval_id: &["id", "requestID", "permissionID"],
        approval_command: &[],
    },
};

/// npm 包：OpenCode 1 的 `bin/opencode` 指向 `opencode-ai/bin/opencode.exe`，它与平台包里的 `opencode` 是同一个文件的硬链接，
/// 系统报告的路径是平台包那个（2026-10-29 实测）；OpenCode 2 的包是 `@opencode/cli`，系统报告的是 `@opencode/cli/bin/opencode.exe`
/// （2026-10-31 实测 2.2.4；它的私有服务 `opencode.exe serve --stdio` 是同一个文件）。都认，平台包也认。
fn matches(p: &Process) -> bool {
    p.path.ends_with("/node_modules/opencode-darwin-arm64/bin/opencode")
        || p.path.ends_with("/node_modules/opencode-ai/bin/opencode.exe")
        || p.path.ends_with("/node_modules/@opencode/cli/bin/opencode.exe")
        || p.path.ends_with("/node_modules/@opencode/cli-darwin-arm64/bin/opencode")
        || p.path.ends_with(SCRIPT)
}

/// 官方安装脚本 `https://opencode.ai/v2/install`（2026-11-08 虚拟机实测 2.2.6）：单个可执行文件放在 `~/.opencode/bin`，没有 `package.json`。
const SCRIPT: &str = "/.opencode/bin/opencode";

/// npm 安装读进程所在包的 `package.json`（两个包的版本相同）；安装脚本装的执行 `opencode --version`。
fn version(p: &Process) -> Option<VersionSource> {
    if p.path.ends_with(SCRIPT) {
        return Some(VersionSource::Command { path: p.path.to_owned() });
    }
    let pkg = p.path.rsplit_once("/bin/")?.0;
    Some(VersionSource::PackageJson { path: format!("{pkg}/package.json"), strip: "" })
}

fn map_event(name: &str, detail: Option<&str>) -> Option<Kind> {
    Some(match name {
        // 子会话的创建带子代理标记，不会走到这里改状态。
        "session.created" => Kind::SessionStart,
        "chat.message" => Kind::PromptSubmit,
        "session.status" => match detail {
            Some("busy" | "retry") => Kind::ToolActivity,
            // 与 `session.idle` 重复，只记录。
            _ => Kind::Other,
        },
        // 审批事件新旧两个名字（v2 SDK 为 `permission.asked`，v1 为 `permission.updated`）；AI 提问算「需确认」
        // （OpenCode 1 为 `question.*`，OpenCode 2 为 `form.*`）。
        "permission.asked" | "permission.updated" | "question.asked" | "form.created" => Kind::PermissionWait,
        "permission.replied" | "question.replied" | "question.rejected" | "form.replied" | "form.cancelled" => Kind::PermissionDone,
        "session.idle" => Kind::TurnEnd,
        // OpenCode 2：提示词来自插件的 `prompt` 钩子；一轮以 `session.execution.*` 开始与结束，拒绝审批也以 `interrupted` 结束
        // （2026-10-31 实测 2.2.4，原因为 `shutdown`），旧的 `session.status`、`session.idle` 不再发。
        "session.prompt" => Kind::PromptSubmit,
        "session.execution.started" => Kind::ToolActivity,
        "session.execution.succeeded" => Kind::TurnEnd,
        "session.execution.failed" => Kind::TurnFail,
        "session.execution.interrupted" => Kind::TurnInterrupt,
        // 只带标题。
        "session.renamed" => Kind::Other,
        "session.error" => match detail {
            Some("MessageAbortedError") => Kind::TurnInterrupt,
            _ => Kind::TurnFail,
        },
        // 只带标题与目录。
        "session.updated" => Kind::Other,
        _ => return None,
    })
}
