//! 工具适配器（docs/design.md 3.1 节）：每款 CLI 一份，状态机与界面只面对这里给出的统一事件与能力，
//! 不出现针对某个工具的分支。本模块只含纯逻辑；读取进程信息的系统调用在 scyllauid 的 procinfo.rs。

mod antigravity;
mod claude;
mod codex;
mod copilot;
mod grok;
mod kimi;
mod opencode;
pub mod prompts;
pub mod reply;
pub mod title;
pub mod usage;
pub mod waiting;

use crate::{HookEvent, Kind, Tool};
use serde_json::Value;

/// 会话标题从哪里来；文件类来源由 scyllauid 的 `title.rs` 读取，解析见 `title` 模块（M1 第 5 块）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSource {
    /// transcript 文件中的 `ai-title` / `custom-title` 记录（Claude Code，格式非官方承诺）。
    TranscriptRecords,
    /// `~/.codex/session_index.jsonl` 的 `thread_name`（格式非官方承诺）。
    CodexSessionIndex,
    /// 每个 Hook 都带的 `session_title`（Kimi）。
    HookField,
    /// `~/.grok/sessions/<URL 编码的启动目录>/<ID>/summary.json` 的 `generated_title`，没有时取非空的
    /// `session_summary`（2026-10-10 核对 Grok 1.0.50，2026-10-11 核对全部本机会话）。
    GrokSummary,
    /// `~/.gemini/antigravity-cli/conversation_summaries.db`（SQLite）中该对话的 `title`；agy 在第一轮之后生成
    /// （2026-11-01 本机 1.4.12 只读核对，表结构非官方承诺）。
    AntigravitySummaries,
}

/// 前台进程组中的一个进程：真实路径（已解析符号链接）与参数（`args[0]` 为 argv0；
/// 参数里的绝对路径由调用方解析为真实路径）。
pub struct Process<'a> {
    pub path: &'a str,
    pub args: &'a [String],
}

pub struct Adapter {
    pub tool: Tool,
    /// 启动命令名，在用户的 Shell 中按 PATH 解析。
    pub command: &'static str,
    /// 新建会话时跟在命令后的参数（恢复时的参数由 `resume_args` 给出）。Codex 为 `--no-daemon`
    /// （2026-10-12 用户决定，docs/design.md 4.4 节），其余为空。
    pub launch_args: &'static str,
    /// 能否以 `--session-id <UUID>` 预先指定新会话 ID（2026-10-08 S0 实测）。
    pub preassign_id: bool,
    pub title_source: TitleSource,
    /// 最后一条 AI 回复从哪里读（M1 第 6 块，`reply` 模块）。
    pub reply_source: reply::ReplySource,
    /// 会话记录里模型从哪里读（M2 第 2 块，`usage` 模块）。
    pub model_source: usage::ModelSource,
    /// 会话记录里 token 与周额度从哪里读（M2 第 3 块，`usage` 模块）。
    pub usage_source: usage::UsageSource,
    /// 会话记录里推理强度从哪里读（维护期 0.3.5，`usage` 模块）。
    pub effort_source: usage::EffortSource,
    /// CLI 自己的守护进程托管的会话（docs/design.md 4.4 节）：这类 Hook 的 `SCYLLAUI_PANE_ID` 不可信，
    /// 由 scyllauid 反查连着它的终端。没有这种机制的工具为 None。
    pub hosted: Option<Hosted>,
    /// 读会话记录、认出 Hook 之外发生的事（M2 第 1 块，`waiting` 模块）：Claude 的拒绝审批，Copilot 的批准、拒绝与中断；
    /// 不需要的工具为 None。默认只在待审批期间看。
    pub waiting_scan: Option<waiting::Scan>,
    /// 运行中也看会话记录（Copilot：Esc 中断不发 Hook，M4b 块 9c）。
    pub scan_while_running: bool,
    /// 会话记录的位置（相对家目录），由会话 ID 推出；Hook 不给路径、或要在第一轮结束前就开始看时用（Copilot）。
    pub session_record: Option<fn(&str) -> String>,
    /// 会话记录的位置拼不出来、要另查时怎么查（监听会话记录用，在监听线程里查）；`session_record` 优先。
    pub record_lookup: Option<RecordLookup>,
    /// 轮次的开始与结束以会话记录为准（Kimi，docs/design.md 3.1 节）：会话还在时一直监听记录，记录里新开的一轮
    /// 也算开始（Kimi 的插话、排队消息有时另开一轮而不发提交 Hook）；监听确认已打开时，Hook 报的一轮结束、出错、
    /// 中断不改状态，等记录里的。记录定位不到或打不开时照 Hook 处理。提交 Hook 不带轮次号时以后台收到的时刻作标识。
    pub turns_by_record: bool,
    /// CLI 进程自己的状态文件（Claude，待审批期间监听）：`<家目录>/<目录>/<前台 AI 的进程号>.json`，整份交给判定函数。
    pub pid_status: Option<(&'static str, waiting::LineScan)>,
    /// 子代理有自己的会话记录、它的等待只写在那里（Antigravity：子代理自己的对话库）：核心从子代理的 Hook 得到它的
    /// 会话 ID，按 `session_record` 另开监听，认出的事件按这个子代理的事件处理。
    pub child_records: bool,
    /// 待审批期间监听的是「需确认」那个事件给出的记录路径（Codex：子代理的审批指向子线程的 rollout），不是会话自己的。
    pub record_from_approval: bool,
    /// 子代理的审批怎样算处理完（docs/design.md 4.3 节「子代理事件」例外）。
    pub subagent_approvals: SubagentApprovals,
    matches: fn(&Process) -> bool,
    /// 版本从哪里读（M1 第 7 块，docs/design.md 3.1 节「版本来源」）；参数为被识别出的那个进程。
    version: fn(&Process) -> Option<VersionSource>,
    resume_args: fn(&str) -> String,
    /// 原生事件名 → 统一事件；认不出的事件名为 None（`normalize` 按 `Kind::Other` 记录，`anomaly` 据此报异常）。
    map_event: fn(&str, Option<&str>) -> Option<Kind>,
    fields: Fields,
}

/// 会话记录另查的办法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordLookup {
    /// Kimi：`~/.kimi-code/session_index.jsonl` 中该会话最后一行的 `sessionDir`，其下的 `agents/main/wire.jsonl`。
    KimiIndex,
    /// Grok：`~/.grok/sessions/<URL 编码的启动目录>/<会话 ID>/events.jsonl`，在各启动目录下找这个会话。
    GrokSessions,
}

/// 子代理审批的两项能力（docs/design.md 4.3 节「子代理事件」例外），状态机只按它们走，不按工具分支。
#[derive(Clone, Copy, Debug, Default)]
pub struct SubagentApprovals {
    /// 有「审批已处理」事件（Kimi `PermissionResult`、OpenCode `permission.replied` 等）：只有它算处理完。
    /// 没有的（Claude、Codex 只有 PermissionRequest）：同一子代理之后又有动静（工具调用、一轮结束）即算处理完，
    /// 主会话一轮结束时还在等的一并清掉。
    pub resolved_event: bool,
    /// 子代理可能比主会话这一轮活得久（OpenCode 2.3.0 默认在后台跑子代理）：主会话一轮结束不清掉还在等的审批。
    pub outlive_turn: bool,
}

