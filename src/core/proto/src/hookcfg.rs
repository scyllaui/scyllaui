//! 四款 CLI 用户级配置中本项目 Hook 的状态分析、修复与撤销（M1 第 7 块，docs/design.md 4.7 节「实现」）。
//! 安装规则与 S0 的 `src/scripts/s0-hooks.py`（2026-10-16 删除，最后版本见提交 459fc2a）逐字节兼容：事件清单、命令文本、本项目条目的识别、Kimi 的标记块、
//! 备份命名都相同。分析与生成改动是纯函数（文本进、文本出）；读写文件的部分在本文件末尾，scyllaui 与 scyllauid 共用。
//! 依据：S0 第 3 项、`docs/s0-hook-compat.md`「防失效取证」（Codex 信任哈希与 `enabled`、Grok `disabled-hooks`）。

use crate::Tool;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const TOOLS: [Tool; 7] = [Tool::Claude, Tool::Codex, Tool::Kimi, Tool::Grok, Tool::OpenCode, Tool::Copilot, Tool::Antigravity];
/// Antigravity 共用 Hook 文件里本项目那一项的名字（顶层键）。
pub const ANTIGRAVITY_NAME: &str = "scyllaui";
/// OpenCode 的插件（docs/design.md 3.1 节「OpenCode 插件」）：整份由本项目生成，按全文比对。
pub const OPENCODE_PLUGIN: &str = include_str!("opencode-plugin.js");
pub const TIMEOUT: u64 = 2;
pub const KIMI_BEGIN: &str = "# >>> scyllaui hooks >>>";
pub const KIMI_END: &str = "# <<< scyllaui hooks <<<";
/// Claude Code 的托管设置（官方路径，组织下发）：其中的 `disableAllHooks` / `allowManagedHooksOnly` 个人改不了。
const CLAUDE_MANAGED: &str = "/Library/Application Support/ClaudeCode/managed-settings.json";

/// 各款安装的事件（与 s0-hooks.py 的 EVENTS 相同，顺序也相同）。
pub fn events(tool: Tool) -> &'static [&'static str] {
    match tool {
        Tool::Claude => &["SessionStart", "UserPromptSubmit", "PermissionRequest", "PostToolUse",
                          "Notification", "Stop", "StopFailure", "SessionEnd"],
        Tool::Codex => &["SessionStart", "UserPromptSubmit", "PermissionRequest", "PostToolUse",
                         "Stop", "Interrupt", "SessionEnd"],
        // 不装 TurnStarted：Kimi 改设置时的严格校验（node-sdk `HookDefSchema`）不认它，装上后 `/login`、`/logout` 报
        // `config.invalid`（2026-11-15 新用户验收，2.5.3、2.6.1 实测；上游 MoonshotAI/kimi-code#4962）。见 KIMI_STRICT_EVENTS。
        Tool::Kimi => &["SessionStart", "UserPromptSubmit", "PermissionRequest", "PermissionResult",
                        "PostToolUse", "Notification", "Stop", "StopFailure", "Interrupt", "SessionEnd"],
        Tool::Grok => &["SessionStart", "UserPromptSubmit", "PostToolUse", "Notification",
                        "Stop", "StopFailure", "StopCancelled", "SessionEnd"],
        // 插件转发的事件（与 opencode-plugin.js 一致）：OpenCode 1 的，加上 OpenCode 2 的（2026-10-31）。
        Tool::OpenCode => &["session.created", "session.updated", "session.status", "session.idle", "session.error",
                            "permission.asked", "permission.updated", "permission.replied",
                            "question.asked", "question.replied", "question.rejected", "chat.message",
                            "session.renamed", "session.execution.started", "session.execution.succeeded",
                            "session.execution.failed", "session.execution.interrupted",
                            "form.created", "form.replied", "form.cancelled", "session.prompt"],
        // 不装 permissionRequest（命令被自动放行时也发，不改状态）、userPromptTransformed、子代理与压缩（2026-10-30 实测 1.0.105）。
        Tool::Copilot => &["sessionStart", "userPromptSubmitted", "preToolUse", "postToolUse", "postToolUseFailure",
                           "notification", "agentStop", "errorOccurred", "sessionEnd"],
        // 不装 PreToolUse：它的 Hook 失败（退出码非零、输出不是 JSON）时 agy 拦下这次工具调用，这份文件 Antigravity 的
        // App、IDE 也执行。不装 PostToolUse：后台命令跑完时才发，会把已结束的一轮改回运行中；工具调用之后的那次调用模型
        // 已有 PreInvocation。PostInvocation 用不上（2026-11-01 实测 1.4.12，docs/design.md 3.1 节「Antigravity CLI」）。
        Tool::Antigravity => &["SessionStart", "PreInvocation", "Stop"],
    }
}

/// Hook 命令。Grok 会先自行展开 `${VAR}`，未设置且无默认值即报错，Grok 又会执行 Claude 的配置，所以这两款用带默认值的
/// 写法；Codex、Kimi 维持原写法：命令文本一变，Codex 就要求重新信任（docs/s0-hook-compat.md「Hook 来源判定与去重」）。
pub fn command(tool: Tool) -> String {
    let bin_ref = match tool {
        Tool::Grok | Tool::Claude | Tool::Copilot | Tool::Antigravity => "${SCYLLAUI_BIN:-}",
        Tool::Codex | Tool::Kimi | Tool::OpenCode => "$SCYLLAUI_BIN",
    };
    format!(
        "if [ -n \"${{SCYLLAUI_SOCK:-}}\" ] && [ -x \"${{SCYLLAUI_BIN:-}}\" ]; then \"{bin_ref}\" hook --config {}; fi",
        tool.as_str()
    )
}

/// 载荷不带事件名的 CLI（Copilot、Antigravity）：每个事件一条命令，以 `--event` 带上事件名。
pub fn command_for(tool: Tool, event: &str) -> String {
    command(tool).replace(&format!("hook --config {}", tool.as_str()), &format!("hook --config {} --event {event}", tool.as_str()))
}

/// 新旧两种写法都认作本项目的条目；不依赖引号（JSON 原文里引号被转义）。
pub fn is_ours_text(c: &str) -> bool {
    c.contains("hook --config") && (c.contains("$SCYLLAUI_BIN") || c.contains("${SCYLLAUI_BIN:-}"))
}

fn is_ours(h: &Value) -> bool {
    h.get("command").and_then(Value::as_str).is_some_and(is_ours_text)
}

