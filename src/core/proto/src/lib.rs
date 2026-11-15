//! ScyllaUI 内核共用部分：线协议常量与默认路径、Hook 来源判定、统一事件与状态机；各款 CLI 的适配器在 adapters/。
//! 依据 docs/design.md 3.1、4.3、4.4、8 节与 docs/protocol.md。

pub mod adapters;
pub mod hookcfg;
pub mod pty;
pub mod redact;
pub mod statusline;

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// App、scyllaui 与 scyllauid 之间 JSON Lines 协议的版本（docs/protocol.md 第 2 节：只有不兼容的改动才升）。
pub const PROTOCOL_VERSION: u64 = 3;

/// 编译时由构建脚本注入，未注入时为 `dev`。
pub const BUILD_ID: &str = match option_env!("SCYLLAUI_BUILD_ID") {
    Some(id) => id,
    None => "dev",
};

/// 单条 Hook 载荷上限；超过即放弃，不影响 CLI。
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// 默认数据目录 `~/Library/Application Support/ScyllaUI`（数据库、socket、单实例锁）。
pub fn default_data_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join("Library/Application Support/ScyllaUI"))
}

/// 常驻 scyllauid 的固定 socket 路径（docs/protocol.md 第 1 节）。
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("scyllauid.sock")
}

/// 墙钟毫秒。数据库与协议中的 `*_at` 字段用。
pub fn now_ms() -> i64 {
    (now_us() / 1000) as i64
}

/// 墙钟微秒。跨进程计算延迟用（同一台机器）。
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Claude,
    Codex,
    Kimi,
    Grok,
    OpenCode,
    Copilot,
    Antigravity,
}

impl Tool {
    pub fn parse(s: &str) -> Option<Tool> {
        match s {
            "claude" => Some(Tool::Claude),
            "codex" => Some(Tool::Codex),
            "kimi" => Some(Tool::Kimi),
            "grok" => Some(Tool::Grok),
            "opencode" => Some(Tool::OpenCode),
            "copilot" => Some(Tool::Copilot),
            "antigravity" => Some(Tool::Antigravity),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Kimi => "kimi",
            Tool::Grok => "grok",
            Tool::OpenCode => "opencode",
            Tool::Copilot => "copilot",
            Tool::Antigravity => "antigravity",
        }
    }
}

/// 按调用时的实际环境判定 Hook 来源，不信任配置里写的工具名（3.1 节「Hook 来源判定与去重」）。
/// 返回 `(实际来源, 是否丢弃)`：Grok 会话中由其他工具配置（如 `~/.claude/settings.json`）触发的那一份丢弃。
pub fn resolve_source(config: Tool, grok_env: bool) -> (Tool, bool) {
    if grok_env {
        (Tool::Grok, config != Tool::Grok)
    } else {
        (config, false)
    }
}

/// 判断是否处在 Grok 的 Hook 进程中：Grok 官方说明的运行器注入变量。
pub fn is_grok_env(get: impl Fn(&str) -> Option<String>) -> bool {
    get("GROK_SESSION_ID").is_some() || get("GROK_HOOK_EVENT").is_some()
}

/// 统一事件：各款 CLI 的原生事件由适配器映射过来（docs/design.md 4.3 节），状态机只认这些。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    SessionStart,
    /// 提交提示词或轮次开始（Kimi 的 TurnStarted）。
    PromptSubmit,
    /// 工具调用前后：回到运行中。
    ToolActivity,
    /// 等待用户审批。
    PermissionWait,
    /// 审批已处理（Kimi 的 PermissionResult）。
    PermissionDone,
    TurnEnd,
    TurnFail,
    /// 轮次被中断或取消（Codex 的 Interrupt、Grok 的 StopCancelled）。
    TurnInterrupt,
    /// CLI 报告空闲（Notification `idle_prompt`）。
    Idle,
    SessionEnd,
    /// 认不出的事件：照常记录，不改变状态。
    Other,
}

/// 规范化后的 Hook 事件；字段名在四款 CLI 之间不一致，由各自的适配器统一。
#[derive(Clone, Debug, PartialEq)]
pub struct HookEvent {
    pub kind: Kind,
    /// 原生事件名，写库与日志用。
    pub name: String,
    pub session_id: Option<String>,
    pub prompt_id: Option<String>,
    pub notification_type: Option<String>,
    pub title: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    /// 载荷里的当前模型（M2 第 2 块）；没有该字段的事件为 None。
    pub model: Option<String>,
    /// 提交的提示词，只在 `PromptSubmit` 时有值。
    pub prompt: Option<String>,
    /// 轮次结束只在与最近一次提交属于同一轮时生效（Grok，见 3.1 节「Grok 的轮次结束判定」）。
    pub turn_scoped: bool,
    /// 这款 CLI 出错之后照发轮次结束（OpenCode：`session.error` 之后紧跟 `session.idle`，2026-10-29 实测 1.19.5），
    /// 轮次结束不把「出错」改回 Ready。
    pub keeps_error: bool,
    /// 子代理事件不改变主会话状态（4.3 节），「需确认」与「审批已处理」除外。
    pub subagent: bool,
    /// 子代理事件属于哪个子代理（`agent_id` 等标记字段的取值，没有时为子会话 ID），按它分别记还在等的审批。
    pub agent_key: Option<String>,
    /// 「需确认」「审批已处理」所指的那次审批的 ID（载荷带时），按它配对。
    pub approval_id: Option<String>,
    /// 「需确认」里待批准的命令原文（适配器声明了 `approval_command` 时）。
    pub command: Option<String>,
    /// CLI 自己起的后台会话：载荷显式给出 `transcript_path: null`。2026-10-08 S0 实测 Codex 0.161.0
    /// 的记忆整理会话（cwd 为 `~/.codex/memories`）继承窗格环境并触发 Hook，不得改变窗格状态。
    pub background: bool,
    /// 新会话接替终端上的上一段对话（Claude 按左方向键转入后台继续，SessionStart `source=fork`）：
    /// 新对话记 `resumed_from` 并沿用上一段的标题（docs/design.md 4.4 节）。
    pub forked: bool,
    /// 会话级的「审批框都已关闭」（从会话记录认出的批准，不分主代理、子代理）：还在等的子代理审批一并算处理完。
    pub all_approvals: bool,
}

/// 按实际来源选用适配器规范化一份原始 Hook 载荷；没有事件名时返回 None。
pub fn normalize(tool: Tool, payload: &Value) -> Option<HookEvent> {
    adapters::adapter(tool).normalize(payload)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Unknown,
    Ready,
    Running,
    Waiting,
    Error,
    /// AI 已退出，这一栏回到 Shell 空闲。
    Ended,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Unknown => "unknown",
            AgentState::Ready => "ready",
            AgentState::Running => "running",
            AgentState::Waiting => "waiting",
            AgentState::Error => "error",
            AgentState::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<AgentState> {
        [AgentState::Unknown, AgentState::Ready, AgentState::Running, AgentState::Waiting, AgentState::Error, AgentState::Ended]
            .into_iter()
            .find(|a| a.as_str() == s)
    }
}