/// CLI 版本的来源（docs/design.md 3.1 节「版本来源」）。读取与执行在 scyllauid；这里只给出位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionSource {
    /// 读该 `package.json` 的 `version`，去掉后缀 `strip`（Codex 原生包为 `-darwin-arm64`）。
    PackageJson { path: String, strip: &'static str },
    /// 版本已知（如可执行文件名里带版本）；`file` 用来判断进程启动后是否被替换。
    Known { version: String, file: String },
    /// 执行 `<path> --version`，用 `parse_version_output` 解析。
    Command { path: String },
}

/// `--version` 的输出中第一个以数字开头、含点号的词：`2.1.300 (Claude Code)`、`codex-cli 0.162.1`、`2.2.0`、
/// `grok 1.0.52 (0920c8169e27)`（2026-10-15 实测）；前面的 `v` 去掉：`opencode v2.2.5`（2026-11-08 实测）。
pub fn parse_version_output(out: &str) -> Option<String> {
    out.split_whitespace()
        .map(|w| w.strip_prefix('v').unwrap_or(w))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains('.'))
        .map(|w| w.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()).to_owned())
}

/// `package.json` 的 `version`，去掉后缀。
pub fn parse_package_version(text: &str, strip: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let ver = v["version"].as_str()?;
    let ver = ver.strip_suffix(strip).unwrap_or(ver);
    (!ver.is_empty()).then(|| ver.to_owned())
}

/// 已验证版本清单（`proto/verified-versions.json`，编译时嵌入）：按版本号精确匹配。
pub fn verified(tool: Tool, version: &str) -> bool {
    verified_list(tool).iter().any(|v| v == version)
}

pub fn verified_list(tool: Tool) -> Vec<String> {
    static LIST: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| serde_json::from_str(include_str!("../../verified-versions.json")).expect("verified-versions.json"));
    list[tool.as_str()]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["version"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// 守护进程托管的会话如何识别、如何找到连着它的终端（2026-10-12 Claude Code 2.1.296 取证，
/// docs/s0-hook-compat.md「Claude Code 2.1.296 后台会话」）。格式均非官方承诺。
pub struct Hosted {
    /// 怎样认出 Hook 来自守护进程。
    pub marker: HostedMarker,
    /// 进程登记文件目录（相对家目录），文件名为 `<pid>.json`；没有登记文件的工具为 None。
    pub registry_dir: Option<&'static str>,
    /// 会话 ID → 任务 ID（连接方用来指称后台会话的 ID）。
    pub job_of: fn(&str) -> Option<String>,
    /// 终端前台的一个进程正连着哪个任务：看进程参数与它的登记文件（没有登记文件时为 None）。
    pub attached: fn(&[String], Option<&Value>) -> Option<String>,
    /// 找不到连着它的终端时稍后重试（Claude：转入后台时登记文件可能晚于 SessionStart 写好）。
    pub retry: bool,
    /// 找不到时按「已绑定的终端 → 当前目录唯一的终端 → 用户正在看的终端」推断（Codex：没有登记文件，
    /// docs/design.md 4.4 节）。
    pub infer: bool,
    /// 登记文件里的进程承载着哪个后台任务（Claude：`jobId`，预启动的备用会话除外）；没有则 None。
    pub hosts_job: fn(&Value) -> Option<String>,
    /// 后台任务的接回参数（跟在命令后，Claude：`attach <任务>`）；没有这种命令的工具为 None，
    /// 一律用 `resume()`（docs/design.md 第 5 节「实现（M1 第 6 块）」）。
    pub reattach_args: Option<fn(&str) -> String>,
    /// 后台任务记录目录（相对家目录），每个任务一个子目录，其中 `state.json` 记着会话 ID；停止了的任务也在
    /// （Claude：`.claude/jobs`，2026-10-15 实测停止后 `--resume` 打不开、`attach` 可以）。没有则为 None。
    pub jobs_dir: Option<&'static str>,
    /// 后台任务记录 `state.json` 里的会话 ID。
    pub job_session: fn(&Value) -> Option<String>,
}

pub enum HostedMarker {
    /// Hook 环境中有这个变量（Claude：`CLAUDE_JOB_DIR`）。
    Env(&'static str),
    /// 最多 4 层上级进程中有参数等于它（Codex：`app-server`）。
    AncestorArg(&'static str),
}

/// `scyllaui hook` 查上级进程的层数。
pub const ANCESTOR_LEVELS: usize = 4;

/// 各款 CLI 载荷中的字段名，按顺序取第一个存在的。
struct Fields {
    session: &'static [&'static str],
    turn: &'static [&'static str],
    notification: &'static [&'static str],
    title: &'static [&'static str],
    transcript: &'static [&'static str],
    /// 子代理事件的标记字段；值为 null 或 `"main"`（Kimi 主代理）时不算子代理。
    subagent: &'static [&'static str],
    /// 轮次结束事件需与最近一次提交属于同一轮才生效（Grok：有阻塞型 Stop Hook 时会多次触发）。
    turn_scoped_end: bool,
    /// SessionStart 载荷中表示「接替上一段对话」的（字段, 取值）；Claude 的 `source=fork`（转入后台继续）。
    forked: Option<(&'static str, &'static str)>,
    /// 当前模型（M2 第 2 块）：Codex 每个事件都带，Claude、Kimi 只在 SessionStart 带（Claude 官方说明不总是有）。
    model: &'static [&'static str],
    /// 工作目录；四款为 `cwd`，OpenCode 为会话信息里的 `info.directory`，没有时取插件补上的 `directory`。
    cwd: &'static [&'static str],
    /// 出错之后照发轮次结束（见 `HookEvent::keeps_error`）。
    end_keeps_error: bool,
    /// 以此开头的标题是 CLI 生成标题之前的占位，不算标题（OpenCode：`New session - <时间>`，2026-10-29 实测 1.19.5）。
    title_placeholder: Option<&'static str>,
    /// 提交的提示词；六款为 `prompt`，Antigravity 为每个事件都带的 `lastUserInput`。值可以是字符串，也可以是内容块
    /// 数组（Kimi：`[{"type":"text","text":…}]`），后者取各文字块拼接。
    prompt: &'static [&'static str],
    /// 这个字段存在且为空字符串时算子代理（Antigravity：子代理的事件 `lastUserInput` 为空，2026-11-01 实测 1.4.12）。
    subagent_empty: Option<&'static str>,
    /// 以此开头的提交内容是 CLI 自己投递的，不是用户输入的提示词：事件照常改状态，但不记为提示词、不当标题
    /// （Claude：子代理交回结果时以 `<agent-message from="…">` 开头的排队消息也触发 UserPromptSubmit，2026-11-05 实测 2.1.318）。
    injected_prompt: &'static [&'static str],
    /// 一次审批的 ID：「需确认」与「审批已处理」按它配对，同一子代理同时有几个审批时不提前离开（Kimi `id`；
    /// OpenCode 2 `permission.asked` 的 `id` 与 `permission.replied` 的 `requestID`，提问为 `id`）。没有的为空。
    approval_id: &'static [&'static str],
    /// 「需确认」载荷里待批准的命令原文（Codex 的 `tool_input.command`）：批准后 CLI 的子进程里出现参数末项与它逐字
    /// 相等的进程，核心据此认出批准（docs/design.md 4.3 节）。没有这种信号的工具为空。
    approval_command: &'static [&'static str],
}

pub static ADAPTERS: [&Adapter; 7] = [
    &claude::ADAPTER,
    &codex::ADAPTER,
    &kimi::ADAPTER,
    &grok::ADAPTER,
    &opencode::ADAPTER,
    &copilot::ADAPTER,
    &antigravity::ADAPTER,
];