/// `UserPromptSubmit` → `user_prompt_submit`：Codex 信任记录的键与 Grok 停用记录的名字都用这种写法。
pub fn snake(event: &str) -> String {
    let mut out = String::new();
    for (i, c) in event.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// 某款 CLI 的配置文件位置：`main` 放 Hook 条目；`aux` 是停用与信任记录（Codex 的 `config.toml`、Grok 的 `disabled-hooks`）；
/// `policy` 是组织策略文件（存在时只读）。
pub struct Layout {
    pub dir: PathBuf,
    pub main: PathBuf,
    pub aux: Option<PathBuf>,
    pub policy: Vec<PathBuf>,
}

pub fn layout(tool: Tool, home: &Path) -> Layout {
    match tool {
        Tool::Claude => Layout {
            dir: home.join(".claude"),
            main: home.join(".claude/settings.json"),
            aux: None,
            policy: vec![PathBuf::from(CLAUDE_MANAGED)],
        },
        Tool::Codex => Layout {
            dir: home.join(".codex"),
            main: home.join(".codex/hooks.json"),
            aux: Some(home.join(".codex/config.toml")),
            policy: vec![],
        },
        Tool::Kimi => Layout {
            dir: home.join(".kimi-code"),
            main: home.join(".kimi-code/config.toml"),
            aux: None,
            policy: vec![],
        },
        Tool::Grok => Layout {
            dir: home.join(".grok"),
            main: home.join(".grok/hooks/scyllaui.json"),
            aux: Some(home.join(".grok/disabled-hooks")),
            // Grok 官方说明：`allow_managed_hooks_only` 只能由策略层设置（requirements.toml、managed_config.toml，
            // 位于 Grok 家目录或 /etc/grok）；macOS MDM 下发的读不到，此时按配置本身判断。
            policy: vec![
                home.join(".grok/requirements.toml"),
                home.join(".grok/managed_config.toml"),
                PathBuf::from("/etc/grok/requirements.toml"),
                PathBuf::from("/etc/grok/managed_config.toml"),
            ],
        },
        // OpenCode 首次运行时建 `~/.config/opencode`（2026-10-29 实测 1.19.5）；全局插件目录启动时自动加载。
        // 自建的 Hook 文件在用户级 hooks 目录，不需要逐个目录信任（2026-10-30 实测）；`settings.json` 的 disableAllHooks
        // 会停用全部 Hook。设了 COPILOT_HOME 时配置在别处，1.0 只支持默认位置。
        Tool::Copilot => Layout {
            dir: home.join(".copilot"),
            main: home.join(".copilot/hooks/scyllaui.json"),
            aux: Some(home.join(".copilot/settings.json")),
            policy: vec![],
        },
        Tool::OpenCode => Layout {
            dir: home.join(".config/opencode"),
            main: home.join(".config/opencode/plugins/scyllaui.js"),
            aux: None,
            policy: vec![],
        },
        // 全局 Hook 文件由 Antigravity 的 App、IDE、CLI 共用（官方 Hooks 文档）；只认 CLI 是否装过（它首次运行时建
        // `~/.gemini/antigravity-cli`，2026-11-01 本机核对）。
        Tool::Antigravity => Layout {
            dir: home.join(".gemini/antigravity-cli"),
            main: home.join(".gemini/config/hooks.json"),
            aux: None,
            policy: vec![],
        },
    }
}

/// 文件内容：`Ok(None)` 不存在，`Err` 读不了（权限、非 UTF-8 等）。
pub type FileText = Result<Option<String>, String>;

/// 分析一款 CLI 所需的全部输入。`home` 原样保留调用方给的写法：Codex 信任记录的键按 HOME 原样拼接、不做路径规范化
/// （2026-10-15 取证）。
pub struct Snapshot {
    pub tool: Tool,
    pub home: String,
    pub dir_exists: bool,
    pub main: FileText,
    pub aux: FileText,
    pub policy: Vec<(PathBuf, FileText)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ok,
    Missing,
    Disabled,
    Untrusted,
    Policy,
    Unreadable,
    Absent,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Missing => "missing",
            State::Disabled => "disabled",
            State::Untrusted => "untrusted",
            State::Policy => "policy",
            State::Unreadable => "unreadable",
            State::Absent => "absent",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub tool: Tool,
    pub state: State,
    pub path: String,
    pub problems: Vec<String>,
    pub fixable: bool,
}

impl Status {
    pub fn to_json(&self) -> Value {
        json!({"tool": self.tool.as_str(), "state": self.state.as_str(), "path": self.path,
               "problems": self.problems, "fixable": self.fixable})
    }
}

/// 分析中间结果：本项目每个事件的条目在哪（组序号、条目序号）、有哪些问题。
#[derive(Default)]
struct Findings {
    missing: Vec<String>,
    outdated: Vec<String>,
    disabled: Vec<String>,
    untrusted: Vec<String>,
    policy: Vec<String>,
    unreadable: Vec<String>,
}

pub fn analyze(s: &Snapshot) -> Status {
    let path = layout(s.tool, Path::new(&s.home)).main.display().to_string();
    let mut f = Findings::default();
    let main_text = match &s.main {
        Err(e) => {
            f.unreadable.push(format!("read:{e}"));
            None
        }
        Ok(t) => t.clone(),
    };
    if !s.dir_exists && main_text.is_none() && f.unreadable.is_empty() {
        return Status { tool: s.tool, state: State::Absent, path, problems: vec![], fixable: false };
    }
    for (p, text) in &s.policy {
        if let Ok(Some(t)) = text {
            if policy_locks(s.tool, t) {
                f.policy.push(format!("policy:{}", p.display()));
            }
        }
    }
    let aux = match &s.aux {
        Err(e) => {
            f.unreadable.push(format!("read:{e}"));
            None
        }
        Ok(t) => t.clone(),
    };
    if f.unreadable.is_empty() {
        if let Err(e) = inspect(s, main_text.as_deref(), aux.as_deref(), &mut f) {
            f.unreadable.push(e);
        }
    }
    let state = if !f.policy.is_empty() {
        State::Policy
    } else if !f.unreadable.is_empty() {
        State::Unreadable
    } else if !f.missing.is_empty() || !f.outdated.is_empty() {
        State::Missing
    } else if !f.disabled.is_empty() {
        State::Disabled
    } else if !f.untrusted.is_empty() {
        State::Untrusted
    } else {
        State::Ok
    };
    let problems = [
        (&f.policy, ""),
        (&f.unreadable, ""),
        (&f.missing, "missing:"),
        (&f.outdated, "outdated:"),
        (&f.disabled, ""),
        (&f.untrusted, "untrusted:"),
    ]
    .iter()
    .flat_map(|(v, prefix)| v.iter().map(move |x| format!("{prefix}{x}")))
    .collect();
    Status {
        tool: s.tool,
        state,
        path,
        problems,
        fixable: matches!(state, State::Missing | State::Disabled),
    }
}

/// 组织策略是否锁定了个人 Hook。
fn policy_locks(tool: Tool, text: &str) -> bool {
    match tool {
        Tool::Claude => serde_json::from_str::<Value>(text).is_ok_and(|v| {
            v["disableAllHooks"] == true || v["allowManagedHooksOnly"] == true
        }),
        // 官方说明：非布尔值也视为开启，只有明确的 false 不锁。
        Tool::Grok => toml_lines(text).iter().any(|l| {
            l.key.as_deref().is_some_and(|k| k == "allow_managed_hooks_only" || k == "allowManagedHooksOnly")
                && l.value.as_deref() != Some("false")
        }),
        Tool::Codex | Tool::Kimi | Tool::OpenCode | Tool::Copilot | Tool::Antigravity => false,
    }
}

fn inspect(s: &Snapshot, main: Option<&str>, aux: Option<&str>, f: &mut Findings) -> Result<(), String> {
    match s.tool {
        Tool::Kimi => {
            inspect_kimi(main.unwrap_or(""), f);
            Ok(())
        }
        // Copilot：自建文件按全文比对；settings.json 里的 disableAllHooks 为真时停用。
        Tool::Copilot => {
            match main {
                None => f.missing.push("hooks_file".into()),
                Some(t) if t != copilot_file() => f.outdated.push("hooks_file".into()),
                _ => {}
            }
            if let Some(text) = aux {
                let v: Value = serde_json::from_str(text).map_err(|e| format!("parse:{e}"))?;
                if v["disableAllHooks"] == true {
                    f.disabled.push("disable_all".into());
                }
            }
            Ok(())
        }
        // Antigravity：共用文件里本项目那一项逐个事件与当前写法比对；`"enabled": false` 为停用（官方 Hooks 文档）。
        Tool::Antigravity => {
            let data = match main {
                Some(t) => serde_json::from_str::<Value>(t).map_err(|e| format!("parse:{e}"))?,
                None => json!({}),
            };
            if !data.is_object() {
                return Err("parse:顶层不是对象".into());
            }
            let ours = &data[ANTIGRAVITY_NAME];
            let fresh = antigravity_entry();
            for ev in events(Tool::Antigravity) {
                match ours.get(*ev) {
                    None => f.missing.push(ev.to_string()),
                    Some(v) if *v != fresh[*ev] => f.outdated.push(ev.to_string()),
                    _ => {}
                }
            }
            if ours["enabled"] == false {
                f.disabled.push("entry_disabled".into());
            }
            Ok(())
        }
        // OpenCode 没有停用插件的开关：只有齐全、缺失、不是当前版本三种。
        Tool::OpenCode => {
            match main {
                None => f.missing.push("plugin".into()),
                Some(t) if t != OPENCODE_PLUGIN => f.outdated.push("plugin".into()),
                _ => {}
            }
            Ok(())
        }
        Tool::Claude | Tool::Codex | Tool::Grok => {
            let data = match main {
                Some(t) => serde_json::from_str::<Value>(t).map_err(|e| format!("parse:{e}"))?,
                None => json!({}),
            };
            if !data.is_object() {
                return Err("parse:顶层不是对象".into());
            }
            let found = ours_by_event(s.tool, &data)?;
            for ev in events(s.tool) {
                match found.get(*ev) {
                    None => f.missing.push(ev.to_string()),
                    Some(o) if !o.current => f.outdated.push(ev.to_string()),
                    _ => {}
                }
            }
            match s.tool {
                Tool::Claude => {
                    if data["disableAllHooks"] == true {
                        f.disabled.push("disable_all".into());
                    }
                }
                Tool::Codex => inspect_codex(&s.home, &found, aux.unwrap_or(""), f),
                Tool::Grok => {
                    let lines = disabled_lines(aux.unwrap_or(""));
                    for ev in events(s.tool) {
                        if let Some(o) = found.get(*ev) {
                            if lines.contains(&grok_name(ev, o.group, o.index)) {
                                f.disabled.push(format!("entry_disabled:{ev}"));
                            }
                        }
                    }
                }
                Tool::Kimi | Tool::OpenCode | Tool::Copilot | Tool::Antigravity => unreachable!(),
            }
            Ok(())
        }
    }
}

fn inspect_codex(home: &str, found: &HashMap<String, Ours>, config: &str, f: &mut Findings) {
    let lines = toml_lines(config);
    if !features_off(&lines).is_empty() {
        f.disabled.push("features_off".into());
    }
    if lines.iter().any(|l| l.table.is_none() && l.key.as_deref() == Some("allow_managed_hooks_only")
        && l.value.as_deref() == Some("true"))
    {
        f.policy.push("policy:allow_managed_hooks_only".into());
    }
    let state = codex_state(&lines);
    for ev in events(Tool::Codex) {
        let Some(o) = found.get(*ev) else { continue };
        let entry = state.get(&codex_key(home, ev, o.group, o.index));
        if entry.and_then(|e| e.enabled) == Some(false) {
            f.disabled.push(format!("entry_disabled:{ev}"));
        }
        let trusted = match (&o.hash, entry.and_then(|e| e.trusted_hash.as_deref())) {
            (Some(h), Some(t)) => t == format!("sha256:{h}"),
            _ => false,
        };
        if !trusted {
            f.untrusted.push(ev.to_string());
        }
    }
}

/// 按命令特征认本项目的条目，不靠标记：Kimi 登录、登出时整份重写 `config.toml`，注释（含标记）全丢、
/// 字符串改用双引号；它自己改 hooks 段时还可能只留下一行标记（2026-11-16 实测 2.6.1，docs/design.md 3.1.5 节）。
/// 每个事件恰好一条当前写法就算正常，有没有标记都一样。
fn inspect_kimi(text: &str, f: &mut Findings) {
    if kimi_inline_hooks(text) {
        return f.unreadable.push("hooks_inline".into());
    }
    let mut stale = false;
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let ours = kimi_ours(text);
    for e in &ours {
        match e.event().filter(|ev| events(Tool::Kimi).contains(ev)) {
            Some(ev) => {
                *seen.entry(ev).or_default() += 1;
                stale |= !e.current(ev);
            }
            None => stale = true,
        }
    }
    stale |= seen.values().any(|n| *n > 1);
    for ev in events(Tool::Kimi) {
        if !seen.contains_key(ev) {
            f.missing.push(ev.to_string());
        }
    }
    if f.missing.is_empty() && stale {
        f.outdated.push("block".into());
    }
}

/// 本项目在某个事件下的条目：位置、命令是否为当前写法、Codex 信任摘要（只有字段都认识时才算）。
struct Ours {
    group: usize,
    index: usize,
    current: bool,
    hash: Option<String>,
}

/// 每个事件取第一个本项目条目。结构不是预期的 JSON 形状时报错（不碰这个文件）。
fn ours_by_event(tool: Tool, data: &Value) -> Result<HashMap<String, Ours>, String> {
    let mut out = HashMap::new();
    let Some(hooks) = data.get("hooks") else { return Ok(out) };
    let hooks = hooks.as_object().ok_or("parse:hooks 不是对象")?;
    let cmd = command(tool);
    for (ev, groups) in hooks {
        let groups = groups.as_array().ok_or_else(|| format!("parse:hooks.{ev} 不是数组"))?;
        for (g, group) in groups.iter().enumerate() {
            let group = group.as_object().ok_or_else(|| format!("parse:hooks.{ev}[{g}] 不是对象"))?;
            let Some(list) = group.get("hooks") else { continue };
            let list = list.as_array().ok_or_else(|| format!("parse:hooks.{ev}[{g}].hooks 不是数组"))?;
            for (i, h) in list.iter().enumerate() {
                if out.contains_key(ev) || !is_ours(h) {
                    continue;
                }
                let current = h["command"] == cmd.as_str();
                let hash = (tool == Tool::Codex)
                    .then(|| codex_hash(ev, h, group.get("matcher")))
                    .flatten();
                out.insert(ev.clone(), Ours { group: g, index: i, current, hash });
            }
        }
    }
    Ok(out)
}

// ---- Codex 信任记录（docs/s0-hook-compat.md「防失效取证」） ----

/// Codex 0.162.1 的信任摘要：对键名排序的紧凑 JSON `{"event_name","hooks":[{"async","command","timeout","type"}]}`
/// 求 SHA-256（`matcher` 存在时一并计入）。条目含其他字段时算不出，返回 None（按未信任处理，不猜）。
fn codex_hash(event: &str, h: &Value, matcher: Option<&Value>) -> Option<String> {
    let obj = h.as_object()?;
    if obj.keys().any(|k| !matches!(k.as_str(), "type" | "command" | "timeout")) {
        return None;
    }
    let mut handler = Map::new();
    handler.insert("async".into(), Value::Bool(false));
    handler.insert("command".into(), obj.get("command")?.clone());
    if let Some(t) = obj.get("timeout") {
        handler.insert("timeout".into(), t.clone());
    }
    handler.insert("type".into(), obj.get("type")?.clone());
    let mut root = Map::new();
    root.insert("event_name".into(), Value::String(snake(event)));
    root.insert("hooks".into(), Value::Array(vec![Value::Object(handler)]));
    if let Some(m) = matcher {
        root.insert("matcher".into(), m.clone());
    }
    Some(hex(&sha256(sorted_compact(&Value::Object(root)).as_bytes())))
}

/// 键名排序的紧凑 JSON（与 preserve_order 无关，自己排序）。
fn sorted_compact(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", Value::String((*k).clone()), sorted_compact(&m[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(sorted_compact).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

fn codex_key(home: &str, event: &str, group: usize, index: usize) -> String {
    format!("{}/.codex/hooks.json:{}:{group}:{index}", home.trim_end_matches('/'), snake(event))
}

#[derive(Default)]
struct HookState {
    trusted_hash: Option<String>,
    enabled: Option<bool>,
    /// `enabled = false` 所在行号（改为 true 用）。
    disabled_line: Option<usize>,
}

fn codex_state(lines: &[TomlLine]) -> HashMap<String, HookState> {
    let mut out: HashMap<String, HookState> = HashMap::new();
    for (n, l) in lines.iter().enumerate() {
        let Some(key) = l.table.as_deref().and_then(state_key) else { continue };
        let entry = out.entry(key).or_default();
        match (l.key.as_deref(), l.value.as_deref()) {
            (Some("trusted_hash"), Some(v)) => entry.trusted_hash = unquote(v),
            (Some("enabled"), Some("false")) => {
                entry.enabled = Some(false);
                entry.disabled_line = Some(n);
            }
            (Some("enabled"), Some("true")) => {
                entry.enabled = Some(true);
                entry.disabled_line = None;
            }
            _ => {}
        }
    }
    out
}

/// `hooks.state."<键>"` → 键。
fn state_key(table: &str) -> Option<String> {
    let rest = table.strip_prefix("hooks.state.")?;
    unquote(rest)
}

fn unquote(v: &str) -> Option<String> {
    let v = v.trim();
    let inner = v
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))?;
    Some(inner.to_owned())
}

/// 关闭 Hook 功能的行：`[features]` 下的 `hooks` / `codex_hooks` 为 false，或顶层的 `features.hooks` 写法。
fn features_off(lines: &[TomlLine]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            let key = match l.table.as_deref() {
                Some("features") => l.key.as_deref(),
                None => l.key.as_deref().and_then(|k| k.strip_prefix("features.")),
                _ => None,
            };
            matches!(key, Some("hooks" | "codex_hooks")) && l.value.as_deref() == Some("false")
        })
        .map(|(n, _)| n)
        .collect()
}

/// 只认本项目需要的 TOML 子集：表头、`键 = 值` 行，跳过注释与多行字符串内部。不做通用解析。
struct TomlLine {
    table: Option<String>,
    key: Option<String>,
    value: Option<String>,
    /// 这一行是表头（`[x]` 或 `[[x]]`）。
    header: bool,
    /// 这一行在多行字符串里（属于上一个键的值）。
    cont: bool,
}

fn toml_lines(text: &str) -> Vec<TomlLine> {
    let mut out = Vec::new();
    let mut table: Option<String> = None;
    let mut multi: Option<&str> = None;
    for raw in text.split('\n') {
        let line = raw.trim();
        if let Some(delim) = multi {
            if line.matches(delim).count() % 2 == 1 {
                multi = None;
            }
            out.push(TomlLine { table: table.clone(), key: None, value: None, header: false, cont: true });
            continue;
        }
        if line.starts_with('[') {
            let inner = line.trim_start_matches('[');
            let end = inner.find(']').unwrap_or(inner.len());
            table = Some(inner[..end].trim().to_owned());
            out.push(TomlLine { table: table.clone(), key: None, value: None, header: true, cont: false });
            continue;
        }
        let (mut key, mut value) = (None, None);
        if !line.starts_with('#') {
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                for delim in ["\"\"\"", "'''"] {
                    if v.matches(delim).count() % 2 == 1 {
                        multi = Some(delim);
                    }
                }
                key = Some(k.trim().trim_matches('"').to_owned());
                value = Some(strip_comment(v).to_owned());
            }
        }
        out.push(TomlLine { table: table.clone(), key, value, header: false, cont: false });
    }
    out
}

