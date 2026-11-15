//! 最后一条 AI 回复的解析（docs/design.md 第 5 节「实现（M1 第 6 块）」：「最近任务」的副标题）。只含纯逻辑；
//! 读文件在 scyllauid 的 `title.rs`（平台层）。各款字段按 CLI 自己的会话记录格式核对，
//! Grok 为 2026-10-14 本机 1.0.50 只读核对。格式都不是官方承诺，取不到时返回 None，由调用方保留旧值。

use serde_json::Value;

/// 会话记录的格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplySource {
    /// 不读。
    None,
    /// Claude Code transcript：`type=assistant` 记录 `message.content` 中 `type=text` 的片段。
    ClaudeTranscript,
    /// Codex rollout：`event_msg` 中每轮结束的 `task_complete` 的 `last_agent_message`（0.162 实测，2026-10-14），
    /// 或旧版的 `agent_message`（`payload.message`）；都没有时退回 `response_item` 中 `role=assistant` 的 `output_text`。
    CodexRollout,
    /// Kimi Code `agents/main/wire.jsonl`：`context.append_loop_event` 中 `event.type=content.part`、
    /// `event.part.type=text` 的 `text`。
    KimiWire,
    /// Grok `updates.jsonl`（ACP）：最后一个 `user_message_chunk` 之后的 `agent_message_chunk` 依次拼接。
    GrokUpdates,
    /// Copilot `~/.copilot/session-state/<ID>/events.jsonl`（适配器的 `session_record`）：提示词足迹为 `type=user.message`
    /// 的 `data.content`（2026-10-30 本机 1.0.105 只读核对）；回复为 `type=assistant.message` 的 `data.content`（2026-10-31 起读）。
    CopilotEvents,
    /// OpenCode 的 SQLite 库 `~/.local/share/opencode/opencode.db`（不是 JSON Lines，由 scyllauid 只读查询，表结构不是官方承诺）：
    /// 提示词足迹为 `message` 中 `role=user` 的消息与其 `part`（2026-10-30 本机 1.19.5 只读核对）；回复为最近一条有文字的
    /// `role=assistant` 消息的 `text` 段（2026-10-31 起读，`opencode_reply`）。
    OpenCodeDb,
    /// Antigravity `~/.gemini/antigravity-cli/brain/<ID>/.system_generated/logs/transcript_full.jsonl`（Hook 的
    /// `transcriptPath`，2026-11-01 本机 1.4.12 只读核对）：提示词足迹为 `type=USER_INPUT`、`source=USER_EXPLICIT` 的
    /// `content` 中 `<USER_REQUEST>` 里的部分；回复为 `type=PLANNER_RESPONSE` 的 `content`。
    AntigravityTranscript,
}

impl ReplySource {
    /// 能读出最后一条 AI 回复（「最近任务」副标题）。
    pub fn has_reply(self) -> bool {
        self != ReplySource::None
    }

    /// 能读出用户提示词（提示词足迹）。
    pub fn has_prompts(self) -> bool {
        self != ReplySource::None
    }
}