pub fn adapter(tool: Tool) -> &'static Adapter {
    ADAPTERS.iter().find(|a| a.tool == tool).copied().unwrap()
}

/// 前台进程组属于哪款 CLI：任一进程被某个适配器识别即是。
pub fn identify(procs: &[Process]) -> Option<Tool> {
    ADAPTERS
        .iter()
        .find(|a| procs.iter().any(|p| (a.matches)(p)))
        .map(|a| a.tool)
}

/// 同 `identify`，另给出被识别出的那个进程（读版本用）。
pub fn identify_proc<'a>(procs: &'a [Process<'a>]) -> Option<(Tool, &'a Process<'a>)> {
    ADAPTERS
        .iter()
        .find_map(|a| procs.iter().find(|p| (a.matches)(p)).map(|p| (a.tool, p)))
}

/// 会话 ID 只允许这些字符：直接拼进 Shell 命令，不做转义。
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

impl Adapter {
    /// 新会话的启动命令；`preassigned` 只在 `preassign_id` 为真时使用。
    pub fn launch(&self, preassigned: Option<&str>) -> Option<String> {
        let command = match self.launch_args {
            "" => self.command.to_owned(),
            args => format!("{} {args}", self.command),
        };
        match preassigned.filter(|_| self.preassign_id) {
            Some(id) if !valid_session_id(id) => None,
            Some(id) => Some(format!("{command} --session-id {id}")),
            None => Some(command),
        }
    }

    pub fn version_source(&self, p: &Process) -> Option<VersionSource> {
        (self.version)(p)
    }

    /// 读得到模型：Hook 载荷带，或会话记录里有（Copilot 两者都没有，2026-10-30）。
    pub fn has_model(&self) -> bool {
        !self.fields.model.is_empty() || self.model_source != usage::ModelSource::None
    }

    pub fn resume(&self, native_id: &str) -> Option<String> {
        valid_session_id(native_id)
            .then(|| format!("{} {}", self.command, (self.resume_args)(native_id)))
    }

    /// 接回仍在运行的后台任务的命令；工具没有这种命令或任务 ID 不合规则时为 None。
    pub fn reattach(&self, job: &str) -> Option<String> {
        let args = self.hosted.as_ref()?.reattach_args?;
        valid_session_id(job).then(|| format!("{} {}", self.command, args(job)))
    }

    /// Hook 来自 CLI 守护进程托管的会话（环境报告中有标记变量）。
    /// 按环境报告判断（帧中没有 `hosted` 字段时用，只能判断按环境变量识别的工具）。
    pub fn is_hosted(&self, env: &Value) -> bool {
        match self.hosted.as_ref().map(|h| &h.marker) {
            Some(HostedMarker::Env(name)) => env.get(*name).is_some(),
            _ => false,
        }
    }

    /// `scyllaui hook` 用：按自身环境与上级进程参数判断，结果作为帧的 `hosted` 字段（docs/protocol.md 第 6 节）。
    pub fn hosted_in(&self, has_env: impl Fn(&str) -> bool, ancestors: &[Vec<String>]) -> bool {
        match self.hosted.as_ref().map(|h| &h.marker) {
            Some(HostedMarker::Env(name)) => has_env(name),
            Some(HostedMarker::AncestorArg(arg)) => ancestors.iter().any(|a| a.iter().any(|x| x == arg)),
            None => false,
        }
    }

    /// 把一份原始 Hook 载荷规范化；没有事件名时返回 None。
    pub fn normalize(&self, payload: &Value) -> Option<HookEvent> {
        let f = &self.fields;
        let name = str_field(payload, &["hook_event_name"])?;
        let notification_type = str_field(payload, f.notification);
        let kind = (self.map_event)(&name, notification_type.as_deref()).unwrap_or(Kind::Other);
        let session_id = str_field(payload, f.session);
        let marker = f.subagent.iter().find_map(|k| payload.get(*k).filter(|v| !v.is_null() && *v != "main"));
        let subagent = marker.is_some() || f.subagent_empty.is_some_and(|k| payload.get(k).and_then(Value::as_str) == Some(""));
        Some(HookEvent {
            turn_scoped: f.turn_scoped_end && kind == Kind::TurnEnd,
            keeps_error: f.end_keeps_error && kind == Kind::TurnEnd,
            kind,
            prompt_id: str_field(payload, f.turn),
            notification_type,
            title: str_field(payload, f.title).filter(|t| f.title_placeholder.is_none_or(|p| !t.starts_with(p))),
            transcript_path: str_field(payload, f.transcript),
            cwd: str_field(payload, f.cwd),
            model: str_field(payload, f.model),
            prompt: (kind == Kind::PromptSubmit)
                .then(|| str_field(payload, f.prompt).or_else(|| text_parts(payload, f.prompt)))
                .flatten()
                .filter(|p| !f.injected_prompt.iter().any(|x| p.starts_with(x))),
            // 区分各个子代理（记还在等的审批用）：标记字段的取值（Kimi、Claude 的 `agent_id`，Copilot 的 `agentId`）；
            // 标记不是字符串（OpenCode 插件的 `scyllaui_subagent: true`）或按空字段认出的（Antigravity）用它自己的会话 ID。
            agent_key: subagent
                .then(|| marker.and_then(Value::as_str).map(str::to_owned).or_else(|| session_id.clone()))
                .flatten(),
            subagent,
            session_id,
            command: (kind == Kind::PermissionWait).then(|| str_field(payload, f.approval_command)).flatten(),
            approval_id: matches!(kind, Kind::PermissionWait | Kind::PermissionDone)
                .then(|| str_field(payload, f.approval_id))
                .flatten(),
            // CLI 自己起的后台会话：载荷显式给出 `transcript_path: null`（2026-10-08 S0 实测 Codex 0.161.0 记忆整理会话）。
            background: f
                .transcript
                .iter()
                .any(|k| payload.get(*k).is_some_and(Value::is_null)),
            forked: kind == Kind::SessionStart
                && f.forked.is_some_and(|(k, v)| payload.get(k).and_then(Value::as_str) == Some(v)),
            all_approvals: false,
            name,
        })
    }

    /// 这份载荷是否说明侧栏的状态可能不准（M4b 块 6，docs/design.md 10.6 节 6a）：只认三类真实异常，版本不在
    /// 清单里不算。返回原因代码（App 显示为文字）：`unparsable`、`missing_event_name`、`unknown_event:<名>`、
    /// `missing_session:<事件>`；事件名过长时截断。
    pub fn anomaly(&self, payload: &Value) -> Option<String> {
        if !payload.is_object() {
            return Some("unparsable".into());
        }
        let Some(name) = str_field(payload, &["hook_event_name"]) else {
            return Some("missing_event_name".into());
        };
        let name: String = name.chars().take(64).collect();
        if (self.map_event)(&name, None).is_none() {
            return Some(format!("unknown_event:{name}"));
        }
        if str_field(payload, self.fields.session).is_none() {
            return Some(format!("missing_session:{name}"));
        }
        None
    }
}

/// 取第一个存在的字段，字符串原样返回，数字与布尔值转成字符串（Kimi 2.1.1 的 `turn_id` 是整数，Copilot 的 `recoverable`）。字段名中的点号表示嵌套
/// （OpenCode 的 `info.title`、`error.name`），数字表示数组下标（Antigravity 的 `workspacePaths.0`）。
fn str_field(p: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| match k.split('.').try_fold(p, |v, part| match (v, part.parse::<usize>()) {
            (Value::Array(a), Ok(i)) => a.get(i),
            _ => v.get(part),
        })? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

/// 内容块数组里的文字（`[{"type":"text","text":…}, …]`，图片等其他块不算），多块之间换行拼接；没有文字为 None。
fn text_parts(p: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| p.get(*k)?.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty())
}