/// 去掉值后面的注释（值不是字符串时；字符串里的 `#` 保留）。
fn strip_comment(v: &str) -> &str {
    if v.starts_with('"') || v.starts_with('\'') {
        return v;
    }
    v.split('#').next().unwrap_or(v).trim()
}

/// 把第 `n` 行的 `false` 改为 `true`，其余字符不变（与 Codex 自己重新启用时写 `enabled = true` 一致）。
fn flip_false(text: &str, rows: &[usize]) -> String {
    text.split('\n')
        .enumerate()
        .map(|(n, l)| {
            if rows.contains(&n) {
                match l.find('=') {
                    Some(eq) => format!("{}{}", &l[..eq], l[eq..].replacen("false", "true", 1)),
                    None => l.to_owned(),
                }
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- Grok 停用记录 ----

/// Grok 1.0.52 `~/.grok/disabled-hooks` 中个人 Hook 的名字（2026-10-15 取证）。
fn grok_name(event: &str, group: usize, index: usize) -> String {
    format!("global/scyllaui:{}[{group}].hooks[{index}]", snake(event))
}

fn disabled_lines(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect()
}

// ---- Kimi 标记块 ----

fn kimi_block_text() -> String {
    let mut lines = vec![KIMI_BEGIN.to_owned()];
    for ev in events(Tool::Kimi) {
        lines.push("[[hooks]]".into());
        lines.push(format!("event = \"{ev}\""));
        lines.push(format!("command = '{}'", command(Tool::Kimi)));
        lines.push(format!("timeout = {TIMEOUT}"));
        lines.push(String::new());
    }
    *lines.last_mut().unwrap() = KIMI_END.to_owned();
    lines.join("\n") + "\n"
}

/// 顶层把 hooks 写成行内数组（`hooks = [ {…} ]`）：后面再追加 `[[hooks]]` 是非法 TOML，整份配置 Kimi 都读不了
/// （2026-11-16 实测 `kimi doctor` 报 Invalid TOML），所以这种写法不碰。
fn kimi_inline_hooks(text: &str) -> bool {
    toml_lines(text).iter().any(|l| l.table.is_none() && l.key.as_deref() == Some("hooks"))
}

/// Kimi 配置里命令是本项目的一个 `[[hooks]]` 元素：从表头到最后一行内容的行号（含两端）与各键解码后的值。
struct KimiEntry {
    start: usize,
    end: usize,
    keys: Vec<(String, Option<String>)>,
}

impl KimiEntry {
    fn get(&self, key: &str) -> Option<&str> {
        self.keys.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
    }

    fn event(&self) -> Option<&str> {
        self.get("event")
    }

    /// 与本项目写入的完全相同：只有这三个键，值也相同（不论 Kimi 改成哪种引号）。
    fn current(&self, ev: &str) -> bool {
        self.keys.len() == 3
            && self.get("event") == Some(ev)
            && self.get("command") == Some(command(Tool::Kimi).as_str())
            && self.get("timeout") == Some(TIMEOUT.to_string().as_str())
    }
}

/// 配置里命令是本项目的 `[[hooks]]` 元素，标记块内外都算。`[hooks.x]` 子表算上一个元素的一部分。
fn kimi_ours(text: &str) -> Vec<KimiEntry> {
    let raw: Vec<&str> = text.split('\n').collect();
    let mut out = vec![];
    let mut cur: Option<KimiEntry> = None;
    let finish = |e: Option<KimiEntry>, out: &mut Vec<KimiEntry>| {
        if let Some(e) = e {
            let ours = e.keys.iter().any(|(k, v)| k == "command" && v.as_deref().is_some_and(is_ours_text));
            if ours {
                out.push(e);
            }
        }
    };
    for (n, l) in toml_lines(text).iter().enumerate() {
        if l.header {
            let table = l.table.as_deref().unwrap_or("");
            if table.starts_with("hooks.") && cur.is_some() {
                cur.as_mut().unwrap().end = n;
                continue;
            }
            finish(cur.take(), &mut out);
            if table == "hooks" && raw[n].trim_start().starts_with("[[") {
                cur = Some(KimiEntry { start: n, end: n, keys: vec![] });
            }
            continue;
        }
        let Some(e) = cur.as_mut() else { continue };
        if let Some(k) = &l.key {
            let v = l.value.as_deref().unwrap_or("");
            // 多行字符串不解码，原文参与识别（不会是当前写法）。
            e.keys.push((k.clone(), Some(toml_scalar(v).unwrap_or_else(|| v.to_owned()))));
            e.end = n;
        } else if l.cont {
            e.end = n;
        }
    }
    finish(cur.take(), &mut out);
    out
}

/// 单行 TOML 值解码：基本字符串（含转义）、字面字符串，其余原样（数字等）；多行字符串或写法不认识为 None。
fn toml_scalar(v: &str) -> Option<String> {
    let v = v.trim();
    if v.starts_with("\"\"\"") || v.starts_with("'''") {
        return None;
    }
    let tail_ok = |rest: &str| rest.trim().is_empty() || rest.trim_start().starts_with('#');
    if let Some(body) = v.strip_prefix('\'') {
        let end = body.find('\'')?;
        return tail_ok(&body[end + 1..]).then(|| body[..end].to_owned());
    }
    let Some(body) = v.strip_prefix('"') else { return Some(strip_comment(v).to_owned()) };
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return tail_ok(&body[i + 1..]).then_some(out),
            '\\' => {
                let (_, e) = chars.next()?;
                match e {
                    'b' => out.push('\u{8}'),
                    't' => out.push('\t'),
                    'n' => out.push('\n'),
                    'f' => out.push('\u{c}'),
                    'r' => out.push('\r'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'u' | 'U' => {
                        let len = if e == 'u' { 4 } else { 8 };
                        let hex: String = (0..len).filter_map(|_| chars.next().map(|(_, h)| h)).collect();
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    _ => return None,
                }
            }
            _ => out.push(c),
        }
    }
    None
}

/// Kimi 配置里属于本项目的行。
struct KimiOwn {
    /// 每行是否属于本项目：条目（表头到最后一行内容）、单独成行的标记、夹在这些行之间的空行。
    mask: Vec<bool>,
    /// 不在成对标记之间的条目数（Kimi 重写配置后留下的）。只用于向用户说明删的是哪些。
    loose: usize,
    /// 有标记行（成对或落单）。
    markers: bool,
}

/// 标记只是给人看的装饰：成对、落单、没有，都不影响认条目；删除时标记行一并删，标记之间别人的内容不动。
fn kimi_own(text: &str) -> KimiOwn {
    let lines: Vec<&str> = text.split('\n').collect();
    let scanned = toml_lines(text);
    let mut mask = vec![false; lines.len()];
    let mut inside = vec![false; lines.len()];
    let mut open = None;
    let mut markers = false;
    for (n, l) in lines.iter().enumerate() {
        let t = l.trim();
        if scanned[n].cont || (t != KIMI_BEGIN && t != KIMI_END) {
            continue;
        }
        mask[n] = true;
        markers = true;
        if t == KIMI_BEGIN {
            open = Some(n);
        } else if let Some(s) = open.take() {
            inside[s..=n].fill(true);
        }
    }
    let ours = kimi_ours(text);
    for e in &ours {
        mask[e.start..=e.end].fill(true);
    }
    // 夹在本项目两行之间的空行（块内条目之间的分隔）。
    let mut n = 0;
    while n < lines.len() {
        if mask[n] || !lines[n].trim().is_empty() {
            n += 1;
            continue;
        }
        let end = (n..lines.len()).find(|i| !lines[*i].trim().is_empty() || mask[*i]).unwrap_or(lines.len());
        if n > 0 && mask[n - 1] && end < lines.len() && mask[end] {
            mask[n..end].fill(true);
        }
        n = end;
    }
    KimiOwn { mask, loose: ours.iter().filter(|e| !inside[e.start]).count(), markers }
}

/// 删去本项目的全部行。每段删除之后的空行一并删；这一段含标记或删到了文件末尾时，前面的空行也删（与
/// s0-hooks.py 的 `toml_strip` 逐字节相同）；只是块外散落的条目时前面的空行留着，前后两段之间仍隔一个空行。
fn kimi_strip(text: &str, own: &KimiOwn) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let blank = |n: usize| lines[n].trim().is_empty();
    let marker = |n: usize| own.mask[n] && matches!(lines[n].trim(), KIMI_BEGIN | KIMI_END);
    let mut drop = own.mask.clone();
    let mut n = 0;
    while n < lines.len() {
        if !drop[n] {
            n += 1;
            continue;
        }
        let start = n;
        while n < lines.len() && (drop[n] || blank(n)) {
            drop[n] = true;
            n += 1;
        }
        if n == lines.len() || (start..n).any(marker) {
            let mut b = start;
            while b > 0 && blank(b - 1) {
                b -= 1;
                drop[b] = true;
            }
        }
    }
    let kept: Vec<&str> = lines.iter().zip(&drop).filter(|(_, d)| !**d).map(|(l, _)| *l).collect();
    let mut rest = kept.join("\n");
    if text.ends_with('\n') && !rest.ends_with('\n') {
        rest.push('\n');
    }
    rest
}

// ---- 生成改动 ----

/// 一个文件的改动：`new` 为 None 表示删除（只用于本项目自建的 Grok 文件）。
#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub old: Option<String>,
    pub new: Option<String>,
    /// 写入前备份为 `<文件>.scyllaui-bak-<时间>`（本项目自建的 Grok 文件不备份）。
    pub backup: bool,
    /// 删除后父目录为空就一并删除（本项目创建的 `~/.grok/hooks`）。
    pub remove_empty_parent: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub tool: Tool,
    pub files: Vec<FileChange>,
    /// 给用户看的改动说明（中文，命令行原样输出）。
    pub actions: Vec<String>,
    /// 与 `actions` 一一对应的结构化说明 `{kind, …}`，App 按界面语言组织文字（M2 第 6 块，docs/protocol.md 第 7 节）。
    pub steps: Vec<Value>,
    /// 打开全局开关后一并恢复的、用户自己的 Hook 条数。
    pub other_hooks_enabled: usize,
    /// 修复后仍需在 Codex `/hooks` 里信任的条数。
    pub trust_needed: usize,
}

impl Plan {
    pub fn diff(&self) -> String {
        self.files
            .iter()
            .map(|c| file_diff(self.tool, c))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "tool": self.tool.as_str(),
            "files": self.files.iter().map(|c| json!({"path": c.path.display().to_string(), "backup": c.backup && c.old.is_some(),
                "create": c.old.is_none(), "delete": c.new.is_none()})).collect::<Vec<_>>(),
            "diff": self.diff(),
            "actions": self.actions,
            "steps": self.steps,
            "other_hooks_enabled": self.other_hooks_enabled,
            "trust_needed": self.trust_needed,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 安装或修复：补齐条目、就地更新写法、解除停用、打开全局开关。
    Repair,
    /// 一键撤销：只删除本项目的条目。
    Uninstall,
}

/// 生成改动；没有要改的为 `Ok(None)`。策略锁定、读不了或格式不认识时为 `Err`（不碰文件）。
pub fn plan(s: &Snapshot, action: Action) -> Result<Option<Plan>, String> {
    let status = analyze(s);
    match status.state {
        State::Policy if action == Action::Repair => return Err(status.problems.join("; ")),
        State::Unreadable => return Err(status.problems.join("; ")),
        _ => {}
    }
    let lay = layout(s.tool, Path::new(&s.home));
    let main = s.main.clone().map_err(|e| format!("read:{e}"))?;
    let aux = s.aux.clone().map_err(|e| format!("read:{e}"))?;
    let mut p = Plan { tool: s.tool, files: vec![], actions: vec![], steps: vec![], other_hooks_enabled: 0, trust_needed: 0 };
    match (s.tool, action) {
        (Tool::Kimi, _) => {
            let old = main.clone().unwrap_or_default();
            let own = kimi_own(&old);
            let base = kimi_strip(&old, &own);
            let new = match action {
                // 去掉本项目的全部条目与标记后在末尾追加一个新块，不会重复。配置已正常时不动（避免仅为挪动位置
                // 或补回标记而改写；没有标记时撤销按命令特征照样删得掉）。
                Action::Repair if status.state == State::Ok => old.clone(),
                Action::Repair => {
                    let b = if base.ends_with('\n') || base.is_empty() { base } else { base + "\n" };
                    format!("{b}\n{}", kimi_block_text())
                }
                Action::Uninstall => base,
            };
            if new != old {
                if own.loose > 0 {
                    act(&mut p, format!("删除本项目在标记块外的 {} 条（Kimi 登录、登出时重写配置会去掉标记）", own.loose),
                        json!({"kind": "uninstall", "count": own.loose}));
                }
                let n = events(Tool::Kimi).len();
                match action {
                    Action::Repair => act(&mut p, format!("在文件末尾写入本项目的标记块（{n} 条 [[hooks]]）"),
                                          json!({"kind": "kimi_block_write", "count": n})),
                    Action::Uninstall if own.markers =>
                        act(&mut p, "删除本项目的标记块".into(), json!({"kind": "kimi_block_remove"})),
                    Action::Uninstall => {}
                }
                push_change(&mut p, &lay.main, main, Some(new), true, false);
            }
        }
        (Tool::Grok, Action::Repair) => {
            let fresh = py_json(&install_json(json!({}), Tool::Grok)) + "\n";
            if main.as_deref() != Some(fresh.as_str()) {
                let n = events(Tool::Grok).len();
                act(&mut p, format!("写入 {}（{n} 条）", lay.main.display()),
                    json!({"kind": "grok_write", "path": lay.main.display().to_string(), "count": n}));
                push_change(&mut p, &lay.main, main.clone(), Some(fresh), false, false);
            }
            // 新写的文件每个事件都在 [0].hooks[0]。
            let ours: Vec<String> = events(Tool::Grok).iter().map(|e| grok_name(e, 0, 0)).collect();
            if let (Some(path), Some(text)) = (&lay.aux, aux.as_deref()) {
                let removed: Vec<&str> = text.lines().filter(|l| ours.contains(&l.trim().to_owned())).collect();
                if !removed.is_empty() {
                    let kept: Vec<&str> = text.lines().filter(|l| !ours.contains(&l.trim().to_owned())).collect();
                    let mut new = kept.join("\n");
                    if !kept.is_empty() && text.ends_with('\n') {
                        new.push('\n');
                    }
                    act(&mut p, format!("重新启用 {} 条（从 disabled-hooks 删除本项目的行）", removed.len()),
                        json!({"kind": "grok_reenable", "count": removed.len()}));
                    push_change(&mut p, path, Some(text.to_owned()), Some(new), true, false);
                }
            }
        }
        (Tool::Copilot, Action::Repair) => {
            let fresh = copilot_file();
            // 没装 Copilot（`~/.copilot` 不存在）时不建，同 OpenCode。
            if status.state != State::Absent && main.as_deref() != Some(fresh.as_str()) {
                let n = events(Tool::Copilot).len();
                act(&mut p, format!("写入 {}（{n} 条）", lay.main.display()),
                    json!({"kind": "copilot_write", "path": lay.main.display().to_string(), "count": n}));
                push_change(&mut p, &lay.main, main.clone(), Some(fresh), false, false);
            }
            if let (Some(path), Some(text)) = (&lay.aux, aux.as_deref()) {
                let mut v: Value = serde_json::from_str(text).map_err(|e| format!("parse:{e}"))?;
                if v["disableAllHooks"] == true {
                    v["disableAllHooks"] = Value::Bool(false);
                    act(&mut p, "打开全局开关（settings.json 的 disableAllHooks 改为 false）".into(),
                        json!({"kind": "copilot_enable_all"}));
                    let eol = if text.ends_with('\n') { "\n" } else { "" };
                    push_change(&mut p, path, Some(text.to_owned()), Some(py_json(&v) + eol), true, false);
                }
            }
        }
        (Tool::Copilot, Action::Uninstall) => {
            if main.is_some() {
                act(&mut p, format!("删除 {}", lay.main.display()), json!({"kind": "copilot_delete", "path": lay.main.display().to_string()}));
                push_change(&mut p, &lay.main, main, None, false, true);
            }
        }
        (Tool::Antigravity, _) => {
            let old = main.clone();
            let mut data = match old.as_deref() {
                Some(t) => serde_json::from_str::<Value>(t).map_err(|e| format!("parse:{e}"))?,
                None => json!({}),
            };
            let obj = data.as_object_mut().ok_or("parse:顶层不是对象")?;
            match action {
                // 没装 Antigravity CLI 时不建，同 OpenCode、Copilot。已有本项目那一项就地替换（保持在原位置），没有则加在末尾。
                Action::Repair if status.state != State::Absent && status.state != State::Ok => {
                    if status.problems.iter().any(|x| x.starts_with("missing:") || x.starts_with("outdated:")) {
                        let n = events(Tool::Antigravity).len();
                        act(&mut p, format!("在 {} 写入本项目的「{ANTIGRAVITY_NAME}」一项（{n} 条）；这份文件 Antigravity 的 App、IDE 也读，这几条只在 ScyllaUI 的终端里起作用", lay.main.display()),
                            json!({"kind": "antigravity_write", "count": n, "path": lay.main.display().to_string()}));
                    }
                    if status.problems.iter().any(|x| x == "entry_disabled") {
                        act(&mut p, "重新启用本项目的一项（去掉 \"enabled\": false）".into(), json!({"kind": "antigravity_enable"}));
                    }
                    obj.insert(ANTIGRAVITY_NAME.into(), antigravity_entry());
                }
                Action::Repair => {}
                Action::Uninstall => {
                    if obj.shift_remove(ANTIGRAVITY_NAME).is_some() {
                        act(&mut p, format!("从 {} 删除本项目的「{ANTIGRAVITY_NAME}」一项，您自己的 Hook 不动", lay.main.display()),
                            json!({"kind": "antigravity_remove", "path": lay.main.display().to_string()}));
                    }
                }
            }
            let new = match obj.is_empty() {
                // 删掉本项目那一项后什么也不剩：文件本是安装时建的，删除，与安装前一致。
                true if action == Action::Uninstall => None,
                _ => Some(py_json(&data) + if old.as_deref().is_some_and(|t| !t.ends_with('\n')) { "" } else { "\n" }),
            };
            if !p.actions.is_empty() {
                push_change(&mut p, &lay.main, old, new, true, false);
            }
        }
        (Tool::OpenCode, Action::Repair) => {
            // 没装 OpenCode（`~/.config/opencode` 不存在）时不建：`~/.config` 是多个程序共用的目录。
            if status.state != State::Absent && main.as_deref() != Some(OPENCODE_PLUGIN) {
                act(&mut p, format!("写入插件 {}", lay.main.display()),
                    json!({"kind": "opencode_write", "path": lay.main.display().to_string()}));
                push_change(&mut p, &lay.main, main.clone(), Some(OPENCODE_PLUGIN.to_owned()), false, false);
            }
        }
        (Tool::OpenCode, Action::Uninstall) => {
            if main.is_some() {
                act(&mut p, format!("删除插件 {}", lay.main.display()),
                    json!({"kind": "opencode_delete", "path": lay.main.display().to_string()}));
                push_change(&mut p, &lay.main, main, None, false, true);
            }
        }
        (Tool::Grok, Action::Uninstall) => {
            if main.is_some() {
                act(&mut p, format!("删除 {}", lay.main.display()), json!({"kind": "grok_delete", "path": lay.main.display().to_string()}));
                push_change(&mut p, &lay.main, main, None, false, true);
            }
        }
        (Tool::Claude | Tool::Codex, _) => {
            let mut data = match main.as_deref() {
                Some(t) => serde_json::from_str::<Value>(t).map_err(|e| format!("parse:{e}"))?,
                None => json!({}),
            };
            let before = data.clone();
            let found = ours_by_event(s.tool, &data)?;
            match action {
                Action::Repair => {
                    let added: Vec<&str> = events(s.tool).iter().copied().filter(|e| !found.contains_key(*e)).collect();
                    let updated = found.values().filter(|o| !o.current).count();
                    data = install_json(data, s.tool);
                    if !added.is_empty() {
                        act(&mut p, format!("追加 {} 条：{}", added.len(), added.join("、")),
                            json!({"kind": "append", "count": added.len(), "names": added}));
                    }
                    if updated > 0 {
                        act(&mut p, format!("{updated} 条改为当前写法"), json!({"kind": "rewrite", "count": updated}));
                    }
                    if s.tool == Tool::Claude && data["disableAllHooks"] == true {
                        data["disableAllHooks"] = Value::Bool(false);
                        p.other_hooks_enabled = count_handlers(&data, false);
                        let r = p.other_hooks_enabled;
                        act(&mut p, format!("打开全局开关（disableAllHooks 改为 false）{}", also_restores(r)),
                            json!({"kind": "claude_enable_all", "restores": r}));
                    }
                }
                Action::Uninstall => {
                    if !found.is_empty() {
                        data = uninstall_json(data);
                        act(&mut p, format!("删除本项目的 {} 条", found.len()), json!({"kind": "uninstall", "count": found.len()}));
                    }
                }
            }
            if data != before {
                let eol = if main.as_deref().is_some_and(|t| !t.ends_with('\n')) { "" } else { "\n" };
                push_change(&mut p, &lay.main, main.clone(), Some(py_json(&data) + eol), true, false);
            }
            if s.tool == Tool::Codex && action == Action::Repair {
                codex_repair_config(s, &data, aux.as_deref(), &lay, &mut p);
            }
        }
    }
    Ok((!p.files.is_empty() || p.trust_needed > 0).then_some(p))
}

/// Codex：解除本项目条目的逐条停用、打开 `features.hooks`，并预测修复后要信任的条数。
fn codex_repair_config(s: &Snapshot, data: &Value, config: Option<&str>, lay: &Layout, p: &mut Plan) {
    let text = config.unwrap_or("");
    let lines = toml_lines(text);
    let state = codex_state(&lines);
    let found = ours_by_event(Tool::Codex, data).unwrap_or_default();
    let mut rows = features_off(&lines);
    let feature_rows = rows.len();
    // 说明里写实际的键（`hooks` 或旧名 `codex_hooks`，以及顶层的 `features.` 写法）。
    let feature_keys: Vec<String> = rows
        .iter()
        .filter_map(|n| {
            let l = &lines[*n];
            let k = l.key.as_deref()?;
            Some(if l.table.as_deref() == Some("features") { format!("[features] {k}") } else { k.to_owned() })
        })
        .collect();
    let mut reenabled = vec![];
    for ev in events(Tool::Codex) {
        let Some(o) = found.get(*ev) else { continue };
        let entry = state.get(&codex_key(&s.home, ev, o.group, o.index));
        if let Some(n) = entry.and_then(|e| e.disabled_line) {
            rows.push(n);
            reenabled.push(*ev);
        }
        let trusted = matches!((&o.hash, entry.and_then(|e| e.trusted_hash.as_deref())),
            (Some(h), Some(t)) if t == format!("sha256:{h}"));
        if !trusted {
            p.trust_needed += 1;
        }
    }
    if feature_rows > 0 {
        p.other_hooks_enabled = count_handlers(data, false);
        let r = p.other_hooks_enabled;
        act(p, format!("打开 Hook 功能（config.toml 的 {} 改为 true）{}", feature_keys.join("、"), also_restores(r)),
            json!({"kind": "codex_feature", "keys": feature_keys, "restores": r}));
    }
    if !reenabled.is_empty() {
        act(p, format!("重新启用 {} 条：{}（config.toml 中该条的 enabled 改为 true）", reenabled.len(), reenabled.join("、")),
            json!({"kind": "codex_reenable", "count": reenabled.len(), "names": reenabled}));
    }
    if !rows.is_empty() {
        if let Some(path) = &lay.aux {
            push_change(p, path, config.map(str::to_owned), Some(flip_false(text, &rows)), true, false);
        }
    }
    if p.trust_needed > 0 {
        // 有文件要改时是修复之后的事；没有时就是现在要做的事。
        let after = !p.files.is_empty();
        let when = if after { "修复后" } else { "" };
        let n = p.trust_needed;
        act(p, format!("{when}需要在 Codex 里输入 /hooks，按提示信任 ScyllaUI 的 {n} 条 Hook"),
            json!({"kind": "codex_trust", "count": n, "after_repair": after}));
    }
}

/// 记一条改动说明：中文句子与结构化的 `steps` 同时写入，保持一一对应。
fn act(p: &mut Plan, text: String, step: Value) {
    p.actions.push(text);
    p.steps.push(step);
}

/// 打开全局开关的副作用说明；没有用户自己的 Hook 时不说。
fn also_restores(n: usize) -> String {
    if n == 0 { String::new() } else { format!("，同时恢复您自己的 {n} 条 Hook") }
}

fn push_change(p: &mut Plan, path: &Path, old: Option<String>, new: Option<String>, backup: bool, remove_empty_parent: bool) {
    p.files.push(FileChange { path: path.to_path_buf(), old, new, backup, remove_empty_parent });
}

/// Copilot 的自建 Hook 文件（官方格式 `{"version": 1, "hooks": {…}}`，命令键 `bash`，超时 `timeoutSec`）。
fn copilot_file() -> String {
    let hooks: Map<String, Value> = events(Tool::Copilot)
        .iter()
        .map(|ev| (ev.to_string(), json!([{"type": "command", "bash": command_for(Tool::Copilot, ev), "timeoutSec": TIMEOUT}])))
        .collect();
    py_json(&json!({"version": 1, "hooks": hooks})) + "\n"
}

/// Antigravity 共用文件里本项目那一项（官方格式：装的三个事件直接是处理器列表，工具事件才有 `{matcher, hooks}` 组；
/// `timeout` 单位为秒）。
fn antigravity_entry() -> Value {
    let entry: Map<String, Value> = events(Tool::Antigravity)
        .iter()
        .map(|ev| (ev.to_string(), json!([{"type": "command", "command": command_for(Tool::Antigravity, ev), "timeout": TIMEOUT}])))
        .collect();
    Value::Object(entry)
}

/// 与 s0-hooks.py 的 `json_install` 相同：已有本项目条目就地改为当前写法，缺的追加到末尾。
fn install_json(mut data: Value, tool: Tool) -> Value {
    let cmd = command(tool);
    if !data.is_object() {
        return data;
    }
    let hooks = data.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else { return data };
    for ev in events(tool) {
        let groups = hooks.entry(ev.to_string()).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else { continue };
        let mut found = false;
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                for h in list.iter_mut().filter(|h| is_ours(h)) {
                    h["command"] = Value::String(cmd.clone());
                    found = true;
                }
            }
        }
        if !found {
            groups.push(json!({"hooks": [{"type": "command", "command": cmd, "timeout": TIMEOUT}]}));
        }
    }
    data
}

/// 与 s0-hooks.py 的 `json_uninstall` 相同：删掉本项目条目，空的组、空的事件与空的 `hooks` 一并删除。
fn uninstall_json(mut data: Value) -> Value {
    let Some(obj) = data.as_object_mut() else { return data };
    if let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        let keys: Vec<String> = hooks.keys().cloned().collect();
        for ev in keys {
            let kept: Vec<Value> = hooks[&ev]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|mut g| {
                    let list: Vec<Value> = g.get("hooks").and_then(Value::as_array).cloned().unwrap_or_default()
                        .into_iter().filter(|h| !is_ours(h)).collect();
                    let empty = list.is_empty();
                    if let Some(o) = g.as_object_mut() {
                        o.insert("hooks".into(), Value::Array(list));
                    }
                    (!empty).then_some(g)
                })
                .collect();
            if kept.is_empty() {
                hooks.shift_remove(&ev);
            } else {
                hooks.insert(ev, Value::Array(kept));
            }
        }
        if hooks.is_empty() {
            obj.shift_remove("hooks");
        }
    }
    data
}