/// 一个事件对窗格状态的作用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// 不属于这个窗格当前会话的事件（子代理、后台会话、其他会话、已结束会话的后续事件）：不改变状态，也不记录。
    Ignored,
    /// 接受但状态没有变化（如重复的 PostToolUse、Grok 旧轮次的 Stop）。
    Unchanged,
    Changed,
}

/// 一个窗格的 Agent 状态。
#[derive(Clone, Debug)]
pub struct PaneState {
    pub tool: Option<Tool>,
    pub state: AgentState,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub transcript_path: Option<String>,
    /// 最近一次提交的轮次 ID，供 `turn_scoped` 的轮次结束事件核对。
    latest_prompt: Option<String>,
    /// 还在等的子代理审批：（`agent_key`，审批 ID）；不空时窗格停在「需确认」（4.3 节「子代理事件」例外，规则一）。
    approvals: Vec<(String, Option<String>)>,
    /// 子代理审批全部处理完后回到的状态：开始等待之前的状态，等待期间主会话的事件改的是它（规则二）。
    resume_to: Option<AgentState>,
    /// 主会话进入「需确认」之前这一轮已经结束（Ready）：分不出主、子代理的审批通知（Grok、Copilot）在后台子代理要审批时
    /// 就是这样到的；从会话记录认出审批已处理后回到 Ready，而不是运行中。
    waited_from_ready: bool,
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState {
            tool: None,
            state: AgentState::Unknown,
            session_id: None,
            title: None,
            transcript_path: None,
            latest_prompt: None,
            approvals: Default::default(),
            resume_to: None,
            waited_from_ready: false,
        }
    }
}

impl PaneState {
    /// scyllauid 重启后从库中恢复的绑定：只知道会话，不知道当前状态（显示「未知」，等下一个事件）。
    pub fn restored(tool: Tool, session_id: String, title: Option<String>) -> PaneState {
        PaneState {
            tool: Some(tool),
            session_id: Some(session_id),
            title,
            ..PaneState::default()
        }
    }

    /// 按 4.3 节状态表推进。
    pub fn apply(&mut self, tool: Tool, ev: &HookEvent) -> Applied {
        if ev.background {
            return Applied::Ignored;
        }
        if ev.subagent {
            return self.apply_subagent(tool, ev);
        }
        // 会话记录表明审批框都已关闭：还在等的子代理审批一并算处理完，回到开始等待之前的状态（主会话这一轮已经结束、
        // 子代理在后台跑时是 Ready，不是运行中）。
        if ev.all_approvals && ev.kind == Kind::PermissionDone && !self.approvals.is_empty() && ev.session_id == self.session_id {
            self.approvals.clear();
            let before = self.state;
            self.state = self.resume_to.take().unwrap_or(AgentState::Running);
            return if self.state == before { Applied::Unchanged } else { Applied::Changed };
        }
        if ev.all_approvals && ev.kind == Kind::PermissionDone && self.state == AgentState::Waiting && self.waited_from_ready && ev.session_id == self.session_id {
            self.waited_from_ready = false;
            self.state = AgentState::Ready;
            return Applied::Changed;
        }
        // 窗格已绑定会话时，其他会话的事件不改变状态；只有 SessionStart 可以换绑（如 Claude `/clear`）。
        // 已结束的会话只接受 SessionStart：Grok 1.0.46 在 SessionEnd 之后还会发一个会话级 Stop。
        let start = ev.kind == Kind::SessionStart;
        // 提交提示词也可换绑（含会话已结束之后）：同一终端用 Ctrl+Z / fg 在几个 AI 之间切换时，fg 回到的 AI 不再发
        // SessionStart，用户在其中提问即认回这段对话（docs/design.md 4.3 节，2026-10-15）。
        // 已结束的同一会话再次提问也认回：在同一终端退出后按原 ID 恢复、恢复时不发 SessionStart 的 CLI（OpenCode，
        // 2026-10-29 实测 1.19.5）第一个事件就是提问。
        let switch = start
            || (ev.kind == Kind::PromptSubmit
                && ev.session_id.is_some()
                && (ev.session_id != self.session_id || self.state == AgentState::Ended));
        if !switch {
            if self.state == AgentState::Ended {
                return Applied::Ignored;
            }
            if let (Some(bound), Some(id)) = (&self.session_id, &ev.session_id) {
                if bound != id {
                    return Applied::Ignored;
                }
            }
        } else if self.session_id != ev.session_id {
            // 换了会话：上一段对话的标题与 transcript 不沿用。
            self.title = None;
            self.transcript_path = None;
        }
        let before = (
            self.tool,
            self.state,
            self.session_id.clone(),
            self.title.clone(),
        );
        if switch || self.tool != Some(tool) {
            self.tool = Some(tool);
            self.latest_prompt = None;
        }
        if ev.session_id.is_some() {
            self.session_id = ev.session_id.clone();
        }
        if ev.title.is_some() {
            self.title = ev.title.clone();
        }
        if ev.transcript_path.is_some() {
            self.transcript_path = ev.transcript_path.clone();
        }
        // 子代理的审批还在等时，窗格显示的「需确认」不是主会话自己的状态，按处理完后要回到的状态判断。
        let current = if self.approvals.is_empty() { self.state } else { self.resume_to.unwrap_or(self.state) };
        let next = match ev.kind {
            // 失败后 CLI 照样会发空闲提示，不能用它把「出错」改回 Ready。
            Kind::Idle if current == AgentState::Error => None,
            Kind::TurnEnd if ev.keeps_error && current == AgentState::Error => None,
            Kind::SessionStart | Kind::TurnInterrupt | Kind::Idle => Some(AgentState::Ready),
            Kind::PromptSubmit => {
                self.latest_prompt = ev.prompt_id.clone();
                Some(AgentState::Running)
            }
            Kind::PermissionWait => Some(AgentState::Waiting),
            Kind::ToolActivity | Kind::PermissionDone => Some(AgentState::Running),
            Kind::TurnEnd => match (&ev.prompt_id, &self.latest_prompt) {
                // 旧轮次的报告忽略；不带轮次 ID 的是会话级报告，直接回到 Ready（Grok 官方说明）。
                (Some(p), Some(latest)) if ev.turn_scoped && p != latest => None,
                _ => Some(AgentState::Ready),
            },
            Kind::TurnFail => Some(AgentState::Error),
            Kind::SessionEnd => Some(AgentState::Ended),
            Kind::Other => None,
        };
        // 子代理的审批还没处理完：会话开始或结束、换绑、中断、用户提问都说明等待已不成立，清掉；没有「审批已处理」
        // 事件、子代理也不比这一轮活得久的 CLI，主会话这一轮结束（含出错）同样清掉——子代理已随之结束，拒绝或最后
        // 一次工具调用之后可能再没有它的事件。其余主会话事件不让窗格离开「需确认」，只改处理完后回到的状态
        // （4.3 节「子代理事件」例外）。
        let decl = adapters::adapter(tool).subagent_approvals;
        let turn_over = !decl.resolved_event && !decl.outlive_turn && matches!(ev.kind, Kind::TurnEnd | Kind::TurnFail | Kind::Idle);
        let next = if switch
            || turn_over
            || matches!(ev.kind, Kind::SessionEnd | Kind::TurnInterrupt)
            || (ev.kind == Kind::PromptSubmit && ev.prompt.is_some())
        {
            self.approvals.clear();
            // 这个事件本身不改状态（如出错后的轮次结束）时，回到等待之前的状态。
            let back = self.resume_to.take();
            next.or(back)
        } else if !self.approvals.is_empty() {
            // 不带代理标识的「需确认」通知不算主会话自己在等（Claude 子代理要审批时除了带 `agent_id` 的 PermissionRequest，
            // 还发一条不带的 Notification `permission_prompt`，2026-11-14）：不改处理完后回到的状态。
            let notice = ev.kind == Kind::PermissionWait && ev.notification_type.is_some();
            if next.is_some() && !notice {
                self.resume_to = next;
            }
            None
        } else {
            next
        };
        if let Some(s) = next {
            // 记下是不是从 Ready 进入「需确认」的；离开「需确认」即作废。
            self.waited_from_ready = s == AgentState::Waiting && (self.waited_from_ready || self.state == AgentState::Ready);
            self.state = s;
        }
        let after = (
            self.tool,
            self.state,
            self.session_id.clone(),
            self.title.clone(),
        );
        if before == after {
            Applied::Unchanged
        } else {
            Applied::Changed
        }
    }

