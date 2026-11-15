//! 提示词足迹（docs/design.md 10.5 节决策 10）：从各款 CLI 自己的会话记录读出用户提示词，按时间先后列出。只含纯逻辑；
//! 读文件在 scyllauid 的 `prompts.rs`。文件与格式与「最后一条回复」相同（`ReplySource`），2026-10-25 本机只读核对：
//! Claude 2.1.309、Codex 0.163.0、Kimi 2.3.1、Grok（updates.jsonl）；Copilot 1.0.105（events.jsonl，2026-10-30）。
//! 格式都不是官方承诺，认不出的行跳过。
//! 每条提示词只取一种记录（同一句可能在几种记录里各出现一次），并滤掉 CLI 注入的内部消息。

use super::reply::ReplySource;
use serde_json::Value;

/// 一条用户提示词；`at` 为毫秒时间戳，记录里没有时为 None。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub at: Option<i64>,
}

/// 会话记录（JSON Lines，可以只是文件末尾的一段完整行）中的用户提示词，先后顺序；相邻的同一句只留一条。
pub fn user_prompts(source: ReplySource, text: &str) -> Vec<Prompt> {
    let mut out: Vec<Prompt> = Vec::new();
    // Codex：旧版的 event_msg user_message 与 response_item 是同一句的两份，有前者时只用前者。
    let mut codex_items: Vec<Prompt> = Vec::new();
    let mut codex_events = false;
    // Grok：一条提示词分多片，同一个 promptIndex 的片段拼在一起。
    let mut grok: Option<(Option<i64>, Prompt)> = None;
    for line in text.lines() {
        // 先按关键字筛掉不可能是提示词的行（大的会话记录里多是回复与工具输出），只解析剩下的。
        let maybe = match source {
            ReplySource::ClaudeTranscript => line.contains("\"type\":\"user\""),
            ReplySource::CodexRollout => line.contains("\"role\":\"user\"") || line.contains("\"user_message\""),
            ReplySource::KimiWire => line.contains("\"turn.prompt\"") || line.contains("\"shell_command\""),
            ReplySource::GrokUpdates => true,
            ReplySource::CopilotEvents => line.contains("\"user.message\""),
            ReplySource::AntigravityTranscript => line.contains("\"USER_INPUT\""),
            // 库不是文本记录，由 scyllauid 查询后交给 `opencode_prompt`。
            ReplySource::OpenCodeDb => false,
            ReplySource::None => false,
        };
        if !maybe {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match source {
            ReplySource::None | ReplySource::OpenCodeDb => {}
            ReplySource::ClaudeTranscript => {
                if let Some(t) = claude_prompt(&v) {
                    push(&mut out, t, iso_ms(&v["timestamp"]));
                }
            }
            ReplySource::CodexRollout => {
                let p = &v["payload"];
                if v["type"] == "event_msg" && p["type"] == "user_message" {
                    if let Some(t) = p["message"].as_str() {
                        codex_events = true;
                        push(&mut out, t.to_owned(), iso_ms(&v["timestamp"]));
                    }
                } else if v["type"] == "response_item" && p["type"] == "message" && p["role"] == "user" {
                    if let Some(t) = codex_item(p) {
                        push(&mut codex_items, t, iso_ms(&v["timestamp"]));
                    }
                }
            }
            ReplySource::KimiWire => {
                if v["type"] == "turn.prompt" && v["origin"]["kind"] == "user" {
                    push(&mut out, parts(&v["input"], "text"), v["time"].as_i64());
                } else if v["type"] == "context.append_message"
                    && v["message"]["origin"]["kind"] == "shell_command"
                    && v["message"]["origin"]["phase"] == "input"
                {
                    // 输入行里的 `!命令`：记录为 `<bash-input>命令</bash-input>`，显示成用户输入时的样子。
                    if let Some(cmd) = between(&parts(&v["message"]["content"], "text"), "<bash-input>", "</bash-input>") {
                        push(&mut out, format!("!{}", cmd.trim()), v["time"].as_i64());
                    }
                }
            }
            ReplySource::AntigravityTranscript => {
                // agy 在原文外加了 `<USER_REQUEST>` 与当前时间等附注；子代理收到的任务、系统消息的 `source` 不是
                // USER_EXPLICIT（2026-11-01 实测 1.4.12）。
                if v["type"] == "USER_INPUT" && v["source"] == "USER_EXPLICIT" {
                    if let Some(c) = v["content"].as_str() {
                        push(&mut out, between(c, "<USER_REQUEST>", "</USER_REQUEST>").unwrap_or(c).to_owned(), iso_ms(&v["created_at"]));
                    }
                }
            }
            ReplySource::CopilotEvents => {
                // `content` 是用户输入的原文；`transformedContent` 另加了 CLI 注入的当前时间，不用。主代理派给子代理的任务也记作
                // `user.message`，带 `source: "agent-<会话 ID>"`，用户输入的没有 `source`（2026-11-13 长跑核对 1.0.105）。
                if v["type"] == "user.message" && !v["data"]["source"].as_str().is_some_and(|s| s.starts_with("agent-")) {
                    if let Some(t) = v["data"]["content"].as_str() {
                        push(&mut out, t.to_owned(), iso_ms(&v["timestamp"]));
                    }
                }
            }
            ReplySource::GrokUpdates => {
                let u = &v["params"]["update"];
                if u["sessionUpdate"] == "user_message_chunk" {
                    let index = u["_meta"]["promptIndex"].as_i64();
                    // 输入行里的 `!命令`：片段带 `_meta.bash_command`、没有 promptIndex，正文不含感叹号。
                    let piece = match u["content"]["_meta"]["bash_command"].as_str() {
                        Some(cmd) => format!("!{cmd}"),
                        None => u["content"]["text"].as_str().unwrap_or("").to_owned(),
                    };
                    let piece = piece.as_str();
                    match &mut grok {
                        Some((i, p)) if *i == index && index.is_some() => p.text.push_str(piece),
                        _ => {
                            if let Some((_, p)) = grok.take() {
                                push(&mut out, p.text, p.at);
                            }
                            grok = Some((index, Prompt { text: piece.to_owned(), at: iso_ms(&v["timestamp"]) }));
                        }
                    }
                } else if u["sessionUpdate"].is_string() {
                    if let Some((_, p)) = grok.take() {
                        push(&mut out, p.text, p.at);
                    }
                }
            }
        }
    }
    if let Some((_, p)) = grok {
        push(&mut out, p.text, p.at);
    }
    if source == ReplySource::CodexRollout && !codex_events {
        out = codex_items;
    }
    out
}

/// OpenCode：一条消息（`message.data`）与它的各段（`part.data`）。只认 `role=user`；正文为 `type=text` 的段，
/// OpenCode 自己补的段（`synthetic`，如引用文件时附上的文件内容）与被忽略的段（`ignored`）不算。不是提示词时为 None。
pub fn opencode_prompt(message: &Value, parts: &[Value]) -> Option<String> {
    if message["role"] != "user" {
        return None;
    }
    let text = parts
        .iter()
        .filter(|p| p["type"] == "text" && p["synthetic"] != true && p["ignored"] != true)
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// OpenCode 2 一条消息（`session_message` 的 `type` 与 `data`，2026-10-31 本机 2.2.4 核对）：`type=user` 的 `text`；
/// 输入行的 `!命令` 存为 `type=shell`，显示为 `!命令`。其余类型（助手、空闲、OpenCode 补的 `synthetic` 等）为 None。
pub fn opencode_v2_prompt(kind: &str, data: &Value) -> Option<String> {
    let text = match kind {
        "user" => data["text"].as_str()?.trim().to_owned(),
        "shell" => format!("!{}", data["command"].as_str()?.trim()),
        _ => return None,
    };
    (text != "!" && !text.is_empty()).then_some(text)
}

/// OpenCode 输入行里的 `!命令`（1.19.5 实测）：用户消息只有 OpenCode 补的段，紧接着的回复（`parentID` 指向它）只有一次
/// bash 调用。`parts` 为那条回复的各段；是这种情况时返回 `!命令`，显示成用户输入时的样子（同 Kimi、Grok）。
pub fn opencode_shell(parts: &[Value]) -> Option<String> {
    let mut work = parts.iter().filter(|p| p["type"] != "step-start" && p["type"] != "step-finish");
    let (only, rest) = (work.next()?, work.next());
    if rest.is_some() || only["type"] != "tool" || only["tool"] != "bash" {
        return None;
    }
    only["state"]["input"]["command"].as_str().map(|c| format!("!{}", c.trim()))
}

fn push(out: &mut Vec<Prompt>, text: String, at: Option<i64>) {
    let text = text.trim().to_owned();
    if text.is_empty() || out.last().is_some_and(|p| p.text == text) {
        return;
    }
    out.push(Prompt { text, at });
}

fn parts(content: &Value, kind: &str) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i["type"] == kind)
            .filter_map(|i| i["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude：`type=user` 且不是内部消息（`isMeta`）、子代理（`isSidechain`）或工具结果；斜杠命令显示为 `/命令 参数`，
/// 命令输出、系统提醒与「Caveat」说明不算提示词。记录带 `origin` 时只认 `kind=human`：后台任务结束时 Claude 投递的
/// `<task-notification>` 为 `kind=task-notification`（2026-11-13 长跑核对 2.1.322；更早的记录没有 `origin`，按开头滤掉）。
fn claude_prompt(v: &Value) -> Option<String> {
    if v["type"] != "user" || v["isMeta"] == true || v["isSidechain"] == true {
        return None;
    }
    if v["origin"]["kind"].as_str().is_some_and(|k| k != "human") {
        return None;
    }
    let content = &v["message"]["content"];
    if content.as_array().is_some_and(|a| a.iter().any(|i| i["type"] == "tool_result")) {
        return None;
    }
    let text = parts(content, "text");
    let t = text.trim_start();
    if let Some(name) = between(t, "<command-name>", "</command-name>") {
        let args = between(t, "<command-args>", "</command-args>").unwrap_or("").trim();
        return Some(if args.is_empty() { name.to_owned() } else { format!("{name} {args}") });
    }
    let internal =
        ["<local-command-", "<system-reminder>", "<command-message>", "Caveat: ", "[Request interrupted", "<task-notification>"];
    (!internal.iter().any(|p| t.starts_with(p))).then_some(text)
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let from = s.find(open)? + open.len();
    Some(&s[from..from + s[from..].find(close)?])
}

/// Codex `response_item` 的用户消息：0.163 起每段内容有种类（`content_item_kinds`），取 `user.text` 与
/// `/goal` 设定的目标（`user.goal`，显示为 `/goal 目标`；目标续跑注入的 `goal.internal_context` 不算）；
/// 更早的版本没有种类，按开头滤掉注入的 AGENTS.md 说明与环境信息。正文里的包装见 `codex_text`，
/// 粘贴的图片在正文后另起一行列出编号。
fn codex_item(p: &Value) -> Option<String> {
    let items = p["content"].as_array()?;
    let kinds = p["internal_chat_message_metadata_passthrough"]["content_item_kinds"].as_array();
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut add = |t: &str| match codex_text(t) {
        CodexText::Text(s) => texts.push(s),
        CodexText::Image(n) => images.push(n),
        CodexText::Skip => {}
    };
    match kinds {
        Some(k) => {
            for (i, kind) in items.iter().zip(k.iter()) {
                let Some(t) = i["text"].as_str() else { continue };
                if kind == "user.text" {
                    add(t);
                } else if kind == "user.goal" {
                    if let Some(goal) = codex_goal(t) {
                        add(&format!("/goal {goal}"));
                    }
                }
            }
        }
        None => items
            .iter()
            .filter(|i| i["type"] == "input_text")
            .filter_map(|i| i["text"].as_str())
            .filter(|t| {
                let t = t.trim_start();
                !["<environment_context>", "# AGENTS.md instructions", "<user_instructions>", "<permissions instructions>"]
                    .iter()
                    .any(|p| t.starts_with(p))
            })
            .for_each(&mut add),
    }
    if !images.is_empty() {
        texts.push(images.join(" "));
    }
    (!texts.is_empty()).then(|| texts.join("\n"))
}

enum CodexText {
    Text(String),
    Image(String),
    Skip,
}

/// Codex 用户正文里 CLI 加的包装（2026-10-25 按本机会话记录核对）：
/// - 粘贴图片：`<image name=[Image #1] path="…">` 与 `</image>` 夹着图片本身，只留 `[Image #1]`（与 Codex 输入框里的写法一致）；
/// - 引用文件或图片、选中回复批注：`# Files mentioned by the user:` / `# Response annotations:` 开头的说明，
///   真正的请求在最后一个 `## My request:` 之后；
/// - 回答 Codex 的提问：`<send_user_message_question_reply>[{question, answer, …}]</…>`，只留回答。
fn codex_text(t: &str) -> CodexText {
    let s = t.trim();
    if s.starts_with("<image") {
        return match between(s, "name=", "]") {
            Some(name) => CodexText::Image(format!("{name}]")),
            None => CodexText::Skip,
        };
    }
    if s == "</image>" {
        return CodexText::Skip;
    }
    if s.starts_with("# Files mentioned by the user") || s.starts_with("# Response annotations") {
        return match s.rfind("## My request:") {
            Some(i) => CodexText::Text(s[i + "## My request:".len()..].trim().to_owned()),
            None => CodexText::Skip,
        };
    }
    if let Some(json) = between(s, "<send_user_message_question_reply>", "</send_user_message_question_reply>") {
        let answers: Vec<String> = serde_json::from_str::<Value>(json)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|q| q["answer"].as_str().map(str::to_owned))
            .collect();
        return if answers.is_empty() { CodexText::Skip } else { CodexText::Text(answers.join("\n")) };
    }
    CodexText::Text(t.to_owned())
}

/// `/goal` 设定的目标：`<codex_internal_context source="user_goal">\nUser set the goal: "目标"\nUser set goal status: "active".\n…`。
fn codex_goal(t: &str) -> Option<String> {
    let open = "User set the goal: \"";
    let rest = &t[t.find(open)? + open.len()..];
    let end = rest.find("\"\nUser set goal status").or_else(|| rest.rfind('"'))?;
    let goal = rest[..end].trim();
    (!goal.is_empty()).then(|| goal.to_owned())
}

/// ISO 8601 时间（`2026-10-24T15:48:52.123Z` 或带 `±HH:MM`）或数字（秒或毫秒）→ 毫秒时间戳。
pub fn iso_ms(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(if n < 100_000_000_000 { n * 1000 } else { n });
    }
    let s = v.as_str()?.as_bytes();
    let num = |a: usize, b: usize| -> Option<i64> { std::str::from_utf8(s.get(a..b)?).ok()?.parse().ok() };
    if s.len() < 19 || s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut i = 19;
    let mut ms = 0;
    if s.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        let frac = std::str::from_utf8(&s[start..i.min(start + 3)]).ok()?;
        ms = format!("{frac:0<3}").parse().ok()?;
    }
    let offset = match s.get(i) {
        Some(b'Z') | None => 0,
        Some(b'+' | b'-') => {
            let sign = if s[i] == b'+' { 1 } else { -1 };
            sign * (num(i + 1, i + 3)? * 60 + num(i + 4, i + 6)?) * 60
        }
        _ => return None,
    };
    // 公历日期 → 自 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + sec - offset) * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Antigravity 1.4.12：只认用户自己输入的 USER_INPUT，去掉 `<USER_REQUEST>` 外的附注；系统消息不算。
    #[test]
    fn antigravity_prompts() {
        let text = [
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-01T06:55:52Z","content":"<USER_REQUEST>\n跑一下测试\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-01T14:55:52+08:00.\n</ADDITIONAL_METADATA>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-01T06:55:53Z","input_tokens":6942,"cache_read_tokens":0,"output_tokens":22,"tool_calls":[{"name":"run_command","args":{}}]}"#,
            r#"{"step_index":2,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-01T06:55:54Z","content":"background command finished"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-01T06:55:56Z","input_tokens":3263,"cache_read_tokens":4043,"output_tokens":13,"content":"测试都过了。"}"#,
            r#"{"step_index":4,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-01T06:58:55Z","content":"没有包装的提示词"}"#,
        ]
        .join("\n");
        let got = user_prompts(ReplySource::AntigravityTranscript, &text);
        assert_eq!(got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["跑一下测试", "没有包装的提示词"]);
        assert_eq!(got[0].at, Some(1_793_516_152_000));
        let sub = r#"{"step_index":0,"source":"SYSTEM","type":"USER_INPUT","content":"<USER_REQUEST>\nsubagent task\n</USER_REQUEST>"}"#;
        assert!(user_prompts(ReplySource::AntigravityTranscript, sub).is_empty(), "子代理收到的任务不算");
    }

    fn texts(v: &[Prompt]) -> Vec<&str> {
        v.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn claude_skips_internal_and_tool_results() {
        let t = r#"{"type":"user","timestamp":"2026-10-22T16:23:48.400Z","message":{"content":"修复登录"}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}
{"type":"user","isMeta":true,"message":{"content":"Caveat: The messages below were generated"}}
{"type":"user","message":{"content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args>opus</command-args>"}}
{"type":"user","message":{"content":"<local-command-stdout>Set model</local-command-stdout>"}}
{"type":"user","isSidechain":true,"message":{"content":"子代理的任务"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"好的"}]}}
{"type":"user","message":{"content":[{"type":"text","text":"再看看日志"},{"type":"image"}]}}
{"type":"user","message":{"content":"再看看日志"}}"#;
        let p = user_prompts(ReplySource::ClaudeTranscript, t);
        assert_eq!(texts(&p), ["修复登录", "/model opus", "再看看日志"]);
        assert_eq!(p[0].at, Some(1792686228400));
        // 回归（2026-11-13 长跑缺陷 B）：后台任务结束时 Claude 投递的通知（2.1.322 会话记录原样，正文截短）。
        let t = r#"{"type":"user","origin":{"kind":"human"},"message":{"content":"数一下目录"}}
{"type":"user","origin":{"kind":"task-notification","producer":"session-task"},"message":{"content":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}
{"type":"user","message":{"content":"<task-notification>\n<task-id>b2</task-id>\n</task-notification>"}}"#;
        assert_eq!(texts(&user_prompts(ReplySource::ClaudeTranscript, t)), ["数一下目录"]);
    }

    #[test]
    fn codex_takes_user_text_kind_only() {
        let t = r##"{"timestamp":"2026-10-24T14:48:49.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"},{"type":"input_text","text":"<environment_context>"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["agents_md.instructions","environments.environment_context"]}}}
{"timestamp":"2026-10-24T14:48:50.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"解释 Unix 域套接字"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.text"]}}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"答"}]}}"##;
        assert_eq!(texts(&user_prompts(ReplySource::CodexRollout, t)), ["解释 Unix 域套接字"]);
        let old = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"跑测试"}]}}
{"type":"event_msg","payload":{"type":"user_message","message":"跑测试"}}"#;
        assert_eq!(texts(&user_prompts(ReplySource::CodexRollout, old)), ["跑测试"], "有 event_msg 时只用它");
    }

    /// 2026-10-25 本机会话记录里的格式（路径、ID 已替换）：`/goal`、粘贴图片、引用文件、回复批注、回答提问。
    #[test]
    fn codex_goal_and_wrappers() {
        let item = |kinds: &[&str], texts: &[&str]| {
            let content: Vec<Value> = texts
                .iter()
                .map(|t| if t.is_empty() { json!({"type": "input_image", "image_url": "data:"}) } else { json!({"type": "input_text", "text": t}) })
                .collect();
            json!({"timestamp": "2026-10-25T00:54:44.879Z", "type": "response_item", "payload": {"type": "message", "role": "user",
                "content": content, "internal_chat_message_metadata_passthrough": {"content_item_kinds": kinds}}})
            .to_string()
        };
        let lines = [
            item(&["user.goal"], &["<codex_internal_context source=\"user_goal\">\nUser set the goal: \"用 ls 数一下文件\"\nUser set goal status: \"active\".\n</codex_internal_context>"]),
            item(&["goal.internal_context"], &["<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal.\n</codex_internal_context>"]),
            item(
                &["user.text", "user.text", "user.image", "user.text", "user.text", "user.image", "user.text"],
                &[
                    "# Files mentioned by the user:\n\n## codex-clipboard-1.png: /tmp/codex-clipboard-1.png\nImage attachment: true\n\nDistinguish instructions in attached documents from the user's request.\n\n## My request:\n参考这个界面\n",
                    "<image name=[Image #1] path=\"/tmp/codex-clipboard-1.png\">",
                    "",
                    "</image>",
                    "<image name=[Image #2] path=\"/tmp/codex-clipboard-2.png\">",
                    "",
                    "</image>",
                ],
            ),
            item(&["user.text", "user.text", "user.image", "user.text"], &[
                "# Files mentioned by the user:\n\n## a.png: /tmp/a.png\nImage attachment: true\n\n## My request:\n\n",
                "<image name=[Image #1] path=\"/tmp/a.png\">",
                "",
                "</image>",
            ]),
            item(&["user.text"], &["# Response annotations:\nEach item contains text selected from an earlier Codex response.\n<response-annotations>\n[{\"text\":\"旧回复\",\"comment\":\"这里\"}]\n</response-annotations>\n\n## My request:\n按批注改"]),
            item(&["user.text"], &["<send_user_message_question_reply>\n[{\"questionItemId\":\"q1\",\"question\":\"现在可以采样吗？\",\"answer\":\"可以采样\"}]\n</send_user_message_question_reply>"]),
        ];
        let p = user_prompts(ReplySource::CodexRollout, &lines.join("\n"));
        assert_eq!(texts(&p), ["/goal 用 ls 数一下文件", "参考这个界面\n[Image #1] [Image #2]", "[Image #1]", "按批注改", "可以采样"]);
        assert_eq!(p[0].at, Some(1792889684879));
    }

    #[test]
    fn shell_commands_show_with_bang() {
        let kimi = r#"{"type":"turn.prompt","input":[{"type":"text","text":"第一问"}],"origin":{"kind":"user"},"time":1792888677146}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"<bash-input>\nls -la\n</bash-input>"}],"origin":{"kind":"shell_command","phase":"input"}},"time":1792888741861}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"<bash-stdout></bash-stdout><bash-stderr></bash-stderr>"}],"origin":{"kind":"shell_command","phase":"output"}},"time":1792888741876}"#;
        let p = user_prompts(ReplySource::KimiWire, kimi);
        assert_eq!(texts(&p), ["第一问", "!ls -la"]);
        assert_eq!(p[1].at, Some(1792888741861));
        let grok = [
            json!({"timestamp": "2026-10-25T00:48:29Z", "params": {"update": {"sessionUpdate": "user_message_chunk",
                "_meta": {"promptIndex": 2}, "content": {"type": "text", "text": "插话"}}}}),
            json!({"timestamp": "2026-10-25T00:49:04Z", "params": {"update": {"sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "ls", "_meta": {"bash_command": "ls"}}}}}),
            json!({"params": {"update": {"sessionUpdate": "tool_call"}}}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        assert_eq!(texts(&user_prompts(ReplySource::GrokUpdates, &grok)), ["插话", "!ls"]);
    }

    #[test]
    fn kimi_uses_turn_prompt_once() {
        let t = r#"{"type":"turn.prompt","input":[{"type":"text","text":"用两句话解释"}],"origin":{"kind":"user"},"time":1792856847912}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"用两句话解释"}]}}
{"type":"agent.message.appended","message":{"message":{"role":"user","content":[{"type":"text","text":"用两句话解释"}]}}}
{"type":"turn.prompt","input":[{"type":"text","text":"系统续写"}],"origin":{"kind":"system"},"time":1}"#;
        let p = user_prompts(ReplySource::KimiWire, t);
        assert_eq!(texts(&p), ["用两句话解释"]);
        assert_eq!(p[0].at, Some(1792856847912));
    }

    #[test]
    fn copilot_takes_content_not_transformed() {
        let t = r#"{"type":"session.start","data":{},"timestamp":"2026-10-30T10:06:00.000Z"}
{"type":"user.message","data":{"content":"请只回复我OK","transformedContent":"<current_datetime>2026-10-30T18:06:23.387+08:00</current_datetime>\n\n请只回复我OK"},"timestamp":"2026-10-30T10:06:23.388Z"}
{"type":"assistant.message","data":{"content":"OK"},"timestamp":"2026-10-30T10:06:25.000Z"}
{"type":"user.message","data":{"content":"请只回复我no"},"timestamp":"2026-10-30T10:06:55.517Z"}"#;
        let p = user_prompts(ReplySource::CopilotEvents, t);
        assert_eq!(texts(&p), ["请只回复我OK", "请只回复我no"]);
        // 回归（2026-11-13 长跑缺陷 B）：主代理派给子代理的任务也是 user.message，带 `source`（1.0.105 会话记录原样，正文截短）；
        // 子代理结束的通知是 system.notification，不是 user.message。
        let sub = r#"{"type":"user.message","data":{"content":"Use a subagent to read the files","interactionId":"i1"},"timestamp":"2026-11-12T20:43:42.557Z"}
{"type":"user.message","data":{"content":"In the working directory /w, inspect only the immediate entries","source":"agent-5845f27b-42f1-4f01-aef3-ae7e9ad825cf"},"timestamp":"2026-11-12T20:43:49.275Z"}
{"type":"system.notification","data":{"content":"<system_notification>\nAgent \"largest-file\" (general-purpose) has finished processing and is now idle.\n</system_notification>","kind":{"type":"agent_idle"}},"timestamp":"2026-11-12T21:05:07.188Z"}"#;
        assert_eq!(texts(&user_prompts(ReplySource::CopilotEvents, sub)), ["Use a subagent to read the files"]);
        assert_eq!(p[0].at, Some(1793354783388));
        assert!(user_prompts(ReplySource::None, t).is_empty());
    }

    #[test]
    fn opencode_user_text_parts_only() {
        let user = json!({"role": "user", "time": {"created": 1793283466351_i64}});
        let parts = [json!({"type": "text", "text": "看一下 @a.rs"}),
                     json!({"type": "text", "text": "Called the Read tool with …", "synthetic": true}),
                     json!({"type": "file", "filename": "a.rs"}),
                     json!({"type": "text", "text": "被忽略", "ignored": true})];
        assert_eq!(opencode_prompt(&user, &parts).as_deref(), Some("看一下 @a.rs"));
        assert_eq!(opencode_prompt(&json!({"role": "assistant"}), &parts), None);
        assert_eq!(opencode_prompt(&user, &[json!({"type": "text", "text": "  "})]), None);
    }

    #[test]
    fn opencode_shell_command_from_reply() {
        let bash = json!({"type": "tool", "tool": "bash", "state": {"input": {"command": "echo hi"}}});
        let step = |t: &str| json!({"type": t});
        assert_eq!(opencode_shell(&[step("step-start"), bash.clone(), step("step-finish")]).as_deref(), Some("!echo hi"));
        assert_eq!(opencode_shell(&[bash.clone(), json!({"type": "text", "text": "好了"})]), None, "AI 自己调用后又回答，不是用户的命令");
        assert_eq!(opencode_shell(&[json!({"type": "tool", "tool": "read"})]), None);
        assert_eq!(opencode_shell(&[]), None);
    }

    #[test]
    fn grok_joins_chunks_by_prompt_index() {
        let chunk = |i: i64, text: &str| {
            json!({"timestamp": "2026-10-20T01:00:00Z", "params": {"update": {"sessionUpdate": "user_message_chunk",
                "_meta": {"promptIndex": i}, "content": {"type": "text", "text": text}}}})
            .to_string()
        };
        let reply = r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"答"}}}}"#;
        let t = [chunk(0, "第一"), chunk(0, "问"), reply.to_owned(), chunk(1, "第二问")].join("\n");
        assert_eq!(texts(&user_prompts(ReplySource::GrokUpdates, &t)), ["第一问", "第二问"]);
    }

    #[test]
    fn timestamps() {
        assert_eq!(iso_ms(&json!("1970-01-01T00:00:00Z")), Some(0));
        assert_eq!(iso_ms(&json!("2026-10-24T15:48:52.123Z")), Some(1792856932123));
        assert_eq!(iso_ms(&json!("2026-10-24T23:48:52.5+08:00")), Some(1792856932500));
        assert_eq!(iso_ms(&json!(1792856847)), Some(1792856847000));
        assert_eq!(iso_ms(&json!("昨天")), None);
    }

    /// OpenCode 2 的消息（2026-10-31 本机 2.2.4）：用户消息与输入行的 `!命令`，其余不算。
    #[test]
    fn opencode_v2_prompts() {
        use serde_json::json;
        assert_eq!(opencode_v2_prompt("user", &json!({"text": " 用 bash 执行 pwd ", "files": []})).as_deref(), Some("用 bash 执行 pwd"));
        assert_eq!(opencode_v2_prompt("shell", &json!({"command": "echo hi", "status": "exited"})).as_deref(), Some("!echo hi"));
        assert_eq!(opencode_v2_prompt("assistant", &json!({"content": []})), None);
        assert_eq!(opencode_v2_prompt("synthetic", &json!({"text": "补的"})), None);
        assert_eq!(opencode_v2_prompt("user", &json!({"text": "  "})), None);
    }
}