/// Hook 条目数；`ours=false` 时只数用户自己的。
fn count_handlers(data: &Value, ours: bool) -> usize {
    data.get("hooks")
        .and_then(Value::as_object)
        .map(|h| {
            h.values()
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|x| is_ours(x) == ours)
                .count()
        })
        .unwrap_or(0)
}

/// 与 Python `json.dumps(indent=2, ensure_ascii=False)` 相同的排版（serde_json 的 pretty 与之一致：两格缩进、`": "`、
/// 空容器写作 `{}` / `[]`、非 ASCII 原样输出、控制字符转义写法相同）。
fn py_json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

// ---- 展示改动 ----

fn file_diff(tool: Tool, c: &FileChange) -> String {
    let name = c.path.display().to_string();
    let old = c.old.as_deref().unwrap_or("");
    let new = c.new.as_deref().unwrap_or("");
    match (tool, c.path.extension().and_then(|e| e.to_str())) {
        // Kimi 的 config.toml 可能含凭据：只显示本项目的行（条目与标记，块内块外都算），不显示别人的内容。
        (Tool::Kimi, _) => {
            let own = |t: &str| -> Vec<String> {
                let mask = kimi_own(t).mask;
                t.split('\n').zip(mask).filter(|(_, m)| *m).map(|(l, _)| l.to_owned()).collect()
            };
            let mut out = format!("--- {name}（原）\n+++ {name}（新）\n");
            for l in own(old) {
                out += &format!("-{l}\n");
            }
            for l in own(new) {
                out += &format!("+{l}\n");
            }
            out
        }
        // Codex 的 config.toml 也可能含凭据：只显示改动的行。
        (Tool::Codex, Some("toml")) => unified_diff(&name, old, new, 0),
        _ => unified_diff(&name, old, new, 1),
    }
}