    /// 子代理事件（4.3 节「子代理事件」例外）：只有「需确认」与「审批已处理」改窗格状态，按（子代理，审批 ID）分别记
    /// 还在等的审批，全部处理完才回到开始等待之前的状态。它们可能来自另一个会话（OpenCode 子会话），不受「其他会话的事件」
    /// 一条限制，但不换绑会话，也不改标题与会话记录。窗格没绑会话、换了工具或会话已结束时不认。
    /// 没有「审批已处理」事件的 CLI（适配器的 `subagent_approvals.resolved_event` 为假，如 Claude、Codex 只有
    /// PermissionRequest）：同一子代理之后又有动静（工具调用、一轮结束）即算它的审批都处理完（待用户确认的兜底规则）。
    fn apply_subagent(&mut self, tool: Tool, ev: &HookEvent) -> Applied {
        if self.session_id.is_none() || self.tool != Some(tool) || self.state == AgentState::Ended {
            return Applied::Ignored;
        }
        let resolved_event = adapters::adapter(tool).subagent_approvals.resolved_event;
        let key = ev.agent_key.clone().unwrap_or_default();
        let before = self.state;
        match ev.kind {
            Kind::PermissionWait => {
                if self.approvals.is_empty() {
                    // 主代理的审批正等着时记下的是「需确认」：子代理的先处理完仍在等主代理的，主代理的先处理完则改成运行中。
                    self.resume_to = Some(before);
                }
                // 同一次审批的重复通知（Claude 的 PermissionRequest 与 permission_prompt）只记一次。
                let entry = (key, ev.approval_id.clone());
                if !self.approvals.contains(&entry) {
                    self.approvals.push(entry);
                }
                self.state = AgentState::Waiting;
            }
            Kind::PermissionDone if resolved_event => {
                // 带 ID 的按 ID 配对；配不上（或没带 ID）时处理这个子代理最早的一个没有 ID 的，再不行最早的一个。
                let mine = |a: &(String, Option<String>)| a.0 == key;
                let at = ev
                    .approval_id
                    .as_ref()
                    .and_then(|id| self.approvals.iter().position(|a| mine(a) && a.1.as_ref() == Some(id)))
                    .or_else(|| self.approvals.iter().position(|a| mine(a) && a.1.is_none()))
                    .or_else(|| ev.approval_id.is_none().then(|| self.approvals.iter().position(mine)).flatten());
                let Some(at) = at else { return Applied::Ignored };
                self.approvals.remove(at);
            }
            Kind::Other => return Applied::Ignored,
            _ if resolved_event => return Applied::Ignored,
            _ => {
                let n = self.approvals.len();
                self.approvals.retain(|a| a.0 != key);
                if self.approvals.len() == n {
                    return Applied::Ignored;
                }
            }
        }
        if ev.kind != Kind::PermissionWait && self.approvals.is_empty() {
            self.state = self.resume_to.take().unwrap_or(AgentState::Running);
        }
        if self.state == before {
            Applied::Unchanged
        } else {
            Applied::Changed
        }
    }

    /// 换版本接管时交给新进程的全部状态（docs/protocol.md 第 1 节「接管」的 `agent.state`），含不对外的字段。
    pub fn handoff(&self) -> Value {
        json!({
            "tool": self.tool.map(Tool::as_str),
            "state": self.state.as_str(),
            "session_id": self.session_id,
            "title": self.title,
            "transcript_path": self.transcript_path,
            "latest_prompt": self.latest_prompt,
            "approvals": self.approvals.iter().map(|(k, id)| json!([k, id])).collect::<Vec<_>>(),
            "resume_to": self.resume_to.map(AgentState::as_str),
            "waited_from_ready": self.waited_from_ready,
        })
    }

    /// 从 [`PaneState::handoff`] 还原；工具或状态认不出时为 None。
    pub fn from_handoff(v: &Value) -> Option<PaneState> {
        let s = |k: &str| v[k].as_str().map(str::to_owned);
        Some(PaneState {
            tool: Some(Tool::parse(v["tool"].as_str()?)?),
            state: AgentState::parse(v["state"].as_str()?)?,
            session_id: s("session_id"),
            title: s("title"),
            transcript_path: s("transcript_path"),
            latest_prompt: s("latest_prompt"),
            approvals: v["approvals"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| Some((e[0].as_str()?.to_owned(), e[1].as_str().map(str::to_owned)))).collect())
                .unwrap_or_default(),
            resume_to: v["resume_to"].as_str().and_then(AgentState::parse),
            waited_from_ready: v["waited_from_ready"] == true,
        })
    }

    /// 当前轮次的 ID（最近一次提交带来的），没有为 None。
    pub fn turn(&self) -> Option<&str> {
        self.latest_prompt.as_deref()
    }

    /// 终端对象中的 `agent` 字段（docs/protocol.md 第 5 节）。
    pub fn to_json(&self) -> Value {
        json!({
            "tool": self.tool.map(Tool::as_str),
            "state": self.state.as_str(),
            "session_id": self.session_id,
            "title": self.title,
            "transcript_path": self.transcript_path,
        })
    }
}