fn text_parts(content: &Value, kind: &str) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i["type"] == kind)
            .filter_map(|i| i["text"].as_str())
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 会话记录（JSON Lines，可以只是文件末尾的一段完整行）中最后一条非空的 AI 回复。
pub fn last_reply(source: ReplySource, text: &str) -> Option<String> {
    let mut last: Option<String> = None;
    // Codex 的 response_item 只作回退（可能是结构化内容），event_msg 优先。
    let mut fallback: Option<String> = None;
    // Grok：一条回复分多片，遇到新的用户消息重新开始。
    let mut grok = String::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let reply = match source {
            ReplySource::ClaudeTranscript if v["type"] == "assistant" => {
                Some(text_parts(&v["message"]["content"], "text"))
            }
            ReplySource::CodexRollout => {
                let p = &v["payload"];
                if v["type"] == "event_msg" && p["type"] == "task_complete" {
                    p["last_agent_message"].as_str().map(str::to_owned)
                } else if v["type"] == "event_msg" && p["type"] == "agent_message" {
                    p["message"].as_str().map(str::to_owned)
                } else if v["type"] == "response_item" && p["type"] == "message" && p["role"] == "assistant" {
                    let t = text_parts(&p["content"], "output_text");
                    if !t.trim().is_empty() {
                        fallback = Some(t);
                    }
                    None
                } else {
                    None
                }
            }
            ReplySource::CopilotEvents if v["type"] == "assistant.message" => v["data"]["content"].as_str().map(str::to_owned),
            ReplySource::AntigravityTranscript if v["type"] == "PLANNER_RESPONSE" => v["content"].as_str().map(str::to_owned),
            ReplySource::KimiWire if v["type"] == "context.append_loop_event" => {
                let e = &v["event"];
                (e["type"] == "content.part" && e["part"]["type"] == "text")
                    .then(|| e["part"]["text"].as_str().map(str::to_owned))
                    .flatten()
            }
            ReplySource::GrokUpdates => {
                let u = &v["params"]["update"];
                match u["sessionUpdate"].as_str() {
                    Some("user_message_chunk") => grok.clear(),
                    Some("agent_message_chunk") => {
                        if let Some(t) = u["content"]["text"].as_str() {
                            grok.push_str(t);
                        }
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        };
        if let Some(r) = reply.filter(|r| !r.trim().is_empty()) {
            last = Some(r);
        }
    }
    if source == ReplySource::GrokUpdates && !grok.trim().is_empty() {
        last = Some(grok);
    }
    last.or(fallback).map(|r| r.trim().to_owned())
}

/// OpenCode 2 一条助手消息（`session_message` 中 `type=assistant` 的 `data`）的回复文字：`content` 里 `type=text` 的段依次
/// 拼接（2026-10-31 本机 2.2.4 核对）；只调了工具为 None。
pub fn opencode_v2_reply(data: &Value) -> Option<String> {
    let text = data["content"]
        .as_array()?
        .iter()
        .filter(|c| c["type"] == "text")
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// OpenCode 1 一条助手消息的回复文字：`text` 段依次拼接，OpenCode 补的段（`synthetic`）与被忽略的段不算（2026-10-31，同
/// `prompts::opencode_prompt`）。`parts` 为这条消息的各段；没有文字（只调了工具）为 None，由调用方往前找。
pub fn opencode_reply(message: &Value, parts: &[Value]) -> Option<String> {
    if message["role"] != "assistant" {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Antigravity 1.4.12 会话记录（结构取自本机记录，正文已改）：回复为最后一条有 `content` 的 PLANNER_RESPONSE。
    #[test]
    fn antigravity_last_reply() {
        let text = [
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-01T06:55:52Z","content":"<USER_REQUEST>\n跑一下测试\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-01T14:55:52+08:00.\n</ADDITIONAL_METADATA>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-01T06:55:53Z","input_tokens":6942,"cache_read_tokens":0,"output_tokens":22,"tool_calls":[{"name":"run_command","args":{}}]}"#,
            r#"{"step_index":2,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-01T06:55:54Z","content":"background command finished"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-01T06:55:56Z","input_tokens":3263,"cache_read_tokens":4043,"output_tokens":13,"content":"测试都过了。"}"#,
            r#"{"step_index":4,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-01T06:58:55Z","content":"没有包装的提示词"}"#,
        ]
        .join("\n");
        assert_eq!(last_reply(ReplySource::AntigravityTranscript, &text).as_deref(), Some("测试都过了。"));
        assert!(ReplySource::AntigravityTranscript.has_prompts());
    }

    #[test]
    fn claude_takes_the_last_text_reply() {
        let t = r#"{"type":"user","message":{"content":"今天星期几"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"阿远，今天星期一。"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"主人，**已完成**。"}]}}
{"type":"ai-title","aiTitle":"x"}"#;
        assert_eq!(last_reply(ReplySource::ClaudeTranscript, t).as_deref(), Some("主人，**已完成**。"));
        assert_eq!(last_reply(ReplySource::ClaudeTranscript, "{\"type\":\"user\"}\n半行{"), None);
    }

    #[test]
    fn codex_prefers_agent_message() {
        let t = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"回退"}]}}
{"type":"event_msg","payload":{"type":"agent_message","message":"阿远，今天是星期一。"}}
{"type":"event_msg","payload":{"type":"task_complete"}}"#;
        assert_eq!(last_reply(ReplySource::CodexRollout, t).as_deref(), Some("阿远，今天是星期一。"));
        let v162 = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"阿远，今天周一。"}]}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t","last_agent_message":"阿远，今天周一。"}}"#;
        assert_eq!(last_reply(ReplySource::CodexRollout, v162).as_deref(), Some("阿远，今天周一。"));
        let only = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"只有这条"}]}}"#;
        assert_eq!(last_reply(ReplySource::CodexRollout, only).as_deref(), Some("只有这条"));
    }

    /// 结构取自 2026-10-30 本机 Copilot 1.0.105 的 `events.jsonl`（正文已删）：只调工具的那条 `content` 为空，跳过。
    #[test]
    fn copilot_takes_the_last_assistant_message() {
        let t = r#"{"type":"user.message","data":{"content":"只回复 en"}}
{"type":"assistant.message","data":{"model":"gpt-6-luna","content":"en","toolRequests":[]}}
{"type":"user.message","data":{"content":"看看目录"}}
{"type":"assistant.message","data":{"model":"gpt-6-luna","content":"阿远，目录里有 3 个文件。","toolRequests":[]}}
{"type":"assistant.message","data":{"model":"gpt-6-luna","content":"","toolRequests":[{"name":"bash"}]}}
{"type":"assistant.turn_end","data":{"turnId":"1"}}"#;
        assert_eq!(last_reply(ReplySource::CopilotEvents, t).as_deref(), Some("阿远，目录里有 3 个文件。"));
        assert!(ReplySource::CopilotEvents.has_reply() && ReplySource::OpenCodeDb.has_reply() && !ReplySource::None.has_reply());
    }

    /// 结构取自 2026-10-31 本机 OpenCode 1.19.5 的库（正文已删）。
    #[test]
    fn opencode_reply_joins_text_parts() {
        use serde_json::json;
        let assistant = json!({"role": "assistant", "modelID": "deepseek-v4.1-flash-free"});
        let parts = [
            json!({"type": "step-start"}),
            json!({"type": "reasoning", "text": "想一想"}),
            json!({"type": "text", "text": "OK"}),
            json!({"type": "text", "text": "补的", "synthetic": true}),
            json!({"type": "step-finish", "tokens": {"total": 3}}),
        ];
        assert_eq!(opencode_reply(&assistant, &parts).as_deref(), Some("OK"));
        assert_eq!(opencode_reply(&assistant, &[json!({"type": "tool", "tool": "bash"})]), None, "只调了工具");
        assert_eq!(opencode_reply(&json!({"role": "user"}), &parts), None);
        // OpenCode 2：助手消息的 `content`。
        let v2 = json!({"model": {"id": "deepseek-v4.1-flash-free", "providerID": "opencode"},
                        "content": [{"type": "reasoning", "text": "想一想"}, {"type": "text", "text": "当前目录是 /w"}]});
        assert_eq!(opencode_v2_reply(&v2).as_deref(), Some("当前目录是 /w"));
        assert_eq!(opencode_v2_reply(&json!({"content": [{"type": "tool", "name": "shell"}]})), None, "只调了工具");
    }

    #[test]
    fn kimi_takes_text_parts() {
        let t = r#"{"type":"turn.prompt","origin":{"kind":"user"}}
{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"主人，**Argon** 的中文意思是「**氩**」"}}}
{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"1"}}"#;
        assert_eq!(last_reply(ReplySource::KimiWire, t).as_deref(), Some("主人，**Argon** 的中文意思是「**氩**」"));
    }

    #[test]
    fn grok_joins_chunks_after_the_last_prompt() {
        let chunk = |kind: &str, text: &str| {
            format!(r#"{{"method":"session/update","params":{{"update":{{"sessionUpdate":"{kind}","content":{{"type":"text","text":"{text}"}}}}}}}}"#)
        };
        let t = [
            chunk("user_message_chunk", "第一问"),
            chunk("agent_message_chunk", "旧回复"),
            chunk("user_message_chunk", "第二问"),
            chunk("agent_thought_chunk", "想一想"),
            chunk("agent_message_chunk", "今天"),
            chunk("agent_message_chunk", "是周一。"),
            r#"{"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed"}}}"#.to_owned(),
        ]
        .join("\n");
        assert_eq!(last_reply(ReplySource::GrokUpdates, &t).as_deref(), Some("今天是周一。"));
        assert_eq!(last_reply(ReplySource::GrokUpdates, &chunk("user_message_chunk", "刚问")), None);
    }
}