/// 按行的统一 diff（最长公共子序列）。只用于展示。
pub fn unified_diff(name: &str, old: &str, new: &str, ctx: usize) -> String {
    let a: Vec<&str> = if old.is_empty() { vec![] } else { old.split_inclusive('\n').collect() };
    let b: Vec<&str> = if new.is_empty() { vec![] } else { new.split_inclusive('\n').collect() };
    let (n, m) = (a.len(), b.len());
    if n * m > 4_000_000 {
        return format!("--- {name}\n（文件过大，不显示逐行改动）\n");
    }
    // lcs[i][j]：a[i..] 与 b[j..] 的最长公共子序列长度。
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    // 操作序列：(' ' | '-' | '+', 文本)。
    let mut ops: Vec<(char, &str)> = vec![];
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // 同样长时先删后加，与常见 diff 的顺序一致。
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| o.0 != ' ').map(|(k, _)| k).collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {name}（原）\n+++ {name}（新）\n");
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(ctx);
        let mut end = changed[k];
        while k < changed.len() && changed[k] <= end + 2 * ctx + 1 {
            end = changed[k];
            k += 1;
        }
        let end = (end + ctx + 1).min(ops.len());
        out += "@@\n";
        for (op, text) in &ops[start..end] {
            out.push(*op);
            out += text;
            if !text.ends_with('\n') {
                out += "\n\\ 文件末尾没有换行\n";
            }
        }
    }
    out
}

// ---- 读写文件 ----

fn read_text(path: &Path) -> FileText {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| "不是 UTF-8 文本".to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 读取一款 CLI 的配置。`home` 原样使用（见 `Snapshot`）。
pub fn read(tool: Tool, home: &str) -> Snapshot {
    let lay = layout(tool, Path::new(home));
    Snapshot {
        tool,
        home: home.to_owned(),
        dir_exists: lay.dir.is_dir(),
        main: read_text(&lay.main),
        aux: lay.aux.as_deref().map(read_text).unwrap_or(Ok(None)),
        policy: lay.policy.iter().map(|p| (p.clone(), read_text(p))).collect(),
    }
}

/// 改动涉及的文件此刻内容的指纹：生成改动后、写入前比对，不一致说明文件刚被别人改过。
pub fn fingerprint(plans: &[Plan]) -> String {
    let mut buf = Vec::new();
    for p in plans {
        for c in &p.files {
            buf.extend_from_slice(c.path.display().to_string().as_bytes());
            buf.push(0);
            match &c.old {
                Some(t) => {
                    buf.push(1);
                    buf.extend_from_slice(t.as_bytes());
                }
                None => buf.push(2),
            }
            buf.push(0);
        }
    }
    hex(&sha256(&buf))
}

/// 写入改动：先逐个核对文件仍是生成改动时的内容（有一个不符就整体放弃，不写任何文件），再备份、写临时文件、改名替换。
/// 返回新建的备份文件。`stamp` 为本地时间 `YYYYmmdd-HHMMSS`。
pub fn apply(plans: &[Plan], stamp: &str) -> Result<Vec<PathBuf>, String> {
    for c in plans.iter().flat_map(|p| &p.files) {
        let now = read_text(&c.path).map_err(|e| format!("{}：{e}", c.path.display()))?;
        if now != c.old {
            return Err(format!("conflict:{}", c.path.display()));
        }
    }
    let mut backups = vec![];
    for c in plans.iter().flat_map(|p| &p.files) {
        if c.backup && c.old.is_some() {
            let mut bak = PathBuf::from(format!("{}.scyllaui-bak-{stamp}", c.path.display()));
            let mut n = 1;
            while bak.exists() {
                bak = PathBuf::from(format!("{}.scyllaui-bak-{stamp}-{n}", c.path.display()));
                n += 1;
            }
            std::fs::copy(&c.path, &bak).map_err(|e| format!("备份 {} 失败：{e}", c.path.display()))?;
            backups.push(bak);
        }
        match &c.new {
            None => {
                std::fs::remove_file(&c.path).map_err(|e| format!("删除 {} 失败：{e}", c.path.display()))?;
                if c.remove_empty_parent {
                    if let Some(dir) = c.path.parent() {
                        if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
                            let _ = std::fs::remove_dir(dir);
                        }
                    }
                }
            }
            Some(text) => write_atomic(&c.path, text)?,
        }
    }
    Ok(backups)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let err = |e: std::io::Error| format!("写入 {} 失败：{e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let tmp = PathBuf::from(format!("{}.scyllaui-tmp", path.display()));
    std::fs::write(&tmp, text).map_err(err)?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777)).map_err(err)?;
    }
    std::fs::rename(&tmp, path).map_err(err)
}

/// 本地时间 `YYYYmmdd-HHMMSS`（备份文件名，与 s0-hooks.py 相同）。
pub fn local_stamp() -> String {
    // SAFETY: time 与 localtime_r 只读写传入的局部变量。
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
        )
    }
}

// ---- SHA-256：macOS 系统库 CommonCrypto（libSystem 自带，不加依赖） ----