/// 布局：argc（i32）、可执行文件路径、若干 NUL 填充，然后是 argc 个以 NUL 结尾的参数（之后是环境变量，不读）。
/// `KERN_PROCARGS2` 开头记录的启动路径（execve 时的可执行文件路径，可能是符号链接）。
pub fn parse_exec_path(buf: &[u8]) -> Option<String> {
    let rest = buf.get(4..)?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    (end > 0).then(|| String::from_utf8_lossy(&rest[..end]).into_owned())
}

pub fn parse_procargs(buf: &[u8]) -> Vec<String> {
    let Some(argc) = buf
        .get(..4)
        .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
    else {
        return Vec::new();
    };
    let mut rest = &buf[4..];
    let exec_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    rest = &rest[exec_end..];
    let start = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
    rest = &rest[start..];
    rest.split(|&b| b == 0)
        .take(argc.max(0) as usize)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

/// Hook 进程环境中与本项目相关的变量：只记名字，值只记白名单内不含凭据的标识。
/// 用于 S0 实测「环境变量继承」，不得记录其他变量的值（可能含 API Key）。
pub fn hook_env_report(vars: impl Iterator<Item = (String, String)>) -> Map<String, Value> {
    const PREFIXES: &[&str] = &[
        "SCYLLAUI_", "CLAUDE", "CODEX", "KIMI", "GROK", "TERM", "GHOSTTY",
    ];
    const VALUE_ALLOWED: &[&str] = &[
        "SCYLLAUI_PANE_ID",
        "SCYLLAUI_SOCK",
        "GROK_HOOK_EVENT",
        "GROK_HOOK_NAME",
        "GROK_SESSION_ID",
        "TERM",
        "TERM_PROGRAM",
    ];
    let mut out = Map::new();
    for (k, v) in vars {
        if PREFIXES.iter().any(|p| k.starts_with(p)) {
            let value = if VALUE_ALLOWED.contains(&k.as_str()) {
                Value::String(v)
            } else {
                Value::from("<set>")
            };
            out.insert(k, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tool: Tool, v: Value) -> HookEvent {
        normalize(tool, &v).unwrap()
    }
    fn cl(v: Value) -> HookEvent {
        ev(Tool::Claude, v)
    }
    fn cx(v: Value) -> HookEvent {
        ev(Tool::Codex, v)
    }
    fn ki(v: Value) -> HookEvent {
        ev(Tool::Kimi, v)
    }
    fn gr(v: Value) -> HookEvent {
        ev(Tool::Grok, v)
    }

    /// OpenCode 出错或中断时先发 `session.error`、紧跟着发 `session.idle`（2026-10-29 实测 1.19.5）：出错不被随后的轮次结束
    /// 改回 Ready；中断后的轮次结束不改变状态；下一次提问照常进入运行中。
    #[test]
    fn opencode_turn_end_after_error() {
        let oc = |v: Value| ev(Tool::OpenCode, v);
        let mut p = PaneState::default();
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.created","sessionID":"s","info":{"id":"s"}})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"chat.message","sessionID":"s","prompt":"x"})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.error","sessionID":"s","error":{"name":"ApiError"}})));
        assert_eq!(p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.idle","sessionID":"s"}))), Applied::Unchanged);
        assert_eq!(p.state, AgentState::Error);
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"chat.message","sessionID":"s","prompt":"再试"})));
        assert_eq!(p.state, AgentState::Running);
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.error","sessionID":"s","error":{"name":"MessageAbortedError"}})));
        assert_eq!(p.state, AgentState::Ready);
        assert_eq!(p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.idle","sessionID":"s"}))), Applied::Unchanged);
        // 退出后在同一终端按原 ID 恢复：不发 session.created，提问即认回；已结束时的其他事件仍忽略。
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.idle","sessionID":"s"})));
        p.state = AgentState::Ended;
        assert_eq!(p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.idle","sessionID":"s"}))), Applied::Ignored);
        assert_eq!(p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"chat.message","sessionID":"s","prompt":"继续"}))),
                   Applied::Changed);
        assert_eq!((p.state, p.session_id.as_deref()), (AgentState::Running, Some("s")));
        // 其他工具的轮次结束照旧把「出错」改回 Ready（不改变现有四款的处理）。
        let mut c = PaneState::default();
        c.apply(Tool::Claude, &cl(json!({"hook_event_name":"StopFailure","session_id":"c"})));
        c.apply(Tool::Claude, &cl(json!({"hook_event_name":"Stop","session_id":"c"})));
        assert_eq!(c.state, AgentState::Ready);
    }

    /// 同一终端挂起 Claude、运行 Codex 并退出，再 fg 回 Claude：Claude 不再发 SessionStart，提问即认回（2026-10-15）；
    /// 其他会话的非提问事件仍忽略。
    #[test]
    fn prompt_submit_rebinds_after_switching_agents() {
        let mut p = PaneState::default();
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"SessionStart","session_id":"c1"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"c1","prompt":"今天星期几"})));
        p.apply(Tool::Codex, &cx(json!({"hook_event_name":"SessionStart","session_id":"x1","transcript_path":"/x"})));
        p.apply(Tool::Codex, &cx(json!({"hook_event_name":"SessionEnd","session_id":"x1","transcript_path":"/x"})));
        assert_eq!((p.tool, p.state), (Some(Tool::Codex), AgentState::Ended));
        assert_eq!(p.apply(Tool::Claude, &cl(json!({"hook_event_name":"Stop","session_id":"c1"}))), Applied::Ignored,
            "其他会话的非提问事件仍忽略");
        assert_eq!(p.apply(Tool::Claude, &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"c1","prompt":"金价"}))),
            Applied::Changed);
        assert_eq!((p.tool, p.session_id.as_deref(), p.state), (Some(Tool::Claude), Some("c1"), AgentState::Running));
        assert_eq!(p.apply(Tool::Claude, &cl(json!({"hook_event_name":"Stop","session_id":"c1"}))), Applied::Changed);
        assert_eq!(p.state, AgentState::Ready);
    }

    /// 这一轮失败后 CLI 约 60 秒发空闲提示（idle_prompt），出错状态不应被它改回 Ready；下一次提问照常进入运行中
    /// （2026-10-29 长跑实测 Grok 额度用完时三次复现）。
    #[test]
    fn idle_prompt_keeps_turn_failure() {
        for (tool, fail, idle, prompt) in [
            (Tool::Claude,
             json!({"hook_event_name":"StopFailure","session_id":"s"}),
             json!({"hook_event_name":"Notification","session_id":"s","notification_type":"idle_prompt"}),
             json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"再试"})),
            (Tool::Grok,
             json!({"hook_event_name":"StopFailure","sessionId":"s"}),
             json!({"hook_event_name":"Notification","sessionId":"s","notificationType":"idle_prompt"}),
             json!({"hook_event_name":"UserPromptSubmit","sessionId":"s","prompt":"再试","promptId":"p2"})),
        ] {
            let mut p = PaneState::default();
            p.apply(tool, &ev(tool, json!({"hook_event_name":"SessionStart","session_id":"s","sessionId":"s"})));
            p.apply(tool, &ev(tool, fail));
            assert_eq!(p.state, AgentState::Error, "{tool:?}");
            assert_eq!(p.apply(tool, &ev(tool, idle)), Applied::Unchanged, "{tool:?}");
            assert_eq!(p.state, AgentState::Error, "{tool:?}");
            p.apply(tool, &ev(tool, prompt));
            assert_eq!(p.state, AgentState::Running, "{tool:?}");
        }
    }

    #[test]
    fn source_resolution_drops_grok_duplicates() {
        assert_eq!(resolve_source(Tool::Claude, false), (Tool::Claude, false));
        assert_eq!(resolve_source(Tool::Claude, true), (Tool::Grok, true));
        assert_eq!(resolve_source(Tool::Grok, true), (Tool::Grok, false));
        assert!(is_grok_env(|k| (k == "GROK_SESSION_ID").then(|| "x".into())));
        assert!(!is_grok_env(|_| None));
    }

    #[test]
    fn normalizes_each_tool_with_its_own_fields() {
        let c = cl(
            json!({"hook_event_name":"SessionStart","session_id":"s1","transcript_path":"/t","cwd":"/w"}),
        );
        assert_eq!(
            (
                c.kind,
                c.session_id.as_deref(),
                c.transcript_path.as_deref(),
                c.cwd.as_deref()
            ),
            (Kind::SessionStart, Some("s1"), Some("/t"), Some("/w"))
        );
        let g = gr(
            json!({"hook_event_name":"Notification","hookEventName":"notification",
            "sessionId":"g1","promptId":"p1","notificationType":"permission_prompt"}),
        );
        assert_eq!(g.session_id.as_deref(), Some("g1"));
        assert_eq!(g.prompt_id.as_deref(), Some("p1"));
        assert_eq!(g.notification_type.as_deref(), Some("permission_prompt"));
        let k = ki(json!({"hook_event_name":"TurnStarted","session_id":"k","session_title":"Fix"}));
        assert_eq!(k.title.as_deref(), Some("Fix"));
        let p = cx(
            json!({"hook_event_name":"UserPromptSubmit","session_id":"x","prompt":"hi","turn_id":"t"}),
        );
        assert_eq!(
            (p.kind, p.prompt.as_deref(), p.prompt_id.as_deref()),
            (Kind::PromptSubmit, Some("hi"), Some("t"))
        );
        assert!(normalize(Tool::Claude, &json!({"session_id":"x"})).is_none());
    }

    #[test]
    fn state_machine_follows_design_table() {
        let mut p = PaneState::default();
        assert_eq!(
            p.apply(
                Tool::Claude,
                &cl(json!({"hook_event_name":"SessionStart","session_id":"a"}))
            ),
            Applied::Changed
        );
        assert_eq!(p.state, AgentState::Ready);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Running);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"PermissionRequest","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"PostToolUse","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Running);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"Stop","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Ready);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"StopFailure","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Error);
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"SessionEnd","session_id":"a"})),
        );
        assert_eq!(p.state, AgentState::Ended);
        assert_eq!(p.session_id.as_deref(), Some("a"));
    }

    /// 回归：2026-10-08 Codex 0.161.0 实测序列。主会话等待审批时，后台记忆整理会话
    /// （transcript_path 为 null）的 SessionStart / UserPromptSubmit / PostToolUse 曾把窗格冲成 running。
    #[test]
    fn codex_background_session_does_not_override_pane() {
        let mut p = PaneState::default();
        let main = |e: &str| {
            cx(json!({"hook_event_name":e,"session_id":"main","transcript_path":"/r.jsonl"}))
        };
        let bg = |e: &str| {
            cx(
                json!({"hook_event_name":e,"session_id":"mem","transcript_path":null,"cwd":"/u/.codex/memories"}),
            )
        };
        p.apply(Tool::Codex, &main("SessionStart"));
        p.apply(Tool::Codex, &main("UserPromptSubmit"));
        assert_eq!(p.apply(Tool::Codex, &bg("SessionStart")), Applied::Ignored);
        assert_eq!(
            p.apply(Tool::Codex, &bg("UserPromptSubmit")),
            Applied::Ignored
        );
        p.apply(Tool::Codex, &main("PermissionRequest"));
        assert_eq!(p.apply(Tool::Codex, &bg("PostToolUse")), Applied::Ignored);
        assert_eq!(
            (p.state, p.session_id.as_deref()),
            (AgentState::Waiting, Some("main"))
        );
        p.apply(Tool::Codex, &main("Interrupt"));
        assert_eq!(p.state, AgentState::Ready);
    }

    /// 回归：Kimi 2.1.1 主代理的审批事件带 `agent_id: "main"` 和整数 `turn_id`，曾被当成子代理忽略。
    #[test]
    fn kimi_main_agent_permission_is_not_subagent() {
        let mut p = PaneState::default();
        let k = |e: &str| {
            ki(
                json!({"hook_event_name":e,"session_id":"session_k","session_title":"T","agent_id":"main","turn_id":1}),
            )
        };
        p.apply(Tool::Kimi, &k("SessionStart"));
        p.apply(Tool::Kimi, &k("TurnStarted"));
        assert_eq!(
            p.apply(Tool::Kimi, &k("PermissionRequest")),
            Applied::Changed
        );
        assert_eq!(p.state, AgentState::Waiting);
        assert_eq!(k("TurnStarted").prompt_id.as_deref(), Some("1"));
        p.apply(Tool::Kimi, &k("PermissionResult"));
        p.apply(Tool::Kimi, &k("Stop"));
        assert_eq!(
            (p.state, p.title.as_deref()),
            (AgentState::Ready, Some("T"))
        );
        let sub =
            ki(json!({"hook_event_name":"Stop","session_id":"session_k","agent_id":"agent_2"}));
        assert!(sub.subagent);
    }

    /// 回归：Grok 1.0.46 退出时先发 SessionEnd，再发不带 promptId 的会话级 Stop，曾把已退出的窗格改回 ready。
    #[test]
    fn grok_stop_after_session_end_keeps_ended() {
        let mut p = PaneState::default();
        let g = |e: &str| gr(json!({"hook_event_name":e,"sessionId":"g"}));
        p.apply(Tool::Grok, &g("SessionStart"));
        p.apply(Tool::Grok, &g("SessionEnd"));
        assert_eq!(p.apply(Tool::Grok, &g("Stop")), Applied::Ignored);
        assert_eq!(p.state, AgentState::Ended);
        // 同一终端里再启动另一款 CLI：SessionStart 换绑，上一段对话的标题不沿用。
        p.title = Some("旧标题".into());
        p.apply(
            Tool::Kimi,
            &ki(json!({"hook_event_name":"SessionStart","session_id":"k"})),
        );
        assert_eq!(
            (p.tool, p.state, p.title.as_deref()),
            (Some(Tool::Kimi), AgentState::Ready, None)
        );
    }

    #[test]
    fn other_session_events_ignored_but_session_start_rebinds() {
        let mut p = PaneState::default();
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"SessionStart","session_id":"a"})),
        );
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"a"})),
        );
        assert_eq!(
            p.apply(
                Tool::Claude,
                &cl(json!({"hook_event_name":"Stop","session_id":"b"}))
            ),
            Applied::Ignored
        );
        assert_eq!(p.state, AgentState::Running);
        // Claude /clear：同一进程换新会话，SessionStart 换绑。
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"SessionStart","session_id":"b","source":"clear"})),
        );
        assert_eq!(
            (p.state, p.session_id.as_deref()),
            (AgentState::Ready, Some("b"))
        );
    }

    /// 回归（2026-11-13 长跑缺陷 D）：OpenCode 2 主会话派子代理，插件把子会话的事件标为子代理后，子会话的提问不换绑终端，
    /// 主会话的完成事件落到终端上（改前子会话的提问换绑，主会话的 `succeeded` 被当作其他会话丢掉，终端卡在运行中）。
    #[test]
    fn opencode_child_session_does_not_take_over_terminal() {
        let oc = |v: Value| normalize(Tool::OpenCode, &v).unwrap();
        let mut p = PaneState::default();
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.prompt","sessionID":"ses_parent","prompt":"派子代理"})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.execution.started","sessionID":"ses_parent"})));
        let child = |e: &str| oc(json!({"hook_event_name":e,"sessionID":"ses_child","scyllaui_subagent":true,
            "prompt":"You are a subagent spawned by another session."}));
        for e in ["session.prompt", "session.execution.started", "session.execution.succeeded"] {
            assert_eq!(p.apply(Tool::OpenCode, &child(e)), Applied::Ignored, "{e}");
        }
        assert_eq!((p.state, p.session_id.as_deref()), (AgentState::Running, Some("ses_parent")));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.execution.succeeded","sessionID":"ses_parent"})));
        assert_eq!(p.state, AgentState::Ready);
    }

    /// 缺陷 E（docs/design.md 4.3 节「子代理事件」例外）：Kimi 2.5.2 子代理的审批带 `agent_id`（如 `agent-0`），曾被当作
    /// 子代理事件整个忽略，侧栏停在运行中、CLI 等了 30 分钟。载荷按主代理审批的样本字段与子代理会话记录
    /// `agents/agent-0/wire.jsonl` 的 interaction.request / interaction.resolved（`agentId: "agent-0"`）重建。
    #[test]
    fn kimi_subagent_approval_waits_and_resumes() {
        let k = |e: &str, agent: &str| {
            ki(json!({"hook_event_name":e,"session_id":"session_k","agent_id":agent,"turn_id":1,"tool_name":"Bash",
                "id":format!("approval_{agent}"),"action":"Running: stat -f '%N %z' README.md"}))
        };
        let mut p = PaneState::default();
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"UserPromptSubmit","session_id":"session_k","prompt":"用子代理复查"})));
        // 子代理接到任务（不带 agent_id，origin_kind 为 system_trigger）：仍是运行中。
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"TurnStarted","session_id":"session_k","turn_id":0,
            "origin_kind":"system_trigger","origin_name":"subagent","prompt":"<git-context>…"})));
        assert_eq!(p.state, AgentState::Running);
        let wait = k("PermissionRequest", "agent-0");
        assert_eq!((wait.kind, wait.subagent, wait.agent_key.as_deref()), (Kind::PermissionWait, true, Some("agent-0")));
        assert_eq!(p.apply(Tool::Kimi, &wait), Applied::Changed);
        assert_eq!(p.state, AgentState::Waiting);
        // 子代理的其他事件仍不改状态（会话级 Stop 等不带 agent_id 的另见报告）。
        assert_eq!(p.apply(Tool::Kimi, &k("Notification", "agent-0")), Applied::Ignored);
        assert_eq!(p.apply(Tool::Kimi, &k("PermissionResult", "agent-0")), Applied::Changed);
        assert_eq!((p.state, p.session_id.as_deref()), (AgentState::Running, Some("session_k")));
        // 主代理的审批照旧：PermissionResult 回到运行中。
        p.apply(Tool::Kimi, &k("PermissionRequest", "main"));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::Kimi, &k("PermissionResult", "main"));
        assert_eq!(p.state, AgentState::Running);
        // 没等过的子代理的「处理完」不认。
        assert_eq!(p.apply(Tool::Kimi, &k("PermissionResult", "agent-9")), Applied::Ignored);
    }

    /// 几个子代理同时等审批：全部处理完才离开「需确认」，期间主会话的事件只改处理完后回到的状态（规则一）；
    /// 主代理的审批也在等时，处理完照旧回到运行中。
    #[test]
    fn concurrent_subagent_approvals_wait_for_all() {
        let k = |e: &str, agent: &str| ki(json!({"hook_event_name":e,"session_id":"s","agent_id":agent}));
        let mut p = PaneState::default();
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"并行"})));
        p.apply(Tool::Kimi, &k("PermissionRequest", "agent-0"));
        p.apply(Tool::Kimi, &k("PermissionRequest", "agent-1"));
        assert_eq!(p.apply(Tool::Kimi, &k("PermissionResult", "agent-0")), Applied::Unchanged);
        assert_eq!(p.state, AgentState::Waiting, "agent-1 还在等");
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"PostToolUse","session_id":"s"})));
        assert_eq!(p.state, AgentState::Waiting, "主会话的工具调用不让窗格离开需确认");
        p.apply(Tool::Kimi, &k("PermissionRequest", "main"));
        p.apply(Tool::Kimi, &k("PermissionResult", "main"));
        assert_eq!(p.state, AgentState::Waiting, "主代理的审批处理完，子代理的还在等");
        p.apply(Tool::Kimi, &k("PermissionResult", "agent-1"));
        assert_eq!(p.state, AgentState::Running);
        // 主代理先在等、子代理后等：子代理处理完仍是需确认，主代理处理完回到运行中。
        p.apply(Tool::Kimi, &k("PermissionRequest", "main"));
        p.apply(Tool::Kimi, &k("PermissionRequest", "agent-0"));
        p.apply(Tool::Kimi, &k("PermissionResult", "agent-0"));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::Kimi, &k("PermissionResult", "main"));
        assert_eq!(p.state, AgentState::Running);
        // 用户提问、会话结束、中断清掉还在等的审批。
        p.apply(Tool::Kimi, &k("PermissionRequest", "agent-0"));
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"换个问题"})));
        assert_eq!(p.state, AgentState::Running);
        p.apply(Tool::Kimi, &k("Stop", "main"));
        assert_eq!(p.state, AgentState::Ready, "清掉后主会话照常");
        p.apply(Tool::Kimi, &k("PermissionRequest", "agent-0"));
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"SessionEnd","session_id":"s"})));
        assert_eq!(p.state, AgentState::Ended);
        assert_eq!(p.apply(Tool::Kimi, &k("PermissionRequest", "agent-0")), Applied::Ignored, "会话已结束");
        let c = |v: Value| cx(v);
        let mut p = PaneState::default();
        p.apply(Tool::Codex, &c(json!({"hook_event_name":"UserPromptSubmit","session_id":"x","prompt":"查","turn_id":"1"})));
        p.apply(Tool::Codex, &c(json!({"hook_event_name":"PermissionRequest","session_id":"x","agent_id":"w1"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::Codex, &c(json!({"hook_event_name":"Interrupt","session_id":"x"})));
        assert_eq!(p.state, AgentState::Ready);
        assert_eq!(p.apply(Tool::Codex, &c(json!({"hook_event_name":"PostToolUse","session_id":"x","agent_id":"w1"}))),
                   Applied::Ignored, "中断已清掉");
    }

    /// OpenCode 2.3.0 子代理默认在后台跑：主会话这一轮先结束，子会话再要审批。子会话的审批改窗格状态但不换绑，
    /// 处理完回到开始等待之前的 Ready，而不是卡在运行中（规则二）。
    #[test]
    fn opencode_child_approval_after_parent_turn_ended() {
        let oc = |v: Value| normalize(Tool::OpenCode, &v).unwrap();
        let mut p = PaneState::default();
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.prompt","sessionID":"ses_parent","prompt":"派子代理"})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.renamed","sessionID":"ses_parent","title":"列文件"})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.execution.succeeded","sessionID":"ses_parent"})));
        assert_eq!(p.state, AgentState::Ready);
        let child = |e: &str| oc(json!({"hook_event_name":e,"sessionID":"ses_child","scyllaui_subagent":true,"id":"per_1"}));
        assert_eq!(child("permission.asked").agent_key.as_deref(), Some("ses_child"));
        assert_eq!(p.apply(Tool::OpenCode, &child("permission.asked")), Applied::Changed);
        assert_eq!((p.state, p.session_id.as_deref(), p.title.as_deref()), (AgentState::Waiting, Some("ses_parent"), Some("列文件")));
        assert_eq!(p.apply(Tool::OpenCode, &child("session.renamed")), Applied::Ignored, "子会话只带标题的事件不认");
        assert_eq!(p.apply(Tool::OpenCode, &child("permission.replied")), Applied::Changed);
        assert_eq!((p.state, p.session_id.as_deref()), (AgentState::Ready, Some("ses_parent")));
        // 等待期间主会话又开了一轮又结束：处理完回到最新的 Ready；又开了一轮未结束：回到运行中。
        p.apply(Tool::OpenCode, &child("permission.asked"));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.execution.started","sessionID":"ses_parent"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::OpenCode, &child("permission.replied"));
        assert_eq!(p.state, AgentState::Running);
    }

    /// 没有「审批已处理」事件的 CLI（Claude 子代理的 PermissionRequest 带 `agent_id`）：同一子代理之后有工具调用即算处理完。
    #[test]
    fn subagent_activity_resolves_its_approval() {
        let mut p = PaneState::default();
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"a","prompt":"查"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PermissionRequest","session_id":"a","agent_id":"sub1"})));
        assert_eq!(p.state, AgentState::Waiting);
        assert_eq!(p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PostToolUse","session_id":"a","agent_id":"sub2"}))), Applied::Ignored);
        assert_eq!(p.state, AgentState::Waiting, "别的子代理的动静不算");
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PostToolUse","session_id":"a","agent_id":"sub1"})));
        assert_eq!(p.state, AgentState::Running);
    }

    /// 有「审批已处理」事件的 CLI（Kimi）：只有它算处理完，按审批 ID 配对。同一子代理并行的工具调用（PostToolUse 带
    /// 同一个 agent_id）不让窗格提前离开；同一子代理同时两个审批，处理完一个仍在等（评审第 2 条）。
    #[test]
    fn approvals_pair_by_id_when_cli_reports_resolution() {
        let k = |e: &str, id: &str| ki(json!({"hook_event_name":e,"session_id":"s","agent_id":"agent-0","id":id}));
        let mut p = PaneState::default();
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"查"})));
        p.apply(Tool::Kimi, &k("PermissionRequest", "approval_a"));
        p.apply(Tool::Kimi, &k("PermissionRequest", "approval_b"));
        assert_eq!(p.apply(Tool::Kimi, &k("PostToolUse", "")), Applied::Ignored, "并行工具调用不算处理完");
        p.apply(Tool::Kimi, &k("PermissionResult", "approval_b"));
        assert_eq!(p.state, AgentState::Waiting, "approval_a 还在等");
        assert_eq!(p.apply(Tool::Kimi, &k("PermissionResult", "approval_b")), Applied::Ignored, "已处理过的不再认");
        p.apply(Tool::Kimi, &k("PermissionResult", "approval_a"));
        assert_eq!(p.state, AgentState::Running);
        // Kimi 子代理跑在主代理的工具调用里，但它的 Stop 不带 agent_id、看起来像主会话的：有处理完事件的 CLI 不因
        // 主会话一轮结束清掉还在等的审批，处理完后回到的状态跟着变。
        p.apply(Tool::Kimi, &k("PermissionRequest", "approval_c"));
        p.apply(Tool::Kimi, &ki(json!({"hook_event_name":"Stop","session_id":"s"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::Kimi, &k("PermissionResult", "approval_c"));
        assert_eq!(p.state, AgentState::Ready);
    }

    /// 没有处理完事件的 CLI（Claude）：拒绝或批准了子代理最后一次工具调用后可能再没有它的事件，主会话这一轮结束时
    /// 清掉还在等的审批，不卡在「需确认」（评审第 1 条）。
    #[test]
    fn main_turn_end_clears_approvals_without_resolution_event() {
        let mut p = PaneState::default();
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"a","prompt":"查"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PermissionRequest","session_id":"a","agent_id":"sub1"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PostToolUse","session_id":"a"})));
        assert_eq!(p.state, AgentState::Waiting, "主会话的工具调用不清");
        p.apply(Tool::Claude, &cl(json!({"hook_event_name":"Stop","session_id":"a"})));
        assert_eq!(p.state, AgentState::Ready);
        assert_eq!(p.apply(Tool::Claude, &cl(json!({"hook_event_name":"PostToolUse","session_id":"a","agent_id":"sub1"}))),
                   Applied::Ignored, "已清掉");
    }

    /// OpenCode：`permission.replied` 带 `requestID` 与 `permission.asked` 的 `id` 配对；等待期间主会话出错、随后的
    /// `session.idle` 不把处理完后回到的状态从「出错」改成 Ready（评审第 4 条）。
    #[test]
    fn opencode_pairing_and_error_during_wait() {
        let oc = |v: Value| normalize(Tool::OpenCode, &v).unwrap();
        let child = |v: Value| {
            let mut v = v;
            v["sessionID"] = json!("ses_c");
            v["scyllaui_subagent"] = json!(true);
            oc(v)
        };
        let mut p = PaneState::default();
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.prompt","sessionID":"ses_p","prompt":"派"})));
        p.apply(Tool::OpenCode, &child(json!({"hook_event_name":"permission.asked","id":"per_1"})));
        p.apply(Tool::OpenCode, &child(json!({"hook_event_name":"permission.asked","id":"per_2"})));
        assert_eq!(child(json!({"hook_event_name":"permission.replied","requestID":"per_2"})).approval_id.as_deref(), Some("per_2"));
        p.apply(Tool::OpenCode, &child(json!({"hook_event_name":"permission.replied","requestID":"per_2","reply":"reject"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.error","sessionID":"ses_p","error":{"name":"ApiError"}})));
        p.apply(Tool::OpenCode, &oc(json!({"hook_event_name":"session.idle","sessionID":"ses_p"})));
        assert_eq!(p.state, AgentState::Waiting, "子会话的审批还在等");
        p.apply(Tool::OpenCode, &child(json!({"hook_event_name":"permission.replied","requestID":"per_1","reply":"once"})));
        assert_eq!(p.state, AgentState::Error, "出错后的空闲不覆盖");
    }

    /// Claude 子代理要审批时发两个事件（2.1.325 包内代码）：带 `agent_id` 的 PermissionRequest 与不带的 Notification
    /// `permission_prompt`。后者不算主会话自己在等；会话记录表明审批框都已关闭时回到开始等待之前的状态。
    #[test]
    fn claude_subagent_approval_with_unscoped_notice_and_record() {
        let record_done = |session: &str| HookEvent { all_approvals: true, ..cl(json!({"hook_event_name": "PostToolUse", "session_id": session})) };
        let done = |session: &str| HookEvent { kind: Kind::PermissionDone, ..record_done(session) };
        let mut p = PaneState::default();
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "SessionStart", "session_id": "s1"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "跑"})));
        let ask = |p: &mut PaneState| {
            p.apply(Tool::Claude, &cl(json!({"hook_event_name": "PermissionRequest", "session_id": "s1", "agent_id": "a1", "tool_name": "Bash"})));
            p.apply(Tool::Claude, &cl(json!({"hook_event_name": "Notification", "session_id": "s1", "notification_type": "permission_prompt"})));
        };
        ask(&mut p);
        assert_eq!(p.state, AgentState::Waiting);
        // 兜底：子代理之后又有动静，回到运行中，不是停在「需确认」。
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "PostToolUse", "session_id": "s1", "agent_id": "a1"})));
        assert_eq!(p.state, AgentState::Running, "不带代理标识的通知不该把要回到的状态改成需确认");
        // 会话记录的批准：立即回到运行中。
        ask(&mut p);
        assert_eq!(p.apply(Tool::Claude, &done("s1")), Applied::Changed);
        assert_eq!(p.state, AgentState::Running);
        // 主会话这一轮已经结束、子代理在后台：批准后回到 Ready。
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "Stop", "session_id": "s1"})));
        assert_eq!(p.state, AgentState::Ready);
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "PermissionRequest", "session_id": "s1", "agent_id": "a1", "tool_name": "Bash"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(Tool::Claude, &done("s1"));
        assert_eq!(p.state, AgentState::Ready);
        // 主代理自己的审批：照旧回到运行中。
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "再跑"})));
        p.apply(Tool::Claude, &cl(json!({"hook_event_name": "PermissionRequest", "session_id": "s1", "tool_name": "Bash"})));
        p.apply(Tool::Claude, &done("s1"));
        assert_eq!(p.state, AgentState::Running);
    }

    #[test]
    fn subagent_events_do_not_change_state() {
        let mut p = PaneState::default();
        p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"UserPromptSubmit","session_id":"a"})),
        );
        let applied = p.apply(
            Tool::Claude,
            &cl(json!({"hook_event_name":"Stop","session_id":"a","agent_id":"sub"})),
        );
        assert_eq!(applied, Applied::Ignored);
        assert_eq!(p.state, AgentState::Running);
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"SessionEnd","sessionId":"c","subagentType":"explore"})),
        );
        assert_eq!(p.state, AgentState::Running);
    }

    #[test]
    fn grok_stop_dedups_by_prompt_id() {
        let mut p = PaneState::default();
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"SessionStart","sessionId":"g"})),
        );
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"UserPromptSubmit","sessionId":"g","promptId":"p2"})),
        );
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"Stop","sessionId":"g","promptId":"p1"})),
        );
        assert_eq!(p.state, AgentState::Running, "旧轮次的 Stop 不得置为 Ready");
        assert_eq!(
            p.apply(
                Tool::Grok,
                &gr(json!({"hook_event_name":"Stop","sessionId":"g","promptId":"p1"}))
            ),
            Applied::Unchanged,
            "旧轮次的 Stop 属于本会话，接受但不改变状态"
        );
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"Stop","sessionId":"g","promptId":"p2"})),
        );
        assert_eq!(p.state, AgentState::Ready);
        p.apply(Tool::Grok, &gr(json!({"hook_event_name":"Notification","sessionId":"g","notificationType":"permission_prompt"})));
        assert_eq!(p.state, AgentState::Waiting);
        p.apply(
            Tool::Grok,
            &gr(json!({"hook_event_name":"StopCancelled","sessionId":"g","promptId":"p2"})),
        );
        assert_eq!(p.state, AgentState::Ready);
    }

    #[test]
    fn env_report_never_leaks_unlisted_values() {
        let vars = vec![
            ("SCYLLAUI_PANE_ID".to_string(), "p1".to_string()),
            ("GROK_API_KEY".to_string(), "secret".to_string()),
            ("XAI_API_KEY".to_string(), "secret".to_string()),
            ("CLAUDE_PROJECT_DIR".to_string(), "/x".to_string()),
        ];
        let r = hook_env_report(vars.into_iter());
        assert_eq!(r["SCYLLAUI_PANE_ID"], "p1");
        assert_eq!(r["GROK_API_KEY"], "<set>");
        assert_eq!(r["CLAUDE_PROJECT_DIR"], "<set>");
        assert!(!r.contains_key("XAI_API_KEY"));
    }
}