/// 四款共有的 Notification 映射：等待审批、空闲提示，其余不改变状态。
fn notification(kind: Option<&str>) -> Kind {
    match kind {
        Some("permission_prompt") => Kind::PermissionWait,
        Some("idle_prompt") => Kind::Idle,
        _ => Kind::Other,
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn id_of(path: &str, a: &[&str]) -> Option<Tool> {
        let a = args(a);
        identify(&[Process { path, args: &a }])
    }

    /// Codex 共享守护进程执行的 Hook（2026-10-12 取证，docs/design.md 4.4 节）：按上级进程参数识别。
    #[test]
    fn codex_hosted_by_ancestor_args() {
        let c = adapter(Tool::Codex);
        let no_env = |_: &str| false;
        let daemon = vec![args(&["sh", "-c", "…"]), args(&["/x/codex", "app-server", "--remote-control", "--listen", "unix://"])];
        assert!(c.hosted_in(no_env, &daemon));
        let tui = vec![args(&["sh", "-c", "…"]), args(&["/x/codex"]), args(&["node", "/x/bin/codex"])];
        assert!(!c.hosted_in(no_env, &tui), "--no-daemon 或旧版本：Hook 由终端里的 codex 执行");
        assert!(!c.is_hosted(&json!({"CLAUDE_JOB_DIR": "<set>"})), "环境报告判断不了 Codex");
        let h = c.hosted.as_ref().unwrap();
        assert!(!h.retry && h.infer && h.registry_dir.is_none());
        assert_eq!((h.job_of)("01a12bf8-6d99-7b31-b586-59803593bd1f").as_deref(), Some("01a12bf8-6d99-7b31-b586-59803593bd1f"));
        let id = "01a12bf8-6d99-7b31-b586-59803593bd1f";
        assert_eq!((h.attached)(&args(&["node", "/x/bin/codex", "resume", id]), None).as_deref(), Some(id));
        // ScyllaUI 的恢复命令带 `--no-daemon`，用户还可能带 `-m <模型>` 等选项：只认 UUID 形的会话 ID。
        assert_eq!((h.attached)(&args(&["/x/codex", "resume", "--no-daemon", id]), None).as_deref(), Some(id));
        assert_eq!(
            (h.attached)(&args(&["/x/codex", "resume", "--no-daemon", "-c", "check_for_update_on_startup=false", id]), None)
                .as_deref(),
            Some(id)
        );
        assert_eq!((h.attached)(&args(&["/x/codex", "resume", "-m", "gpt-5", id, "继续"]), None).as_deref(), Some(id));
        assert_eq!((h.attached)(&args(&["node", "/x/bin/codex"]), None), None);
        assert_eq!((h.attached)(&args(&["node", "/x/bin/codex", "resume", "--last"]), None), None);
        assert_eq!(c.reattach(id), None, "Codex 没有接回后台任务的命令");
        // Claude 仍按环境变量识别，不看上级进程。
        let cl = adapter(Tool::Claude);
        assert!(cl.hosted_in(|k| k == "CLAUDE_JOB_DIR", &[]));
        assert!(!cl.hosted_in(no_env, &daemon));
    }

    /// Claude 守护进程托管的会话（2026-10-12 取证，docs/design.md 4.4 节）。
    #[test]
    fn claude_hosted_sessions() {
        let c = adapter(Tool::Claude);
        assert!(c.is_hosted(&json!({"CLAUDE_JOB_DIR": "<set>", "SCYLLAUI_PANE_ID": "x"})));
        assert!(!c.is_hosted(&json!({"SCYLLAUI_PANE_ID": "x"})), "直接在终端里运行的交互会话");
        assert!(!adapter(Tool::Codex).is_hosted(&json!({"CLAUDE_JOB_DIR": "<set>"})), "其他工具没有这种机制");
        let h = c.hosted.as_ref().unwrap();
        assert_eq!((h.job_of)("ee4afd71-c942-453f-a45b-f3e09a0fc8cf").as_deref(), Some("ee4afd71"));
        assert_eq!((h.job_of)("abc"), None);
        assert_eq!((h.attached)(&args(&["claude", "attach", "11375847"]), None).as_deref(), Some("11375847"));
        let reg = json!({"sessionId": "806715e8-9687", "kind": "interactive", "parkedJobId": "ee4afd71"});
        assert_eq!((h.attached)(&args(&["claude", "--session-id", "806715e8"]), Some(&reg)).as_deref(), Some("ee4afd71"));
        assert_eq!((h.attached)(&args(&["claude"]), Some(&json!({"kind": "interactive"}))), None, "没有停靠任务");
        // 登记文件：后台任务的进程带 jobId；备用会话（spare）与交互进程不算（2026-10-12 取证）。
        assert_eq!((h.hosts_job)(&json!({"sessionId": "ee4afd71-c942", "jobId": "ee4afd71"})).as_deref(), Some("ee4afd71"));
        assert_eq!((h.hosts_job)(&json!({"jobId": "05da4a74", "spare": true})), None);
        assert_eq!((h.hosts_job)(&json!({"sessionId": "806715e8", "kind": "interactive"})), None);
        // 还活着的后台任务用 attach 接回（`claude --help` 的 stop 条：停止后 --resume 才可用）。
        assert_eq!(c.reattach("ee4afd71").as_deref(), Some("claude attach ee4afd71"));
        assert_eq!(c.reattach("a; rm"), None);
        // SessionStart 的 source=fork 即接替上一段对话。
        let fork = c.normalize(&json!({"hook_event_name":"SessionStart","session_id":"ee4afd71-c942","source":"fork"})).unwrap();
        assert!(fork.forked);
        let start = c.normalize(&json!({"hook_event_name":"SessionStart","session_id":"x","source":"startup"})).unwrap();
        assert!(!start.forked);
    }

    /// 2026-10-10 本机实测的真实路径（docs/design.md 3.1 节）。
    #[test]
    fn identifies_measured_install_paths() {
        let nvm = "/Users/u/.nvm/versions/node/v24.14.1/lib/node_modules";
        assert_eq!(
            id_of(
                &format!("{nvm}/@anthropic-ai/claude-code/bin/claude.exe"),
                &["claude"]
            ),
            Some(Tool::Claude)
        );
        assert_eq!(
            id_of(
                &format!("{nvm}/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"),
                &["codex"]
            ),
            Some(Tool::Codex)
        );
        // 首进程是 node 时按脚本的真实路径识别。
        assert_eq!(
            id_of(
                "/Users/u/.nvm/versions/node/v24.14.1/bin/node",
                &["node", &format!("{nvm}/@openai/codex/bin/codex.js")]
            ),
            Some(Tool::Codex)
        );
        assert_eq!(
            id_of("/Users/u/.kimi-code/bin/kimi", &["kimi"]),
            Some(Tool::Kimi)
        );
        assert_eq!(
            id_of(
                "/Users/u/.grok/downloads/grok-1.0.50-macos-aarch64",
                &["grok"]
            ),
            Some(Tool::Grok)
        );
        assert_eq!(
            id_of("/Users/u/.grok/downloads/grok-macos-aarch64", &["agent"]),
            Some(Tool::Grok)
        );
    }

    #[test]
    fn does_not_guess_from_names() {
        assert_eq!(id_of("/bin/zsh", &["-zsh"]), None);
        assert_eq!(
            id_of("/usr/local/bin/claude", &["claude"]),
            None,
            "同名的其他程序"
        );
        assert_eq!(
            id_of("/opt/tools/agent", &["agent"]),
            None,
            "agent 可能是其他工具"
        );
        assert_eq!(id_of("/usr/bin/node", &["node", "/srv/app/codex.js"]), None);
        assert_eq!(
            id_of("/Users/u/.grok/downloads/grok-1.0.50-linux", &["grok"]),
            None
        );
    }

    #[test]
    fn launch_and_resume_commands() {
        let id = "11111111-2222-4333-8444-555555555555";
        let c = adapter(Tool::Claude);
        assert_eq!(
            c.launch(Some(id)).unwrap(),
            format!("claude --session-id {id}")
        );
        assert_eq!(c.resume(id).unwrap(), format!("claude --resume {id}"));
        assert_eq!(
            adapter(Tool::Grok).launch(Some(id)).unwrap(),
            format!("grok --session-id {id}")
        );
        assert_eq!(
            adapter(Tool::Grok).resume(id).unwrap(),
            format!("grok --resume {id}")
        );
        let codex = adapter(Tool::Codex);
        assert!(!codex.preassign_id);
        assert_eq!(
            codex.launch(Some(id)).unwrap(),
            "codex --no-daemon",
            "不支持预分配时忽略；不用共享守护进程"
        );
        assert_eq!(
            codex.resume(id).unwrap(),
            format!("codex resume --no-daemon -c check_for_update_on_startup=false {id}"),
            "恢复时不停在 Codex 自己的升级提示上"
        );
        assert_eq!(
            adapter(Tool::Kimi).resume("session_61f5a8db").unwrap(),
            "kimi -r session_61f5a8db"
        );
        assert_eq!(c.resume("a; rm -rf ~"), None, "不合规则的 ID 拒绝生成命令");
        assert_eq!(c.launch(Some("$(x)")), None);
        assert!(!valid_session_id(&"a".repeat(129)));
    }

    #[test]
    fn maps_native_events_to_unified_kinds() {
        let k = |t: Tool, v: Value| adapter(t).normalize(&v).unwrap();
        // Kimi 的提交：`prompt` 是内容块数组（2.6.1 冒烟样本），取文字块；不带轮次号（由后台按收到的时刻补）。
        let kimi = k(
            Tool::Kimi,
            json!({"hook_event_name":"UserPromptSubmit","session_id":"s","is_steer":false,
                   "prompt":[{"type":"text","text":"修 bug"},{"type":"image_url","image_url":{"url":"x"}},{"type":"text","text":"再看下"}]}),
        );
        assert_eq!(
            (kimi.kind, kimi.prompt.as_deref(), kimi.prompt_id.as_deref()),
            (Kind::PromptSubmit, Some("修 bug\n再看下"), None)
        );
        assert_eq!(
            k(
                Tool::Kimi,
                json!({"hook_event_name":"PermissionResult","session_id":"s"})
            )
            .kind,
            Kind::PermissionDone
        );
        assert_eq!(
            k(
                Tool::Codex,
                json!({"hook_event_name":"Interrupt","session_id":"s"})
            )
            .kind,
            Kind::TurnInterrupt
        );
        assert_eq!(
            k(
                Tool::Grok,
                json!({"hook_event_name":"StopCancelled","sessionId":"s"})
            )
            .kind,
            Kind::TurnInterrupt
        );
        let g = k(
            Tool::Grok,
            json!({"hook_event_name":"Stop","sessionId":"s","promptId":"p"}),
        );
        assert!(g.turn_scoped && g.kind == Kind::TurnEnd);
        let c = k(
            Tool::Claude,
            json!({"hook_event_name":"Stop","session_id":"s","prompt_id":"p"}),
        );
        assert!(!c.turn_scoped, "只有 Grok 的轮次结束按轮次核对");
        let n = k(
            Tool::Claude,
            json!({"hook_event_name":"Notification","session_id":"s","notification_type":"permission_prompt"}),
        );
        assert_eq!(n.kind, Kind::PermissionWait);
        let n = k(
            Tool::Grok,
            json!({"hook_event_name":"Notification","sessionId":"s","notificationType":"idle_prompt"}),
        );
        assert_eq!(n.kind, Kind::Idle);
        let n = k(
            Tool::Grok,
            json!({"hook_event_name":"Notification","sessionId":"s","notificationType":"elicitation_dialog"}),
        );
        assert_eq!(n.kind, Kind::PermissionWait, "Grok 的 AI 提问算需确认");
        // Claude 自己投递的排队消息改状态，但不算提示词；用户输入的照常记下。
        let injected = |p: &str| k(Tool::Claude, json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":p}));
        let a = injected("<agent-message from=\"a400f2aaf10c4bf38\">\n[Subagent hand-back] …");
        assert_eq!((a.kind, a.prompt), (Kind::PromptSubmit, None));
        assert_eq!(injected("<task-notification>\n<task-id>b1</task-id>").prompt, None);
        assert_eq!(injected("介绍一下 <agent-message> 这个标记").prompt.as_deref(), Some("介绍一下 <agent-message> 这个标记"));
        // 0.4.48 及更早装的配置（还没修复）仍会发 TurnStarted：不再认作提交，不改状态，也不记提示词（2026-11-13 长跑
        // 缺陷 B 中子代理收到任务时发的 TurnStarted 同样落在这里）。
        let old = k(
            Tool::Kimi,
            json!({"hook_event_name":"TurnStarted","session_id":"session_k","turn_id":0,"origin_kind":"system_trigger",
                   "origin_name":"subagent","prompt":"<git-context>\nWorking directory: /w\n</git-context>\n\nTask: …"}),
        );
        assert_eq!((old.kind, old.prompt), (Kind::Other, None));
        let other = k(
            Tool::Claude,
            json!({"hook_event_name":"PreCompact","session_id":"s"}),
        );
        assert_eq!(
            (other.kind, other.name.as_str()),
            (Kind::Other, "PreCompact")
        );
        // prompt 只取自提交事件。
        assert_eq!(
            k(
                Tool::Claude,
                json!({"hook_event_name":"Stop","session_id":"s","prompt":"x"})
            )
            .prompt,
            None
        );
        assert!(adapter(Tool::Claude)
            .normalize(&json!({"session_id":"x"}))
            .is_none());
    }

    /// 三类真实异常各自认出；认识的事件、Notification 的未知子类型都不算（M4b 块 6）。全部历史样本零误报见
    /// tests/replay_samples.rs。
    #[test]
    fn anomalies_are_three_real_classes() {
        for tool in [Tool::Claude, Tool::Codex, Tool::Kimi, Tool::Grok] {
            let a = adapter(tool);
            assert_eq!(a.anomaly(&Value::Null).as_deref(), Some("unparsable"));
            assert_eq!(a.anomaly(&json!([1])).as_deref(), Some("unparsable"));
            assert_eq!(a.anomaly(&json!({"session_id":"s"})).as_deref(), Some("missing_event_name"));
            assert_eq!(
                a.anomaly(&json!({"hook_event_name":"SessionBegin","session_id":"s"})).as_deref(),
                Some("unknown_event:SessionBegin")
            );
            assert_eq!(a.anomaly(&json!({"hook_event_name":"Stop"})).as_deref(), Some("missing_session:Stop"));
            assert_eq!(a.anomaly(&json!({"hook_event_name":"Stop","session_id":"s"})), None);
            let note = json!({"hook_event_name":"Notification","session_id":"s","notification_type":"auth_success"});
            assert_eq!((a.normalize(&note).unwrap().kind, a.anomaly(&note)), (Kind::Other, None));
        }
        let long = format!("X{}", "y".repeat(100));
        let code = adapter(Tool::Codex).anomaly(&json!({"hook_event_name": long, "session_id":"s"})).unwrap();
        assert_eq!(code.chars().count(), "unknown_event:".len() + 64);
    }

    /// 2026-10-15 本机安装路径与 `--version` 输出（docs/design.md 3.1 节「版本来源」）。
    #[test]
    fn version_sources_for_measured_installs() {
        let nvm = "/Users/u/.nvm/versions/node/v24.14.1/lib/node_modules";
        let src = |path: &str, a: &[&str]| {
            let a = args(a);
            let procs = [Process { path, args: &a }];
            let (tool, p) = identify_proc(&procs)?;
            adapter(tool).version_source(p)
        };
        // Claude Code 官方原生安装（2026-11-08 虚拟机实测）：文件名就是版本号。
        let native = "/Users/u/.local/share/claude/versions/2.1.319";
        assert_eq!(id_of(native, &["claude"]), Some(Tool::Claude));
        assert_eq!(src(native, &["claude"]), Some(VersionSource::Known { version: "2.1.319".into(), file: native.into() }));
        assert_eq!(id_of("/Users/u/.local/share/claude/versions/staging", &["claude"]), None);
        assert_eq!(id_of("/Users/u/.local/bin/claude", &["claude"]), None, "链接本身不是系统报告的路径");
        assert_eq!(
            src(&format!("{nvm}/@anthropic-ai/claude-code/bin/claude.exe"), &["claude"]),
            Some(VersionSource::PackageJson { path: format!("{nvm}/@anthropic-ai/claude-code/package.json"), strip: "" })
        );
        assert_eq!(
            src("/Users/u/.nvm/versions/node/v24.14.1/bin/node", &["node", &format!("{nvm}/@openai/codex/bin/codex.js")]),
            Some(VersionSource::PackageJson { path: format!("{nvm}/@openai/codex/package.json"), strip: "" })
        );
        let native = format!("{nvm}/@openai/codex/node_modules/@openai/codex-darwin-arm64");
        assert_eq!(
            src(&format!("{native}/vendor/aarch64-apple-darwin/bin/codex"), &["codex"]),
            Some(VersionSource::PackageJson { path: format!("{native}/package.json"), strip: "-darwin-arm64" })
        );
        // Codex 官方独立安装包（2026-10-31 本机 0.166.0）；共享后台的 app-server-daemon 不认。
        let rel = "/Users/u/.codex/packages/standalone/releases/0.166.0-aarch64-apple-darwin";
        assert_eq!(id_of(&format!("{rel}/bin/codex"), &["codex", "--no-daemon"]), Some(Tool::Codex));
        assert_eq!(src(&format!("{rel}/bin/codex"), &["codex"]),
                   Some(VersionSource::PackageJson { path: format!("{rel}/codex-package.json"), strip: "" }));
        assert_eq!(id_of("/Users/u/.codex/packages/app-server-daemon/releases/local-x-aarch64-apple-darwin/bin/codex", &["codex"]), None);
        assert_eq!(parse_package_version(r#"{"layoutVersion":1,"version":"0.166.0","target":"aarch64-apple-darwin"}"#, "").as_deref(), Some("0.166.0"));
        assert_eq!(
            src("/Users/u/.kimi-code/bin/kimi", &["kimi"]),
            Some(VersionSource::Command { path: "/Users/u/.kimi-code/bin/kimi".into() })
        );
        let g = "/Users/u/.grok/downloads/grok-1.0.52-macos-aarch64";
        assert_eq!(src(g, &["grok"]), Some(VersionSource::Known { version: "1.0.52".into(), file: g.into() }));
        let old = "/Users/u/.grok/downloads/grok-macos-aarch64";
        assert_eq!(src(old, &["agent"]), Some(VersionSource::Command { path: old.into() }));
        for (out, want) in [
            ("2.1.300 (Claude Code)\n", "2.1.300"),
            ("codex-cli 0.162.1\n", "0.162.1"),
            ("2.2.0\n", "2.2.0"),
            ("grok 1.0.52 (0920c8169e27)\n", "1.0.52"),
            ("opencode v2.2.5\n", "2.2.5"),
            ("GitHub Copilot CLI 1.0.106.\nRun 'copilot update' to check for updates.\n", "1.0.106"),
        ] {
            assert_eq!(parse_version_output(out).as_deref(), Some(want), "{out}");
        }
        assert_eq!(parse_version_output("usage: x"), None);
        assert_eq!(parse_package_version(r#"{"version":"0.162.1-darwin-arm64"}"#, "-darwin-arm64").as_deref(), Some("0.162.1"));
        assert_eq!(parse_package_version(r#"{"version":"0.163.0-alpha.1"}"#, "").as_deref(), Some("0.163.0-alpha.1"));
        assert_eq!(parse_package_version("{}", ""), None);
    }

    /// 清单里的每个版本都有样本；精确匹配。
    #[test]
    fn verified_versions_have_samples() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/hook-samples");
        let list: Value = serde_json::from_str(include_str!("../../verified-versions.json")).unwrap();
        for tool in [Tool::Claude, Tool::Codex, Tool::Kimi, Tool::Grok, Tool::OpenCode, Tool::Copilot] {
            let entries = list[tool.as_str()].as_array().unwrap();
            assert!(!entries.is_empty());
            for e in entries {
                let sample = e["sample"].as_str().unwrap();
                assert!(root.join(sample).is_file(), "{} {} 的样本 {sample} 不存在", tool.as_str(), e["version"]);
                assert!(sample.starts_with(&format!("{}-{}", tool.as_str(), e["version"].as_str().unwrap())));
                assert!(e["verified"].as_str().is_some_and(|d| d.len() == 10));
            }
        }
        assert!(verified(Tool::Claude, "2.1.296"));
        assert!(!verified(Tool::Claude, "2.1.29"), "不按前缀匹配");
        assert!(!verified(Tool::Kimi, "2.1.1.1"));
    }

    /// OpenCode 1.19.5（2026-10-29 实测，docs/design.md 3.1 节「新增三款」）：插件原样转发的事件、嵌套字段、占位标题、
    /// 安装路径与恢复命令。
    #[test]
    fn opencode_events_and_paths() {
        let o = adapter(Tool::OpenCode);
        let k = |v: Value| o.normalize(&v).unwrap();
        let created = k(json!({"hook_event_name":"session.created","sessionID":"ses_1","directory":"/w",
            "info":{"id":"ses_1","title":"New session - 2026-10-29T14:38:00.000Z","directory":"/p"}}));
        assert_eq!((created.kind, created.title, created.cwd.as_deref()), (Kind::SessionStart, None, Some("/p")), "占位标题不算");
        let named = k(json!({"hook_event_name":"session.updated","sessionID":"ses_1","info":{"id":"ses_1","title":"修 bug"}}));
        assert_eq!((named.kind, named.title.as_deref()), (Kind::Other, Some("修 bug")));
        let chat = k(json!({"hook_event_name":"chat.message","sessionID":"ses_1","directory":"/w","prompt":"修 bug",
            "model":{"providerID":"opencode","modelID":"deepseek-v4.1-flash-free"}}));
        assert_eq!((chat.kind, chat.prompt.as_deref(), chat.model.as_deref(), chat.cwd.as_deref()),
                   (Kind::PromptSubmit, Some("修 bug"), Some("deepseek-v4.1-flash-free"), Some("/w")));
        let status = |t: &str| k(json!({"hook_event_name":"session.status","sessionID":"s","status":{"type":t}})).kind;
        assert_eq!([status("busy"), status("retry"), status("idle")], [Kind::ToolActivity, Kind::ToolActivity, Kind::Other]);
        for (name, want) in [("permission.asked", Kind::PermissionWait), ("permission.updated", Kind::PermissionWait),
                             ("question.asked", Kind::PermissionWait), ("permission.replied", Kind::PermissionDone),
                             ("question.replied", Kind::PermissionDone), ("question.rejected", Kind::PermissionDone)] {
            assert_eq!(k(json!({"hook_event_name": name, "sessionID":"s"})).kind, want, "{name}");
        }
        let end = k(json!({"hook_event_name":"session.idle","sessionID":"s"}));
        assert!(end.kind == Kind::TurnEnd && end.keeps_error && !end.turn_scoped);
        let err = |n: &str| k(json!({"hook_event_name":"session.error","sessionID":"s","error":{"name":n}})).kind;
        assert_eq!([err("MessageAbortedError"), err("ApiError")], [Kind::TurnInterrupt, Kind::TurnFail]);
        assert!(k(json!({"hook_event_name":"session.idle","sessionID":"c","scyllaui_subagent":true})).subagent);
        assert!(!adapter(Tool::Claude).normalize(&json!({"hook_event_name":"Stop","session_id":"s"})).unwrap().keeps_error);
        assert_eq!(o.anomaly(&json!({"hook_event_name":"session.status","sessionID":"s"})), None, "没有细分值也认得");
        assert_eq!(o.anomaly(&json!({"hook_event_name":"message.part.delta","sessionID":"s"})).as_deref(),
                   Some("unknown_event:message.part.delta"));
        let nvm = "/Users/u/.nvm/versions/node/v24.14.1/lib/node_modules/opencode-ai";
        let platform = format!("{nvm}/node_modules/opencode-darwin-arm64/bin/opencode");
        assert_eq!(id_of(&platform, &["opencode"]), Some(Tool::OpenCode));
        assert_eq!(id_of(&format!("{nvm}/bin/opencode.exe"), &["opencode"]), Some(Tool::OpenCode));
        assert_eq!(id_of("/usr/local/bin/opencode", &["opencode"]), None, "其他安装方式实测后再认");
        // 官方安装脚本（2026-11-08 虚拟机实测 2.2.6）：`~/.opencode/bin/opencode`，版本执行 `--version`。
        let script = "/Users/u/.opencode/bin/opencode";
        assert_eq!(id_of(script, &["opencode", "--standalone"]), Some(Tool::OpenCode));
        assert_eq!(o.version_source(&Process { path: script, args: &args(&["opencode"]) }),
                   Some(VersionSource::Command { path: script.into() }));
        let a = args(&["opencode"]);
        assert_eq!(o.version_source(&Process { path: &platform, args: &a }),
                   Some(VersionSource::PackageJson { path: format!("{nvm}/node_modules/opencode-darwin-arm64/package.json"), strip: "" }));
        assert_eq!(o.launch(Some("x")).as_deref(), Some("opencode"));
        assert_eq!(o.resume("ses_e77e6b1c1ffej0usMZwsmn6d9L").as_deref(), Some("opencode -s ses_e77e6b1c1ffej0usMZwsmn6d9L"));
    }

    /// OpenCode 2.2.4（2026-10-31 实测）：插件把事件的 `data` 摊平转发，提示词由 `prompt` 钩子转为 `session.prompt`；
    /// 一轮以 `session.execution.*` 开始与结束；进程是 `@opencode/cli` 包里的 `opencode.exe`。
    #[test]
    fn opencode_2_events_and_paths() {
        let o = adapter(Tool::OpenCode);
        let k = |v: Value| o.normalize(&v).unwrap();
        let created = k(json!({"hook_event_name":"session.created","sessionID":"ses_2","directory":"/w"}));
        assert_eq!((created.kind, created.cwd.as_deref()), (Kind::SessionStart, Some("/w")));
        let prompt = k(json!({"hook_event_name":"session.prompt","sessionID":"ses_2","directory":"/w","prompt":"用 bash 执行 pwd"}));
        assert_eq!((prompt.kind, prompt.prompt.as_deref()), (Kind::PromptSubmit, Some("用 bash 执行 pwd")));
        let renamed = k(json!({"hook_event_name":"session.renamed","sessionID":"ses_2","title":"用 bash 执行 pwd"}));
        assert_eq!((renamed.kind, renamed.title.as_deref()), (Kind::Other, Some("用 bash 执行 pwd")));
        for (name, want) in [("session.execution.started", Kind::ToolActivity), ("session.execution.succeeded", Kind::TurnEnd),
                             ("session.execution.failed", Kind::TurnFail), ("session.execution.interrupted", Kind::TurnInterrupt),
                             ("permission.asked", Kind::PermissionWait), ("permission.replied", Kind::PermissionDone),
                             ("form.created", Kind::PermissionWait), ("form.replied", Kind::PermissionDone),
                             ("form.cancelled", Kind::PermissionDone)] {
            assert_eq!(k(json!({"hook_event_name": name, "sessionID":"ses_2"})).kind, want, "{name}");
            assert_eq!(o.anomaly(&json!({"hook_event_name": name, "sessionID":"ses_2"})), None, "{name} 认得");
        }
        assert!(k(json!({"hook_event_name":"session.execution.succeeded","sessionID":"c","scyllaui_subagent":true})).subagent);
        let nvm = "/Users/u/.nvm/versions/node/v24.14.1/lib/node_modules/@opencode/cli";
        let exe = format!("{nvm}/bin/opencode.exe");
        assert_eq!(id_of(&exe, &["opencode", "--standalone"]), Some(Tool::OpenCode));
        assert_eq!(id_of(&exe, &["opencode.exe", "serve", "--stdio", "--port", "0"]), Some(Tool::OpenCode), "它的私有服务");
        assert_eq!(id_of(&format!("{nvm}/node_modules/@opencode/cli-darwin-arm64/bin/opencode"), &["opencode"]), Some(Tool::OpenCode));
        let a = args(&["opencode"]);
        assert_eq!(o.version_source(&Process { path: &exe, args: &a }),
                   Some(VersionSource::PackageJson { path: format!("{nvm}/package.json"), strip: "" }));
    }

    /// Copilot 1.0.105（2026-10-30 实测，docs/design.md 3.1 节「新增三款」）：事件名由 Hook 命令补进载荷；
    /// permissionRequest 不算需确认，弹出审批框的 notification 才算；npm 包装进程与原生子进程都认。
    #[test]
    fn copilot_events_and_paths() {
        let c = adapter(Tool::Copilot);
        let k = |v: Value| c.normalize(&v).unwrap();
        let ev = |name: &str| k(json!({"hook_event_name": name, "sessionId": "s1", "cwd": "/w"})).kind;
        assert_eq!(ev("sessionStart"), Kind::SessionStart);
        let p = k(json!({"hook_event_name": "userPromptSubmitted", "sessionId": "s1", "cwd": "/w", "prompt": "修 bug"}));
        assert_eq!((p.kind, p.prompt.as_deref(), p.cwd.as_deref()), (Kind::PromptSubmit, Some("修 bug"), Some("/w")));
        for name in ["preToolUse", "postToolUse", "postToolUseFailure"] {
            assert_eq!(ev(name), Kind::ToolActivity, "{name}");
        }
        assert_eq!(ev("permissionRequest"), Kind::Other, "自动放行时也发");
        let note = |t: &str| k(json!({"hook_event_name": "notification", "sessionId": "s1", "notification_type": t})).kind;
        assert_eq!([note("permission_prompt"), note("elicitation_dialog"), note("agent_idle"), note("shell_completed")],
                   [Kind::PermissionWait, Kind::PermissionWait, Kind::Idle, Kind::Other]);
        let err = |r: bool| k(json!({"hook_event_name": "errorOccurred", "sessionId": "s1", "recoverable": r})).kind;
        assert_eq!([err(false), err(true)], [Kind::TurnFail, Kind::Other]);
        assert_eq!(ev("agentStop"), Kind::TurnEnd);
        assert_eq!(ev("sessionEnd"), Kind::SessionEnd);
        assert!(k(json!({"hook_event_name": "postToolUse", "sessionId": "s1", "agentId": "a1"})).subagent);
        assert_eq!(c.anomaly(&json!({"hook_event_name": "subagentStart", "sessionId": "s1"})), None);
        assert_eq!(c.anomaly(&json!({"sessionId": "s1"})).as_deref(), Some("missing_event_name"), "命令没带 --event");
        let nvm = "/Users/u/.nvm/versions/node/v24.14.1";
        let native = format!("{nvm}/lib/node_modules/@github/copilot/node_modules/@github/copilot-darwin-arm64/copilot");
        assert_eq!(id_of(&native, &["copilot"]), Some(Tool::Copilot));
        let loader = format!("{nvm}/lib/node_modules/@github/copilot/npm-loader.js");
        assert_eq!(id_of(&format!("{nvm}/bin/node"), &["node", &loader]), Some(Tool::Copilot));
        assert_eq!(id_of("/usr/local/bin/copilot", &["copilot"]), None, "其他安装方式实测后再认");
        // 官方安装脚本（2026-11-08 虚拟机实测）：`~/.local/bin/copilot`，版本执行 `--version`。
        let script = "/Users/u/.local/bin/copilot";
        assert_eq!(id_of(script, &["copilot"]), Some(Tool::Copilot));
        assert_eq!(c.version_source(&Process { path: script, args: &args(&["copilot"]) }),
                   Some(VersionSource::Command { path: script.into() }));
        let a = args(&["copilot"]);
        assert_eq!(c.version_source(&Process { path: &native, args: &a }),
                   Some(VersionSource::PackageJson {
                       path: format!("{nvm}/lib/node_modules/@github/copilot/node_modules/@github/copilot-darwin-arm64/package.json"),
                       strip: "" }));
        let b = args(&["node", &loader]);
        assert_eq!(c.version_source(&Process { path: &format!("{nvm}/bin/node"), args: &b }),
                   Some(VersionSource::PackageJson { path: format!("{nvm}/lib/node_modules/@github/copilot/package.json"), strip: "" }));
        let id = "c846656a-a9f7-4145-b710-f45af1aefce8";
        assert_eq!(c.resume(id).unwrap(), format!("copilot --resume={id}"));
        assert_eq!((c.session_record.unwrap())(id), format!(".copilot/session-state/{id}/events.jsonl"));
        assert!(c.scan_while_running && c.waiting_scan.is_some());
    }

    /// Antigravity 1.4.12（2026-11-01 实测，docs/design.md 3.1 节「Antigravity CLI」）：事件名由 Hook 命令补进载荷；
    /// 每轮第一次调用模型即提交提示词（原文在 `lastUserInput`）；`lastUserInput` 为空的是子代理；目录取 `workspacePaths` 第一项。
    #[test]
    fn antigravity_events_and_paths() {
        let a = adapter(Tool::Antigravity);
        let id = "eb669d56-2056-4647-9e87-b3cc31354479";
        let base = |name: &str| json!({"hook_event_name": name, "conversationId": id, "lastUserInput": "跑测试",
            "modelName": "gemini-3.8-flash-low", "workspacePaths": ["/w", "/x"],
            "transcriptPath": format!("/h/.gemini/antigravity-cli/brain/{id}/.system_generated/logs/transcript_full.jsonl")});
        let k = |v: Value| a.normalize(&v).unwrap();
        let start = k(base("SessionStart"));
        assert_eq!((start.kind, start.cwd.as_deref(), start.model.as_deref()), (Kind::SessionStart, Some("/w"), Some("gemini-3.8-flash-low")));
        assert!(start.transcript_path.as_deref().unwrap().ends_with("transcript_full.jsonl"));
        let mut first = base("PreInvocation");
        first["invocationNum"] = json!(0);
        let p = k(first.clone());
        assert_eq!((p.kind, p.prompt.as_deref(), p.session_id.as_deref()), (Kind::PromptSubmit, Some("跑测试"), Some(id)));
        first["invocationNum"] = json!(2);
        assert_eq!(k(first).kind, Kind::ToolActivity, "工具调用之后再次调用模型");
        let mut stop = base("Stop");
        stop["error"] = json!("");
        stop["fullyIdle"] = json!(false);
        assert_eq!(k(stop.clone()).kind, Kind::TurnEnd, "后台命令还在跑也回到输入框");
        stop["error"] = json!("quota exceeded");
        assert_eq!(k(stop).kind, Kind::TurnFail);
        for name in ["PreToolUse", "PostToolUse", "PostInvocation"] {
            assert_eq!(k(base(name)).kind, Kind::Other, "{name}");
        }
        let mut sub = base("PreInvocation");
        sub["lastUserInput"] = json!("");
        sub["invocationNum"] = json!(0);
        assert!(k(sub).subagent, "子代理的事件 lastUserInput 为空");
        assert!(!k(base("Stop")).subagent);
        assert_eq!(a.anomaly(&json!({"conversationId": id})).as_deref(), Some("missing_event_name"), "命令没带 --event");
        assert_eq!(a.anomaly(&base("PostInvocation")), None);
        let agy = "/Users/u/.local/bin/agy";
        assert_eq!(id_of(agy, &["agy"]), Some(Tool::Antigravity));
        assert_eq!(id_of("/opt/homebrew/bin/agy", &["agy"]), None, "其他安装方式实测后再认");
        let x = args(&["agy"]);
        assert_eq!(a.version_source(&Process { path: agy, args: &x }), Some(VersionSource::Command { path: agy.into() }));
        assert_eq!(a.resume(id).unwrap(), format!("agy --conversation={id}"));
        assert_eq!(a.launch(None).as_deref(), Some("agy"));
        assert_eq!((a.session_record.unwrap())(id), format!(".gemini/antigravity-cli/conversations/{id}.db"));
        assert!(a.scan_while_running && matches!(a.waiting_scan, Some(waiting::Scan::AntigravityDb)));
        assert!(a.has_model());
    }

    #[test]
    fn title_sources_follow_design_table() {
        let src = |t| adapter(t).title_source;
        assert_eq!(
            [
                src(Tool::Claude),
                src(Tool::Codex),
                src(Tool::Kimi),
                src(Tool::Grok)
            ],
            [
                TitleSource::TranscriptRecords,
                TitleSource::CodexSessionIndex,
                TitleSource::HookField,
                TitleSource::GrokSummary
            ]
        );
    }
}