extern "C" {
    fn CC_SHA256(data: *const libc::c_void, len: u32, md: *mut u8) -> *mut u8;
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    assert!(data.len() <= u32::MAX as usize, "SHA-256 输入过大");
    // SAFETY: data 有效且长度不超过 u32；out 为 32 字节，CC_SHA256 只写 32 字节。
    unsafe { CC_SHA256(data.as_ptr().cast(), data.len() as u32, out.as_mut_ptr()) };
    out
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// 本机真实 `~/.codex/config.toml` 记录的信任摘要（2026-10-15，内容只含本项目自己的命令文本）。这组摘要是改名前的命令录下的，
    /// 用来核对摘要算法与 Codex 一致；改名后的命令文本只把环境变量前缀换了，算法不变（2026-11-05，D8）。
    #[test]
    fn codex_hash_matches_recorded_trust() {
        let legacy = ["PIT", "WALL_"].concat();
        let h = json!({"type": "command", "command": command(Tool::Codex).replace("SCYLLAUI_", &legacy), "timeout": 2});
        assert_eq!(codex_hash("SessionStart", &h, None).unwrap(), "98730da02970d850f989894da7dfaa7c56b26997260fc77a79b499468d4c69b5");
        assert_eq!(codex_hash("Stop", &h, None).unwrap(), "07a8ce6b264629292dbd3c1076224916f048f39d68a679bd03191926ada4dc7a");
        assert_eq!(codex_hash("Interrupt", &h, None).unwrap(), "dd2cfef1d45ed01fc4887a71dbde95daf5f188d149bf39f4e2fafac49ccc0130");
        assert_eq!(codex_hash("UserPromptSubmit", &h, None).unwrap(), "31206fe9647c1a02d3b4f5937af138636dcf073dc9afbce2415a4e788e31e120");
        let odd = json!({"type": "command", "command": "x", "statusMessage": "y"});
        assert_eq!(codex_hash("Stop", &odd, None), None, "不认识的字段不猜");
    }

    #[test]
    fn snake_case_event_names() {
        assert_eq!(snake("UserPromptSubmit"), "user_prompt_submit");
        assert_eq!(snake("StopCancelled"), "stop_cancelled");
        assert_eq!(snake("Stop"), "stop");
    }

    #[test]
    fn toml_scanner_skips_multiline_strings_and_comments() {
        let t = "a = \"\"\"\n[hooks.state.\"x\"]\nenabled = false\n\"\"\"\n# [features]\n[features]\nhooks = false # 关\n";
        let lines = toml_lines(t);
        assert!(codex_state(&lines).is_empty(), "多行字符串里的内容不算");
        assert_eq!(features_off(&lines), vec![6]);
        assert_eq!(flip_false(t, &[6]).lines().nth(6), Some("hooks = true # 关"));
        let top = toml_lines("features.codex_hooks = false\n");
        assert_eq!(features_off(&top), vec![0]);
    }

    fn snap(tool: Tool, main: Option<&str>, aux: Option<&str>) -> Snapshot {
        Snapshot {
            tool,
            home: "/h".into(),
            dir_exists: true,
            main: Ok(main.map(str::to_owned)),
            aux: Ok(aux.map(str::to_owned)),
            policy: vec![],
        }
    }

    fn installed(tool: Tool) -> String {
        py_json(&install_json(json!({}), tool)) + "\n"
    }

    /// 修复后的内容（按改动替换对应文件）。
    fn after(s: &Snapshot, p: &Plan) -> Snapshot {
        let lay = layout(s.tool, Path::new(&s.home));
        let pick = |path: &Path, cur: &FileText| -> FileText {
            match p.files.iter().find(|c| c.path == path) {
                Some(c) => Ok(c.new.clone()),
                None => cur.clone(),
            }
        };
        Snapshot {
            tool: s.tool,
            home: s.home.clone(),
            dir_exists: true,
            main: pick(&lay.main, &s.main),
            aux: lay.aux.as_deref().map(|a| pick(a, &s.aux)).unwrap_or(Ok(None)),
            policy: s.policy.clone(),
        }
    }

    /// Codex 配置：给 hooks.json 中本项目每个条目写上当前位置的信任记录。
    fn trusted_config(hooks_json: &str) -> String {
        let data: Value = serde_json::from_str(hooks_json).unwrap();
        let found = ours_by_event(Tool::Codex, &data).unwrap();
        let mut out = String::from("[hooks.state]\n");
        for ev in events(Tool::Codex) {
            let o = &found[*ev];
            out += &format!("\n[hooks.state.\"{}\"]\ntrusted_hash = \"sha256:{}\"\n", codex_key("/h", ev, o.group, o.index), o.hash.as_ref().unwrap());
        }
        out
    }

    #[test]
    fn claude_states_and_repairs() {
        let ok = installed(Tool::Claude);
        assert_eq!(analyze(&snap(Tool::Claude, Some(&ok), None)).state, State::Ok);
        // 删掉一个事件的条目。
        let mut v: Value = serde_json::from_str(&ok).unwrap();
        v["hooks"].as_object_mut().unwrap().shift_remove("SessionStart");
        let s = snap(Tool::Claude, Some(&py_json(&v)), None);
        let st = analyze(&s);
        assert_eq!((st.state, st.problems.clone(), st.fixable), (State::Missing, vec!["missing:SessionStart".to_owned()], true));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(analyze(&after(&s, &p)).state, State::Ok);
        // 旧写法：就地更新，位置不变。
        let old_form = ok.replace("${SCYLLAUI_BIN:-}\\\" hook", "$SCYLLAUI_BIN\\\" hook");
        assert_ne!(old_form, ok);
        let s = snap(Tool::Claude, Some(&old_form), None);
        assert_eq!(analyze(&s).state, State::Missing);
        assert!(analyze(&s).problems.iter().all(|x| x.starts_with("outdated:")));
        assert_eq!(after(&s, &plan(&s, Action::Repair).unwrap().unwrap()).main.unwrap().unwrap(), ok);
        // 全局开关：修复打开它，并说明同时恢复用户自己的 Hook。
        let mut v: Value = serde_json::from_str(&ok).unwrap();
        v["disableAllHooks"] = json!(true);
        v["hooks"]["Stop"].as_array_mut().unwrap().insert(0, json!({"hooks": [{"type": "command", "command": "say hi"}]}));
        let s = snap(Tool::Claude, Some(&py_json(&v)), None);
        let st = analyze(&s);
        assert_eq!((st.state, st.problems), (State::Disabled, vec!["disable_all".to_owned()]));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.other_hooks_enabled, 1);
        assert!(p.actions.iter().any(|a| a.ends_with("同时恢复您自己的 1 条 Hook")));
        let diff = p.diff();
        let (minus, plus) = (diff.find("-  \"disableAllHooks\": true").unwrap(), diff.find("+  \"disableAllHooks\": false").unwrap());
        assert!(minus < plus, "先删后加：{diff}");
        let fixed = after(&s, &p);
        assert_eq!(analyze(&fixed).state, State::Ok);
        assert!(fixed.main.unwrap().unwrap().contains("\"disableAllHooks\": false"));
        // 组织策略：不修复，只能撤销。
        let mut s = snap(Tool::Claude, Some(&ok), None);
        s.policy = vec![(PathBuf::from(CLAUDE_MANAGED), Ok(Some("{\"allowManagedHooksOnly\": true}".into())))];
        let st = analyze(&s);
        assert_eq!((st.state, st.fixable), (State::Policy, false));
        assert!(plan(&s, Action::Repair).is_err());
        assert!(plan(&s, Action::Uninstall).unwrap().is_some());
        // 读不了、格式不认识：不碰文件。
        for bad in ["{", "[]", "{\"hooks\": []}", "{\"hooks\": {\"Stop\": {}}}"] {
            let s = snap(Tool::Claude, Some(bad), None);
            assert_eq!(analyze(&s).state, State::Unreadable, "{bad}");
            assert!(plan(&s, Action::Repair).is_err(), "{bad}");
        }
        let mut s = snap(Tool::Claude, None, None);
        s.main = Err("Permission denied".into());
        assert_eq!(analyze(&s).state, State::Unreadable);
        // 没装：不提示。
        let mut s = snap(Tool::Claude, None, None);
        s.dir_exists = false;
        assert_eq!(analyze(&s).state, State::Absent);
        // 目录在、文件不在：全部缺失。
        assert_eq!(analyze(&snap(Tool::Claude, None, None)).problems.len(), events(Tool::Claude).len());
    }

    /// Copilot（M4b 块 9c）：自建文件按全文比对，每条命令带 `--event`；settings.json 的 disableAllHooks 为停用，修复时打开；
    /// 撤销删除自建文件；没装时不建。
    #[test]
    fn copilot_hooks_file_states() {
        let s = snap(Tool::Copilot, None, None);
        assert_eq!(analyze(&s).problems, vec!["missing:hooks_file".to_owned()]);
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        let text = p.files[0].new.clone().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 1);
        for ev in events(Tool::Copilot) {
            let cmd = v["hooks"][*ev][0]["bash"].as_str().unwrap();
            assert!(cmd.ends_with(&format!("hook --config copilot --event {ev}; fi")) && is_ours_text(cmd), "{cmd}");
            assert_eq!(v["hooks"][*ev][0]["timeoutSec"], TIMEOUT);
        }
        assert!(v["hooks"].get("permissionRequest").is_none());
        assert_eq!(analyze(&after(&s, &p)).state, State::Ok);
        let off = snap(Tool::Copilot, Some(&text), Some("{\n  \"disableAllHooks\": true\n}\n"));
        let st = analyze(&off);
        assert_eq!((st.state, st.fixable), (State::Disabled, true));
        let p = plan(&off, Action::Repair).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "copilot_enable_all"})]);
        assert!(p.files[0].backup && p.files[0].new.as_deref().unwrap().contains("\"disableAllHooks\": false"));
        let p = plan(&snap(Tool::Copilot, Some(&text), None), Action::Uninstall).unwrap().unwrap();
        assert_eq!(p.steps[0]["kind"], "copilot_delete");
        assert!(p.files[0].new.is_none() && p.files[0].remove_empty_parent);
        let mut absent = snap(Tool::Copilot, None, None);
        absent.dir_exists = false;
        assert!(plan(&absent, Action::Repair).unwrap().is_none(), "没装 Copilot 不建 ~/.copilot");
    }

    /// Antigravity（M4b 块 9b）：共用文件里只增删本项目那一项，用户自己的项原样保留、位置不变；`"enabled": false` 为停用；
    /// 撤销后与安装前逐字节一致（文件本不存在时删除）；没装 CLI 时不建。
    #[test]
    fn antigravity_shared_file() {
        let s = snap(Tool::Antigravity, None, None);
        assert_eq!(analyze(&s).problems, vec!["missing:SessionStart", "missing:PreInvocation", "missing:Stop"]);
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "antigravity_write", "count": 3, "path": "/h/.gemini/config/hooks.json"})]);
        let text = p.files[0].new.clone().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        for ev in events(Tool::Antigravity) {
            let h = &v[ANTIGRAVITY_NAME][*ev][0];
            let cmd = h["command"].as_str().unwrap();
            assert!(cmd.starts_with("if [ -n \"${SCYLLAUI_SOCK:-}\" ]") && cmd.ends_with(&format!("hook --config antigravity --event {ev}; fi")), "{cmd}");
            assert_eq!((h["type"].as_str(), h["timeout"].as_u64()), (Some("command"), Some(TIMEOUT)));
        }
        assert!(v[ANTIGRAVITY_NAME].get("PreToolUse").is_none(), "不装 PreToolUse：失败会拦下工具调用");
        assert_eq!(analyze(&after(&s, &p)).state, State::Ok);
        let p = plan(&after(&s, &p), Action::Uninstall).unwrap().unwrap();
        assert!(p.files[0].new.is_none(), "文件本是安装时建的，撤销即删除");

        // 用户已有自己的 Hook：保留，本项目那一项加在末尾；撤销后与原文一致。
        let mine = "{\n  \"lint\": {\n    \"Stop\": [\n      {\n        \"command\": \"./lint.sh\"\n      }\n    ]\n  }\n}\n";
        let s = snap(Tool::Antigravity, Some(mine), None);
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert!(p.files[0].backup);
        let text = p.files[0].new.clone().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["lint", ANTIGRAVITY_NAME]);
        let un = plan(&snap(Tool::Antigravity, Some(&text), None), Action::Uninstall).unwrap().unwrap();
        assert_eq!(un.files[0].new.as_deref(), Some(mine));

        // 停用、旧写法：修复就地替换，位置不变。
        let mut off: Value = serde_json::from_str(&text).unwrap();
        off[ANTIGRAVITY_NAME]["enabled"] = json!(false);
        off[ANTIGRAVITY_NAME]["Stop"][0]["timeout"] = json!(30);
        let off = snap(Tool::Antigravity, Some(&(py_json(&json!({ANTIGRAVITY_NAME: off[ANTIGRAVITY_NAME].clone(), "lint": v["lint"].clone()})) + "\n")), None);
        let st = analyze(&off);
        assert_eq!((st.state, st.problems.clone()), (State::Missing, vec!["outdated:Stop".to_owned(), "entry_disabled".to_owned()]));
        let p = plan(&off, Action::Repair).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "antigravity_write", "count": 3, "path": "/h/.gemini/config/hooks.json"}),
                                 json!({"kind": "antigravity_enable"})]);
        let fixed: Value = serde_json::from_str(p.files[0].new.as_deref().unwrap()).unwrap();
        assert_eq!(fixed.as_object().unwrap().keys().collect::<Vec<_>>(), vec![ANTIGRAVITY_NAME, "lint"]);
        assert!(fixed[ANTIGRAVITY_NAME].get("enabled").is_none());
        assert_eq!(analyze(&after(&off, &p)).state, State::Ok);
        assert!(plan(&after(&off, &p), Action::Repair).unwrap().is_none(), "已正常时不改");

        let mut absent = snap(Tool::Antigravity, None, None);
        absent.dir_exists = false;
        assert_eq!(analyze(&absent).state, State::Absent);
        assert!(plan(&absent, Action::Repair).unwrap().is_none(), "没装 Antigravity CLI 不建 ~/.gemini/config/hooks.json");
        assert!(analyze(&snap(Tool::Antigravity, Some("[1]"), None)).state == State::Unreadable);
    }

    /// OpenCode 插件（M4b 块 9a）：缺失、不是当前版本、齐全三种；修复整份写入、撤销删除，都不备份；没装时不建。
    #[test]
    fn opencode_plugin_states() {
        let s = snap(Tool::OpenCode, None, None);
        let st = analyze(&s);
        assert_eq!((st.state, st.fixable, st.problems.clone()), (State::Missing, true, vec!["missing:plugin".to_owned()]));
        assert!(st.path.ends_with("/.config/opencode/plugins/scyllaui.js"));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "opencode_write", "path": "/h/.config/opencode/plugins/scyllaui.js"})]);
        assert!(!p.files[0].backup && p.files[0].old.is_none());
        assert_eq!(analyze(&after(&s, &p)).state, State::Ok);
        assert!(plan(&after(&s, &p), Action::Repair).unwrap().is_none(), "已是当前版本不改");
        let old = snap(Tool::OpenCode, Some("// 旧版插件\n"), None);
        assert_eq!(analyze(&old).problems, vec!["outdated:plugin".to_owned()]);
        let p = plan(&snap(Tool::OpenCode, Some(OPENCODE_PLUGIN), None), Action::Uninstall).unwrap().unwrap();
        assert_eq!(p.steps[0]["kind"], "opencode_delete");
        assert!(p.files[0].new.is_none() && p.files[0].remove_empty_parent);
        assert!(plan(&s, Action::Uninstall).unwrap().is_none(), "没有插件时撤销无改动");
        let mut absent = snap(Tool::OpenCode, None, None);
        absent.dir_exists = false;
        assert_eq!(analyze(&absent).state, State::Absent);
        assert!(plan(&absent, Action::Repair).unwrap().is_none(), "没装 OpenCode 不建 ~/.config/opencode");
        // 插件只转发适配器认得的事件。
        for ev in events(Tool::OpenCode).iter().filter(|e| **e != "chat.message" && **e != "session.prompt") {
            assert!(OPENCODE_PLUGIN.contains(&format!("\"{ev}\"")), "{ev}");
        }
    }

    #[test]
    fn codex_trust_disable_and_features() {
        let hooks = installed(Tool::Codex);
        let cfg = trusted_config(&hooks);
        assert_eq!(analyze(&snap(Tool::Codex, Some(&hooks), Some(&cfg))).state, State::Ok);
        // 没有信任记录：未信任，修复不改文件，只说明要信任几条。
        let s = snap(Tool::Codex, Some(&hooks), None);
        let st = analyze(&s);
        assert_eq!((st.state, st.fixable, st.problems.len()), (State::Untrusted, false, 7));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert!(p.files.is_empty());
        assert_eq!(p.trust_needed, 7);
        assert_eq!(p.actions, vec!["需要在 Codex 里输入 /hooks，按提示信任 ScyllaUI 的 7 条 Hook".to_owned()]);
        // 逐条停用（Codex /hooks 写 enabled = false）：修复改为 enabled = true，与 Codex 自己重新启用的写法相同。
        let key = codex_key("/h", "SessionStart", 0, 0);
        let disabled = cfg.replace(
            &format!("[hooks.state.\"{key}\"]\ntrusted_hash"),
            &format!("[hooks.state.\"{key}\"]\nenabled = false\ntrusted_hash"),
        );
        assert_ne!(disabled, cfg);
        let s = snap(Tool::Codex, Some(&hooks), Some(&disabled));
        let st = analyze(&s);
        assert_eq!((st.state, st.problems.clone()), (State::Disabled, vec!["entry_disabled:SessionStart".to_owned()]));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!((p.files.len(), p.trust_needed), (1, 0));
        let fixed = after(&s, &p);
        assert!(fixed.aux.as_ref().unwrap().as_ref().unwrap().contains("enabled = true\ntrusted_hash"));
        assert_eq!(analyze(&fixed).state, State::Ok);
        assert!(!p.diff().contains("trusted_hash"), "config.toml 只显示改动的行");
        // 全局开关。
        let off = format!("model = \"x\"\n\n[features]\nhooks = false\n\n{cfg}");
        let s = snap(Tool::Codex, Some(&hooks), Some(&off));
        assert_eq!(analyze(&s).problems, vec!["features_off".to_owned()]);
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert!(p.actions[0].starts_with("打开 Hook 功能（config.toml 的 [features] hooks 改为 true）"), "{:?}", p.actions);
        let fixed = after(&s, &p);
        assert!(fixed.aux.as_ref().unwrap().as_ref().unwrap().contains("[features]\nhooks = true\n"));
        // 旧名与行尾注释：说明写实际的键，注释保留。
        let old = snap(Tool::Codex, Some(&hooks), Some(&format!("[features]\ncodex_hooks = false # 旧名\n\n{cfg}")));
        let p = plan(&old, Action::Repair).unwrap().unwrap();
        assert!(p.actions[0].contains("[features] codex_hooks 改为 true"), "{:?}", p.actions);
        assert!(after(&old, &p).aux.unwrap().unwrap().starts_with("[features]\ncodex_hooks = true # 旧名\n"));
        assert_eq!(analyze(&fixed).state, State::Ok);
        // 策略。
        let s = snap(Tool::Codex, Some(&hooks), Some(&format!("allow_managed_hooks_only = true\n{cfg}")));
        assert_eq!(analyze(&s).state, State::Policy);
    }

    /// 删掉本项目条目后再修复：原来排在最后的，追加回原位，信任仍然有效；原来排在用户条目前面的，追加后换了位置，要重新信任。
    #[test]
    fn codex_repair_predicts_trust() {
        let user = json!({"hooks": [{"type": "command", "command": "say stop", "timeout": 2}]});
        // 用户在前、本项目在后（追加安装的常见情形）。
        let mut v = json!({"hooks": {"Stop": [user.clone()]}});
        v = install_json(v, Tool::Codex);
        let hooks = py_json(&v) + "\n";
        let cfg = trusted_config(&hooks);
        let mut gone = v.clone();
        gone["hooks"]["Stop"].as_array_mut().unwrap().pop();
        let s = snap(Tool::Codex, Some(&(py_json(&gone) + "\n")), Some(&cfg));
        assert_eq!(analyze(&s).problems, vec!["missing:Stop".to_owned()]);
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.trust_needed, 0, "追加回原位，信任仍匹配");
        assert_eq!(analyze(&after(&s, &p)).state, State::Ok);
        // 本项目在前、用户在后：删掉本项目的条目后用户的条目前移，修复追加到末尾，位置变了。
        let mut v = install_json(json!({}), Tool::Codex);
        v["hooks"]["Stop"].as_array_mut().unwrap().push(user);
        let cfg = trusted_config(&(py_json(&v) + "\n"));
        v["hooks"]["Stop"].as_array_mut().unwrap().remove(0);
        let s = snap(Tool::Codex, Some(&(py_json(&v) + "\n")), Some(&cfg));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.trust_needed, 1);
        assert!(p.actions.iter().any(|a| a.contains("/hooks")));
        assert_eq!(analyze(&after(&s, &p)).state, State::Untrusted);
    }

    #[test]
    fn kimi_block_states() {
        let user = "default_model = \"k\"\napi_key = \"secret-value\"\n";
        let s = snap(Tool::Kimi, Some(user), None);
        assert_eq!(analyze(&s).problems.len(), events(Tool::Kimi).len());
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert!(!p.diff().contains("secret-value"), "不显示原有内容");
        let fixed = after(&s, &p);
        assert_eq!(analyze(&fixed).state, State::Ok);
        let text = fixed.main.clone().unwrap().unwrap();
        // 块里改了一个字：算缺失，可修复。
        let edited = text.replace("timeout = 2\n\n[[hooks]]\nevent = \"Stop\"", "timeout = 9\n\n[[hooks]]\nevent = \"Stop\"");
        assert_ne!(edited, text);
        let st = analyze(&snap(Tool::Kimi, Some(&edited), None));
        assert_eq!((st.state, st.problems), (State::Missing, vec!["outdated:block".to_owned()]));
        // 撤销后与原文一致。
        let s = snap(Tool::Kimi, Some(&text), None);
        assert_eq!(after(&s, &plan(&s, Action::Uninstall).unwrap().unwrap()).main.unwrap().unwrap().trim_end(), user.trim_end());
    }

    /// Kimi 改设置时（登录写入托管配置、`/logout`）按这份清单严格校验 `[[hooks]].event`，不在其中的整份配置被拒，
    /// 而运行时与 `kimi doctor` 照常接受（2.6.1 包内 node-sdk `HOOK_EVENT_TYPES`；2026-11-15 新用户验收发现，
    /// 上游 MoonshotAI/kimi-code#4962）。本机已装版本的实时核对见 `src/scripts/cli-hook-schema-check.py`。
    const KIMI_STRICT_EVENTS: [&str; 16] = [
        "PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest", "PermissionResult", "UserPromptSubmit",
        "Stop", "StopFailure", "Interrupt", "SessionStart", "SessionEnd", "SubagentStart", "SubagentStop",
        "PreCompact", "PostCompact", "Notification",
    ];

    #[test]
    fn kimi_events_pass_kimi_strict_schema() {
        for ev in events(Tool::Kimi) {
            assert!(KIMI_STRICT_EVENTS.contains(ev), "{ev} 不在 Kimi 改设置时的事件清单里，装上后 /login 会失败");
        }
    }

    #[test]
    fn kimi_block_with_turn_started_is_outdated_and_repairable() {
        // 0.4.48 及更早写入的块多一条 TurnStarted：判为需要更新，修复后不再有它。
        let fresh = kimi_block_text();
        let old = fresh.replacen(
            "event = \"PermissionRequest\"",
            &format!("event = \"TurnStarted\"\ncommand = '{}'\ntimeout = 2\n\n[[hooks]]\nevent = \"PermissionRequest\"", command(Tool::Kimi)),
            1,
        );
        assert!(old.contains("TurnStarted"));
        let s = snap(Tool::Kimi, Some(&old), None);
        let st = analyze(&s);
        assert_eq!((st.state, st.problems), (State::Missing, vec!["outdated:block".to_owned()]));
        let fixed = after(&s, &plan(&s, Action::Repair).unwrap().unwrap());
        assert_eq!(analyze(&fixed).state, State::Ok);
        assert!(!fixed.main.unwrap().unwrap().contains("TurnStarted"));
    }

    /// Kimi 2.6.1 `/logout` 前后的真实文件（2026-11-16 临时 HOME 实测）：整份重写，注释与标记全丢，命令改为双引号。
    /// 两条用户自己的 Hook 在本项目条目前后各一条。
    const KIMI_BEFORE_LOGOUT: &str = include_str!("../../../../tests/hook-config/kimi-rewrite/before-logout.toml");
    const KIMI_AFTER_LOGOUT: &str = include_str!("../../../../tests/hook-config/kimi-rewrite/after-logout.toml");

    fn kimi_user_hooks(text: &str) -> Vec<String> {
        text.split("[[hooks]]").filter(|x| x.contains("my-hook.sh"))
            .map(|x| x.trim().split("\n\n").next().unwrap().to_owned()).collect()
    }

    #[test]
    fn kimi_fixture_is_a_real_rewrite() {
        assert!(KIMI_BEFORE_LOGOUT.contains(KIMI_BEGIN) && !KIMI_AFTER_LOGOUT.contains('#'));
        assert_eq!(analyze(&snap(Tool::Kimi, Some(KIMI_BEFORE_LOGOUT), None)).state, State::Ok);
    }

    #[test]
    fn kimi_without_markers_is_recognised_by_command() {
        let s = snap(Tool::Kimi, Some(KIMI_AFTER_LOGOUT), None);
        // 状态：条目都在、写法等价，算正常；修复不动文件（不会再追加一组）。
        assert_eq!(analyze(&s).state, State::Ok);
        assert_eq!(plan(&s, Action::Repair).unwrap(), None);
        // 撤销：删干净本项目的 10 条，用户自己的两条原样保留。
        let p = plan(&s, Action::Uninstall).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "uninstall", "count": events(Tool::Kimi).len()})]);
        let left = after(&s, &p).main.unwrap().unwrap();
        assert!(!left.contains("SCYLLAUI"), "{left}");
        assert_eq!(kimi_user_hooks(&left), kimi_user_hooks(KIMI_AFTER_LOGOUT));
        assert_eq!(kimi_user_hooks(&left).len(), 2);
        assert!(left.starts_with("default_model = \"\"\n") && left.trim_end().ends_with("command = \"~/bin/my-hook.sh stop\""), "{left}");
        // Kimi 写的文件以两个换行结尾，删后保持原样。
        assert_eq!(left.ends_with("\n\n"), KIMI_AFTER_LOGOUT.ends_with("\n\n"));
        assert!(!left.contains("\n\n\n"), "删除处不留多余空行：{left}");
        assert_eq!(analyze(&snap(Tool::Kimi, Some(&left), None)).problems.len(), events(Tool::Kimi).len());
    }

    #[test]
    fn kimi_duplicates_from_old_repair_are_collapsed() {
        // 0.4.49 及更早：标记丢失后修复会在末尾再追加一个标记块，每个事件两条。
        let dup = format!("{KIMI_AFTER_LOGOUT}\n{}", kimi_block_text());
        let s = snap(Tool::Kimi, Some(&dup), None);
        let st = analyze(&s);
        assert_eq!((st.state, st.problems, st.fixable), (State::Missing, vec!["outdated:block".to_owned()], true));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        assert_eq!(p.steps[0], json!({"kind": "uninstall", "count": events(Tool::Kimi).len()}));
        let fixed = after(&s, &p);
        let text = fixed.main.clone().unwrap().unwrap();
        assert_eq!(analyze(&fixed).state, State::Ok);
        assert_eq!(text.matches("hook --config kimi").count(), events(Tool::Kimi).len());
        assert_eq!(text.matches(KIMI_BEGIN).count(), 1);
        assert_eq!(kimi_user_hooks(&text), kimi_user_hooks(KIMI_AFTER_LOGOUT));
        // 再修复不变；撤销删掉块，用户的条目还在。
        assert_eq!(plan(&fixed, Action::Repair).unwrap(), None);
        let p = plan(&fixed, Action::Uninstall).unwrap().unwrap();
        assert_eq!(p.steps, vec![json!({"kind": "kimi_block_remove"})]);
        let left = after(&fixed, &p).main.unwrap().unwrap();
        assert!(!left.contains("SCYLLAUI") && kimi_user_hooks(&left).len() == 2);
    }

    #[test]
    fn kimi_partial_or_changed_loose_entries() {
        // 块外只剩一部分：缺的算缺失；修复删掉块外的，写一个完整的块。
        let only_stop = format!("x = 1\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"{}\"\ntimeout = 2\n",
                                command(Tool::Kimi).replace('"', "\\\""));
        let s = snap(Tool::Kimi, Some(&only_stop), None);
        assert_eq!(analyze(&s).problems.len(), events(Tool::Kimi).len() - 1);
        let fixed = after(&s, &plan(&s, Action::Repair).unwrap().unwrap());
        let text = fixed.main.clone().unwrap().unwrap();
        assert_eq!(analyze(&fixed).state, State::Ok);
        assert_eq!(text.matches("event = \"Stop\"").count(), 1);
        assert!(text.starts_with("x = 1\n\n# >>> scyllaui hooks >>>\n"), "{text}");
        // 块外的条目改了超时、多了键：算需要更新。
        for bad in [KIMI_AFTER_LOGOUT.replacen("timeout = 2", "timeout = 9", 1),
                    KIMI_AFTER_LOGOUT.replacen("timeout = 2", "timeout = 2\nmatcher = \"x\"", 1)] {
            let st = analyze(&snap(Tool::Kimi, Some(&bad), None));
            assert_eq!((st.state, st.problems), (State::Missing, vec!["outdated:block".to_owned()]));
        }
        // 别人的条目命令不含本项目特征：一律不认、不动。
        let foreign = "[[hooks]]\nevent = \"Stop\"\ncommand = \"echo SCYLLAUI\"\ntimeout = 2\n";
        let s = snap(Tool::Kimi, Some(foreign), None);
        assert_eq!(plan(&s, Action::Uninstall).unwrap(), None);
        let fixed = after(&s, &plan(&s, Action::Repair).unwrap().unwrap()).main.unwrap().unwrap();
        assert!(fixed.starts_with(foreign));
    }

    /// 预览里的减号行（不算空行，排序后比较：与别人条目相同的行如 `[[hooks]]` 只看条数）。
    fn minus_lines(p: &Plan) -> Vec<String> {
        let mut v: Vec<String> = p.diff().lines().filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .map(|l| l[1..].to_owned()).filter(|l| !l.trim().is_empty()).collect();
        v.sort();
        v
    }

    /// 实际删掉的行（不算空行，排序）：原文比新文多出来的。
    fn removed_lines(s: &Snapshot, p: &Plan) -> Vec<String> {
        let old = s.main.clone().unwrap().unwrap();
        let new = after(s, p).main.unwrap().unwrap();
        let mut left: Vec<&str> = new.lines().collect();
        let mut v: Vec<String> = old.lines().filter(|l| !l.trim().is_empty()).filter(|l| match left.iter().position(|x| x == l) {
            Some(i) => {
                left.remove(i);
                false
            }
            None => true,
        }).map(str::to_owned).collect();
        v.sort();
        v
    }

    #[test]
    fn kimi_preview_shows_every_removed_line() {
        // 标记丢失后撤销：预览要列出将删的 10 条，不带别人的内容。
        let s = snap(Tool::Kimi, Some(KIMI_AFTER_LOGOUT), None);
        let p = plan(&s, Action::Uninstall).unwrap().unwrap();
        assert_eq!(minus_lines(&p).len(), events(Tool::Kimi).len() * 4);
        assert_eq!(minus_lines(&p), removed_lines(&s, &p));
        assert!(!p.diff().contains("my-hook"), "{}", p.diff());
        // 有标记、旧版修复留下重复：两组都列出。
        for text in [KIMI_BEFORE_LOGOUT.to_owned(), format!("{KIMI_AFTER_LOGOUT}\n{}", kimi_block_text())] {
            let s = snap(Tool::Kimi, Some(&text), None);
            let p = plan(&s, Action::Uninstall).unwrap().unwrap();
            assert_eq!(minus_lines(&p), removed_lines(&s, &p));
            assert!(!p.diff().contains("my-hook") && !p.diff().contains("api_key"));
        }
    }

    #[test]
    fn kimi_markers_are_only_decoration() {
        let n = events(Tool::Kimi).len();
        // 只剩一行标记（Kimi 自己改 hooks 段时，段内的标记丢、段外的留）：照常判断、照常撤销，标记行一并删。
        for lone in [KIMI_BEGIN, KIMI_END] {
            let gone = if lone == KIMI_BEGIN { KIMI_END } else { KIMI_BEGIN };
            let text = KIMI_BEFORE_LOGOUT.replace(&format!("{gone}\n"), "");
            assert!(text.contains(lone) && !text.contains(gone));
            let s = snap(Tool::Kimi, Some(&text), None);
            assert_eq!(analyze(&s).state, State::Ok, "{lone}");
            assert_eq!(plan(&s, Action::Repair).unwrap(), None);
            let p = plan(&s, Action::Uninstall).unwrap().unwrap();
            assert_eq!(p.steps, vec![json!({"kind": "uninstall", "count": n}), json!({"kind": "kimi_block_remove"})]);
            let left = after(&s, &p).main.unwrap().unwrap();
            assert!(!left.to_lowercase().contains("scyllaui"), "{left}");
            assert_eq!(kimi_user_hooks(&left), kimi_user_hooks(KIMI_BEFORE_LOGOUT));
            assert!(left.contains("# my settings") && left.contains("[providers.\"managed:kimi-code\"]"));
            assert_eq!(minus_lines(&p), removed_lines(&s, &p));
        }
        // 成对标记之间有别人的条目（Kimi 往 hooks 段加了一条）：撤销不删它。
        let mine = "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"~/bin/my-hook.sh pre\"\n\n";
        let text = kimi_block_text().replacen("[[hooks]]\nevent = \"Stop\"", &format!("{mine}[[hooks]]\nevent = \"Stop\""), 1);
        let s = snap(Tool::Kimi, Some(&text), None);
        assert_eq!(analyze(&s).state, State::Ok);
        let p = plan(&s, Action::Uninstall).unwrap().unwrap();
        assert_eq!(after(&s, &p).main.unwrap().unwrap(), mine.trim_end().to_owned() + "\n");
        // 标记字样在多行字符串里：不是标记。
        let quoted = format!("note = \"\"\"\n{KIMI_BEGIN}\n\"\"\"\n");
        assert_eq!(plan(&snap(Tool::Kimi, Some(&quoted), None), Action::Uninstall).unwrap(), None);
    }

    #[test]
    fn kimi_inline_hooks_array_is_left_alone() {
        // 追加 [[hooks]] 会让整份配置成为非法 TOML（kimi doctor: Invalid TOML）：只说明，不碰。
        for text in ["hooks = [ { event = \"Stop\", command = \"~/bin/my-hook.sh stop\" } ]\n",
                     "default_model = \"k\"\nhooks = [\n  { event = \"Stop\", command = \"x\" },\n]\n\n[models.k]\nmodel = \"k\"\n"] {
            let s = snap(Tool::Kimi, Some(text), None);
            let st = analyze(&s);
            assert_eq!((st.state, st.fixable), (State::Unreadable, false));
            assert_eq!(st.problems, vec!["hooks_inline".to_owned()]);
            assert!(plan(&s, Action::Repair).is_err() && plan(&s, Action::Uninstall).is_err());
        }
        // 别的表里叫 hooks 的键不算。
        let other = "[features]\nhooks = true\n";
        assert_eq!(analyze(&snap(Tool::Kimi, Some(other), None)).state, State::Missing);
    }

    #[test]
    fn toml_scalar_decodes_kimi_rewrites() {
        assert_eq!(toml_scalar("'a \"b\"'").as_deref(), Some("a \"b\""));
        assert_eq!(toml_scalar("\"a \\\"b\\\" \\\\ \\u00e9\" # c").as_deref(), Some("a \"b\" \\ é"));
        assert_eq!(toml_scalar("2 # 秒").as_deref(), Some("2"));
        assert_eq!(toml_scalar("\"a\" b"), None);
        assert_eq!(toml_scalar("\"\"\"x"), None);
        let lines: Vec<String> = KIMI_AFTER_LOGOUT.lines().filter(|l| l.contains("SCYLLAUI")).map(str::to_owned).collect();
        assert_eq!(lines.len(), events(Tool::Kimi).len());
        for l in lines {
            assert_eq!(toml_scalar(l.split_once('=').unwrap().1).as_deref(), Some(command(Tool::Kimi).as_str()));
        }
    }

    #[test]
    fn grok_disabled_lines_and_policy() {
        let ok = installed(Tool::Grok);
        assert_eq!(analyze(&snap(Tool::Grok, Some(&ok), None)).state, State::Ok);
        assert_eq!(analyze(&snap(Tool::Grok, None, None)).state, State::Missing);
        let lines = "global/other:stop[0].hooks[0]\nglobal/scyllaui:session_start[0].hooks[0]\n";
        let s = snap(Tool::Grok, Some(&ok), Some(lines));
        let st = analyze(&s);
        assert_eq!((st.state, st.problems.clone()), (State::Disabled, vec!["entry_disabled:SessionStart".to_owned()]));
        let p = plan(&s, Action::Repair).unwrap().unwrap();
        let fixed = after(&s, &p);
        assert_eq!(fixed.aux.as_ref().unwrap().as_deref(), Some("global/other:stop[0].hooks[0]\n"), "只删本项目的行");
        assert_eq!(analyze(&fixed).state, State::Ok);
        // 全部删光时与 Grok 自己一样留空文件。
        let s = snap(Tool::Grok, Some(&ok), Some("global/scyllaui:stop[0].hooks[0]\n"));
        assert_eq!(after(&s, &plan(&s, Action::Repair).unwrap().unwrap()).aux.unwrap().as_deref(), Some(""));
        let mut s = snap(Tool::Grok, Some(&ok), None);
        s.policy = vec![(PathBuf::from("/h/.grok/requirements.toml"), Ok(Some("allow_managed_hooks_only = true\n".into())))];
        assert_eq!(analyze(&s).state, State::Policy);
        s.policy = vec![(PathBuf::from("/h/.grok/requirements.toml"), Ok(Some("allow_managed_hooks_only = false\n".into())))];
        assert_eq!(analyze(&s).state, State::Ok);
    }

    /// 结构化说明（M2 第 6 块）：每款工具在空的家目录里安装、写入后再撤销，`steps` 都与 `actions` 一一对应，种类符合协议。
    #[test]
    fn steps_follow_actions() {
        let dir = std::env::temp_dir().join(format!("scyllaui-hookcfg-steps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::create_dir_all(dir.join(".kimi-code")).unwrap();
        std::fs::create_dir_all(dir.join(".grok")).unwrap();
        let home = dir.to_str().unwrap();
        let kinds = |p: &Plan| p.steps.iter().map(|s| s["kind"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        for (tool, install, remove) in [
            (Tool::Claude, vec!["append"], vec!["uninstall"]),
            (Tool::Codex, vec!["append", "codex_trust"], vec!["uninstall"]),
            (Tool::Kimi, vec!["kimi_block_write"], vec!["kimi_block_remove"]),
            (Tool::Grok, vec!["grok_write"], vec!["grok_delete"]),
        ] {
            let p = plan(&read(tool, home), Action::Repair).unwrap().unwrap();
            assert_eq!(p.steps.len(), p.actions.len(), "{tool:?}");
            assert_eq!(kinds(&p), install, "{tool:?}");
            assert_eq!(p.to_json()["steps"].as_array().unwrap().len(), p.actions.len());
            if tool == Tool::Claude {
                assert_eq!(p.steps[0]["count"].as_u64(), Some(events(Tool::Claude).len() as u64));
                assert_eq!(p.steps[0]["names"].as_array().unwrap().len(), events(Tool::Claude).len());
            }
            apply(&[p], "20261017-000000").unwrap();
            let u = plan(&read(tool, home), Action::Uninstall).unwrap().unwrap();
            assert_eq!(kinds(&u), remove, "{tool:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生成改动后文件被别人改了：整体放弃，不写任何文件，也不留备份。
    #[test]
    fn apply_refuses_when_file_changed() {
        let dir = std::env::temp_dir().join(format!("scyllaui-hookcfg-conflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::create_dir_all(dir.join(".kimi-code")).unwrap();
        let settings = dir.join(".claude/settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        let home = dir.to_str().unwrap();
        let plans: Vec<Plan> = [Tool::Kimi, Tool::Claude]
            .iter()
            .filter_map(|t| plan(&read(*t, home), Action::Repair).unwrap())
            .collect();
        let fp = fingerprint(&plans);
        std::fs::write(&settings, "{\"theme\": \"x\"}\n").unwrap();
        let fresh: Vec<Plan> = [Tool::Kimi, Tool::Claude]
            .iter()
            .filter_map(|t| plan(&read(*t, home), Action::Repair).unwrap())
            .collect();
        assert_ne!(fingerprint(&fresh), fp, "文件变了，指纹随之变");
        let err = apply(&plans, "20261015-000000").unwrap_err();
        assert!(err.starts_with("conflict:"), "{err}");
        assert!(!dir.join(".kimi-code/config.toml").exists(), "排在前面的 Kimi 也没有写");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\"theme\": \"x\"}\n");
        assert!(std::fs::read_dir(dir.join(".claude")).unwrap().count() == 1, "没有留下备份或临时文件");
        // 按新内容重新生成后写入成功，权限保留。
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fresh: Vec<Plan> = [Tool::Claude].iter().filter_map(|t| plan(&read(*t, home), Action::Repair).unwrap()).collect();
        let backups = apply(&fresh, "20261015-000000").unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(analyze(&read(Tool::Claude, home)).state, State::Ok);
        // 同一秒内第二次写入不覆盖已有备份。
        let again = plan(&read(Tool::Claude, home), Action::Uninstall).unwrap().unwrap();
        let b2 = apply(&[again], "20261015-000000").unwrap();
        assert_ne!(b2[0], backups[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
