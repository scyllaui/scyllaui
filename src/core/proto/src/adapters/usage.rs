//! 标题栏状态行的用量信息（M2 第 2 块起，docs/design.md 第 5 节「实现（M2 第 2 块）」）：从会话记录里读模型。
//! 只含纯逻辑；读文件在 scyllauid 的 `title.rs`。格式都是本机实测（2026-10-16 核对），不是官方承诺，取不到时返回 None。

use serde_json::Value;

/// 会话记录里模型的来源；Hook 载荷里的模型字段见适配器的 `Fields::model`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// 不读会话记录（Codex：每个 Hook 都带 `model`）。
    None,
    /// Claude Code transcript：最后一条 `type=assistant` 记录的 `message.model`；`<synthetic>` 一类尖括号值不是真实模型。
    ClaudeTranscript,
    /// Kimi Code `agents/main/wire.jsonl`：最后一条 `type=usage.record` 的 `model`（如 `kimi-code/kimi-for-coding`）。
    KimiWire,
    /// Grok 会话目录的 `summary.json`：`current_model_id`（如 `grok-4.7`）。
    GrokSummary,
    /// Copilot `events.jsonl`：最后一条 `type=assistant.message` 的 `data.model`（如 `gpt-6-luna`；2026-10-31 起读）。
    CopilotEvents,
    /// OpenCode 的库（由 scyllauid 只读查询，2026-10-31 起读）：OpenCode 2 为 `session_v2.model` 的 `id`，OpenCode 1 为
    /// 最后一条助手消息的 `modelID`。OpenCode 1 的提示词事件也带模型（`Fields::model`）。
    OpenCodeDb,
}

/// 会话记录（JSON Lines，可以只是末尾一段完整行）中最后出现的模型。`GrokSummary` 与 `None` 不按行读，返回 None。
pub fn last_model(source: ModelSource, text: &str) -> Option<String> {
    let mut last = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let model = match source {
            ModelSource::ClaudeTranscript if v["type"] == "assistant" => v["message"]["model"].as_str(),
            ModelSource::KimiWire if v["type"] == "usage.record" => v["model"].as_str(),
            ModelSource::CopilotEvents if v["type"] == "assistant.message" => v["data"]["model"].as_str(),
            _ => None,
        };
        if let Some(m) = model.filter(|m| !m.is_empty() && !m.starts_with('<')) {
            last = Some(m.to_owned());
        }
    }
    last
}

/// 会话记录里推理强度的来源（维护期 0.3.5，docs/design.md 第 5 节「实现（维护期 0.3.5）」）。都是 CLI 记录的原文
/// （如 `high`、`xhigh`、`max`），2026-10-23 本机核对，格式非官方承诺。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortSource {
    None,
    /// Claude Code transcript：`type=assistant` 记录顶层的 `effort`（2.1.219 起有）。
    ClaudeTranscript,
    /// Codex rollout：每轮开头 `type=turn_context` 的 `payload.effort`。
    CodexRollout,
    /// Kimi Code `agents/main/wire.jsonl`：带 `thinkingEffort` 的记录（每次请求的 `llm.request`、会话开头的 `profile.bind`）。
    KimiWire,
    /// Grok 会话目录的 `summary.json`：`reasoning_effort`。
    GrokSummary,
}

/// 会话记录（JSON Lines，可以只是末尾一段完整行）中最后出现的推理强度。`GrokSummary` 与 `None` 不按行读，返回 None。
pub fn last_effort(source: EffortSource, text: &str) -> Option<String> {
    let mut last = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let effort = match source {
            EffortSource::ClaudeTranscript if v["type"] == "assistant" => v["effort"].as_str(),
            EffortSource::CodexRollout if v["type"] == "turn_context" => v["payload"]["effort"].as_str(),
            EffortSource::KimiWire => v["thinkingEffort"].as_str(),
            _ => None,
        };
        if let Some(e) = effort.filter(|e| !e.is_empty()) {
            last = Some(e.to_owned());
        }
    }
    last
}

/// Grok `summary.json` 的推理强度。
pub fn grok_summary_effort(summary: &Value) -> Option<String> {
    summary["reasoning_effort"].as_str().filter(|e| !e.is_empty()).map(str::to_owned)
}

/// 会话累计 token（M2 第 3 块）：四项口径统一为「输入（不含缓存）、缓存读、缓存写、输出」；`total` 为 CLI 自己报告的合计，
/// 没有时为四项之和（docs/design.md 第 5 节「实现（M2 第 3 块）」）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub total: u64,
}

impl Tokens {
    fn sum(input: u64, cache_read: u64, cache_write: u64, output: u64) -> Tokens {
        Tokens { input, cache_read, cache_write, output, total: input + cache_read + cache_write + output }
    }

    pub fn add(&mut self, o: &Tokens) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.total += o.total;
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({"input": self.input, "cache_read": self.cache_read, "cache_write": self.cache_write,
                           "output": self.output})
    }
}

/// 额度的时间窗口（M2 第 3 块为 7 天，2026-10-31 用户要求加 5 小时，2026-11-01 加按月计的 Copilot）；协议里的 `window`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Window {
    FiveHour,
    SevenDay,
    Month,
}

impl Window {
    pub fn as_str(self) -> &'static str {
        match self {
            Window::FiveHour => "5h",
            Window::SevenDay => "7d",
            Window::Month => "month",
        }
    }

    /// 窗口时长（分钟）归类（2026-11-01 标题栏额度统一规则）：300 为 5 小时、10080 为 7 天、
    /// 28—31 天为月；别的认不出，不收。Codex `rate_limits` 的 `window_minutes` 用它，以后各家改了窗口也按时长归类。
    pub fn from_minutes(m: i64) -> Option<Window> {
        match m {
            300 => Some(Window::FiveHour),
            10080 => Some(Window::SevenDay),
            40320..=44640 => Some(Window::Month),
            _ => None,
        }
    }
}

/// 一个窗口的额度（M2 第 3 块）：已用百分比（0—100）、重置时间与数据所对应的时间（Unix 毫秒）。`unlimited` 为来源明确写了
/// 这个窗口不限制（目前只有 Copilot 的 `isUnlimitedEntitlement`），此时百分比与重置时间无意义、界面不显示。账号没有的窗口
/// 不再补一项（2026-11-01 统一规则：数据里有的窗口就显示，没有的不显示）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quota {
    pub window: Window,
    pub used_percent: f64,
    pub resets_at: i64,
    pub as_of: i64,
    pub unlimited: bool,
}

/// 会话记录里 token 与周额度的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageSource {
    /// 不读。
    None,
    /// Claude transcript：`type=assistant` 记录的 `message.usage`，同一 `message.id` 只算一次（逐行增量）。
    ClaudeTranscript,
    /// Codex rollout：最后一条 `event_msg` / `token_count`（`info.total_token_usage` 与 `rate_limits`）。
    CodexRollout,
    /// Kimi `wire.jsonl`：`type=usage.record` 的每轮增量（逐行增量）。
    KimiWire,
    /// Grok 会话目录的 `usage.json`：`session` 汇总。
    GrokUsageJson,
    /// Copilot `events.jsonl`：`type=session.usage_record` 的每次调用（逐行增量，2026-10-31 起读）。
    CopilotEvents,
    /// OpenCode 的库：这个会话每一步结束（`type=step-finish` 的段）的 `tokens` 相加（2026-10-31 起读，由 scyllauid 只读查询）。
    OpenCodeDb,
    /// Antigravity 的会话记录（同 `ReplySource::AntigravityTranscript`）：每次调用模型（`type=PLANNER_RESPONSE`）的 token
    /// 相加（逐行增量）。
    AntigravityTranscript,
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// Claude 的一行：`(message.id, 这条消息的 token)`；不是带 usage 的 assistant 记录为 None。
pub fn claude_usage_line(line: &Value) -> Option<(String, Tokens)> {
    if line["type"] != "assistant" {
        return None;
    }
    let m = &line["message"];
    let (id, u) = (m["id"].as_str()?, m.get("usage")?);
    Some((id.to_owned(), Tokens::sum(n(&u["input_tokens"]), n(&u["cache_read_input_tokens"]),
                                      n(&u["cache_creation_input_tokens"]), n(&u["output_tokens"]))))
}

/// Kimi 的一行：一轮的 token；不是 `usage.record` 为 None。
pub fn kimi_usage_line(line: &Value) -> Option<Tokens> {
    if line["type"] != "usage.record" {
        return None;
    }
    let u = line.get("usage")?;
    Some(Tokens::sum(n(&u["inputOther"]), n(&u["inputCacheRead"]), n(&u["inputCacheCreation"]), n(&u["output"])))
}

/// Copilot 的一行：一次调用的 token；不是 `session.usage_record` 为 None。`inputTokens` 已含缓存读写：本机 1.0.105 的
/// `session.shutdown` 汇总里「输入」恰为 `inputTokens - cacheReadTokens - cacheWriteTokens`（2026-10-31 核对 7 个会话），
/// `outputTokens` 已含推理。
pub fn copilot_usage_line(line: &Value) -> Option<Tokens> {
    if line["type"] != "session.usage_record" {
        return None;
    }
    let u = line["data"].get("usage")?;
    let (input, read, write, output) = (n(&u["inputTokens"]), n(&u["cacheReadTokens"]), n(&u["cacheWriteTokens"]), n(&u["outputTokens"]));
    Some(Tokens { input: input.saturating_sub(read + write), cache_read: read, cache_write: write, output, total: input + output })
}

/// Antigravity 的一行：一次调用模型的 token；不是带 `input_tokens` 的 `PLANNER_RESPONSE` 为 None。`input_tokens` 不含缓存
/// （2026-11-01 本机 1.4.12：有一次 `input_tokens` 3263、`cache_read_tokens` 4043，缓存比输入还多），没有缓存写入。
pub fn antigravity_usage_line(line: &Value) -> Option<Tokens> {
    if line["type"] != "PLANNER_RESPONSE" || line.get("input_tokens").is_none() {
        return None;
    }
    Some(Tokens::sum(n(&line["input_tokens"]), n(&line["cache_read_tokens"]), 0, n(&line["output_tokens"])))
}

/// OpenCode 一步的 token（`type=step-finish` 的段）：`input` 不含缓存，推理单列，`total` 为五项之和（2026-10-31 本机 1.19.5
/// 核对）。推理算进输出，与其余几款一致。
pub fn opencode_step_tokens(part: &Value) -> Option<Tokens> {
    if part["type"] != "step-finish" {
        return None;
    }
    let t = part.get("tokens")?;
    let (input, read, write) = (n(&t["input"]), n(&t["cache"]["read"]), n(&t["cache"]["write"]));
    let output = n(&t["output"]) + n(&t["reasoning"]);
    Some(Tokens { input, cache_read: read, cache_write: write, output, total: t["total"].as_u64().unwrap_or(input + read + write + output) })
}

/// ISO 8601（`2026-10-15T13:31:32.123Z`）转 Unix 毫秒；只认 UTC 的 `Z` 结尾，其他格式为 None。
fn iso_ms(s: &str) -> Option<i64> {
    // 结尾为 `Z`，或时差 `+08:00` / `-05:00`（Grok 的计费周期写 `+00:00`）。
    let (s, offset_min) = match s.strip_suffix('Z') {
        Some(s) => (s, 0),
        None => {
            let cut = s.len().checked_sub(6)?;
            let (body, off) = (s.get(..cut)?, s.get(cut..)?);
            let sign = match off.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h, m) = off[1..].split_once(':')?;
            (body, sign * (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?))
        }
    };
    let (date, time) = s.split_once('T')?;
    let d: Vec<i64> = date.split('-').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let t: Vec<i64> = hms.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if d.len() != 3 || t.len() != 3 {
        return None;
    }
    // 公历日期到日数（Howard Hinnant 的 days_from_civil）。
    let (y, m) = if d[1] <= 2 { (d[0] - 1, d[1] + 9) } else { (d[0], d[1] - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d[2] - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    Some(((days * 24 + t[0]) * 60 + t[1] - offset_min) * 60_000 + t[2] * 1000 + ms)
}

/// Codex rollout（可以只是末尾一段完整行）中最后一条 `token_count`：累计 token，以及 `rate_limits` 里 primary、secondary
/// 两个窗口的额度，按 `window_minutes` 归类（`Window::from_minutes`）。记录里没有的窗口不补；认不出的时长放在第三项，
/// 由调用方记日志。
pub fn codex_last_usage(text: &str) -> Option<(Tokens, Vec<Quota>, Vec<i64>)> {
    let mut last = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] == "event_msg" && v["payload"]["type"] == "token_count" && v["payload"]["info"].is_object() {
            last = Some(v);
        }
    }
    let v = last?;
    let t = &v["payload"]["info"]["total_token_usage"];
    let (input, cached) = (n(&t["input_tokens"]), n(&t["cached_input_tokens"]));
    let tokens = Tokens {
        input: input.saturating_sub(cached),
        cache_read: cached,
        cache_write: n(&t["cache_write_input_tokens"]),
        output: n(&t["output_tokens"]),
        total: n(&t["total_tokens"]),
    };
    let rl = &v["payload"]["rate_limits"];
    let (mut quotas, mut unknown) = (Vec::new(), Vec::new());
    if let Some(as_of) = v["timestamp"].as_str().and_then(iso_ms) {
        for w in ["primary", "secondary"].iter().map(|k| &rl[*k]) {
            let Some(minutes) = w["window_minutes"].as_i64() else { continue };
            let Some(window) = Window::from_minutes(minutes) else {
                unknown.push(minutes);
                continue;
            };
            if let (Some(used), Some(resets)) = (w["used_percent"].as_f64(), w["resets_at"].as_i64()) {
                quotas.push(Quota { window, used_percent: used, resets_at: resets * 1000, as_of, unlimited: false });
            }
        }
    }
    Some((tokens, quotas, unknown))
}

/// 按量计费的依据（2026-11-01 标题栏额度统一规则：数据表明是按量计费才显示 `API`，没有依据不推断）。Codex：rollout 第一行
/// `session_meta` 的 `model_provider` 不是 `openai`（接了别家服务商，2026-11-01 本机 0.166.0 为 `openai`）。用 OpenAI API key
/// 登录时记录里看不出，不标。
pub fn codex_api(first_line: &Value) -> bool {
    first_line["type"] == "session_meta" && first_line["payload"]["model_provider"].as_str().is_some_and(|p| !p.is_empty() && p != "openai")
}

/// 同上，Kimi：会话的模型别名（wire 的 `modelAlias`，如 `kimi-code/kimi-for-coding`）是「服务商/模型」，服务商不是 Kimi 会员
/// 自带的 `kimi-code`（即用 `kimi provider add` 接的第三方）。没有服务商段的别名看不出，不标。
pub fn kimi_api(model_alias: &str) -> bool {
    model_alias.split_once('/').is_some_and(|(provider, _)| !provider.is_empty() && provider != "kimi-code")
}

/// 同上，Copilot：`session.usage_record` 的 `data.usage.isByok`（自带 key 的模型，2026-11-01 本机 1.0.105 为 `false`）。
/// 不是这类行为 None。
pub fn copilot_byok(line: &Value) -> Option<bool> {
    (line["type"] == "session.usage_record").then(|| line["data"]["usage"]["isByok"] == true)
}

/// Antigravity 查额度的命令参数（`~/.local/bin/agy` 后接）：官方的 `/usage` 斜杠命令以打印模式运行、输出 JSON，不调模型、
/// 不耗 token，由 agy 自己联网查询（2026-11-01 本机 1.4.12 实测约 4 秒；changelog 写明 `--json` 输出供脚本用，时间为 UTC）。
pub const ANTIGRAVITY_USAGE_ARGS: [&str; 6] = ["-p", "/usage", "--output-format", "json", "--print-timeout", "20s"];

/// `agy -p /usage --output-format json` 的输出 → 额度。额度按模型组分（本机：「Gemini Models」与「Claude and GPT
/// models」），组内模型共用；取当前模型所在的组（模型名第一段，如 `gemini-3.8-flash-low` 的 `gemini`，出现在组名或说明里），
/// 对不上时取剩余最少的组。组里 `window` 为 `weekly` 的一项即 7 天额度（已用 = 1 − `remaining_fraction`）；没有的窗口不补，
/// 认不出的窗口不用；什么也没认出为空。
pub fn antigravity_quotas(out: &Value, model: Option<&str>, now: i64) -> Vec<Quota> {
    let groups = out["command"]["data"]["groups"].as_array().cloned().unwrap_or_default();
    let remaining = |g: &Value| {
        g["buckets"].as_array().into_iter().flatten().filter_map(|b| b["remaining_fraction"].as_f64()).fold(f64::INFINITY, f64::min)
    };
    let family = model.and_then(|m| m.split(['-', ' ', '/']).next()).map(str::to_lowercase).filter(|f| !f.is_empty());
    let mentions = |g: &Value, f: &str| {
        format!("{} {}", g["name"].as_str().unwrap_or(""), g["description"].as_str().unwrap_or("")).to_lowercase().contains(f)
    };
    let group = family
        .and_then(|f| groups.iter().find(|g| mentions(g, &f)))
        .or_else(|| groups.iter().min_by(|a, b| remaining(a).total_cmp(&remaining(b))));
    let Some(group) = group else { return Vec::new() };
    let weekly = group["buckets"].as_array().into_iter().flatten().find_map(|b| {
        let (window, left) = (b["window"].as_str()?, b["remaining_fraction"].as_f64()?);
        (window == "weekly").then(|| Quota {
            window: Window::SevenDay,
            used_percent: ((1.0 - left) * 100.0).clamp(0.0, 100.0),
            resets_at: b["reset_time"].as_str().and_then(iso_ms).unwrap_or(0),
            as_of: now,
            unlimited: false,
        })
    });
    weekly.into_iter().collect()
}

/// Copilot `events.jsonl` 里每次调用模型（`type=model.model_call_success`）带的 `data.quotaSnapshots` → 月额度
/// （2026-11-01 本机 1.0.105 核对，`resetDate` 为下月 1 日）。快照分 `chat`、`completions`、`premium_interactions`，
/// CLI 用的是对话：免费账号计 `chat`（每月 200 次），付费账号计 `premium_interactions`（`chat` 不限）；`completions` 是
/// 编辑器补全，不看。取有限额（不是不限、额度大于 0）的一项，两项都有限额取剩余少的；都不限时记为没有限制。已用 = 100 −
/// `remainingPercentage`。不是这类行为 None。
pub fn copilot_quota_line(line: &Value) -> Option<Quota> {
    if line["type"] != "model.model_call_success" {
        return None;
    }
    let snaps = &line["data"]["quotaSnapshots"];
    let as_of = line["timestamp"].as_str().and_then(iso_ms)?;
    let kinds: Vec<&Value> = ["premium_interactions", "chat"].iter().map(|k| &snaps[*k]).filter(|s| s.is_object()).collect();
    let limited = kinds.iter().filter(|s| s["isUnlimitedEntitlement"] != true && s["entitlementRequests"].as_f64().unwrap_or(0.0) > 0.0);
    match limited.filter_map(|s| Some((s["remainingPercentage"].as_f64()?, s["resetDate"].as_str().and_then(iso_ms)?)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
    {
        Some((left, resets_at)) => Some(Quota { window: Window::Month, used_percent: (100.0 - left).clamp(0.0, 100.0), resets_at, as_of, unlimited: false }),
        None if kinds.iter().any(|s| s["isUnlimitedEntitlement"] == true) => {
            Some(Quota { window: Window::Month, used_percent: 0.0, resets_at: 0, as_of, unlimited: true })
        }
        None => None,
    }
}

/// `kimi web` 启动时打印的本机地址（`Local:    http://127.0.0.1:51204/#token=…`）→ 端口与这个本机服务的令牌。
pub fn kimi_web_address(line: &str) -> Option<(u16, String)> {
    let rest = &line[line.find("http://127.0.0.1:")? + "http://127.0.0.1:".len()..];
    let (port, rest) = rest.split_once('/')?;
    let token: String = rest.strip_prefix("#token=")?.chars().take_while(|c| !c.is_whitespace()).collect();
    (!token.is_empty()).then_some((port.parse().ok()?, token))
}

/// `kimi web` 的 `GET /api/v1/oauth/usage` → 额度（2026-11-01 本机 2.4.1 核对：`data.quota.usages` 里 `limit5h`、`limit7d`、
/// 按月计的账号还有 `monthTotal`，各为 `usedRatio`（0—1）与 `resetAt`，与 Kimi `/usage` 斜杠命令同源）。没有的窗口跳过、
/// 不补。`data.kind` 不是 `ok`（未登录、用的不是 Kimi 会员）为空。
pub fn kimi_quotas(body: &Value, now: i64) -> Vec<Quota> {
    let data = &body["data"];
    if data["kind"] != "ok" {
        return Vec::new();
    }
    let usages = &data["quota"]["usages"];
    [(Window::FiveHour, "limit5h"), (Window::SevenDay, "limit7d"), (Window::Month, "monthTotal")]
        .into_iter()
        .filter_map(|(window, key)| {
            let u = &usages[key];
            Some(Quota {
                window,
                used_percent: (u["usedRatio"].as_f64()? * 100.0).clamp(0.0, 100.0),
                resets_at: u["resetAt"].as_str().and_then(iso_ms)?,
                as_of: now,
                unlimited: false,
            })
        })
        .collect()
}

/// Grok 的 ACP 扩展方法 `_x.ai/billing`（`grok agent stdio`，Grok 用自己的登录查询）的 `result` → 额度（2026-11-01 本机
/// 1.0.70 核对：`config.creditUsagePercent` 为本期已用百分比，`currentPeriod.type` 为 `USAGE_PERIOD_TYPE_WEEKLY` 时是
/// 7 天额度、`end` 为重置时间）。认不出的周期不用，没有的窗口不补。
pub fn grok_quotas(result: &Value, now: i64) -> Vec<Quota> {
    let config = &result["config"];
    let period = &config["currentPeriod"];
    let window = match period["type"].as_str() {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => Window::SevenDay,
        Some("USAGE_PERIOD_TYPE_MONTHLY") => Window::Month,
        _ => return Vec::new(),
    };
    let (Some(used), Some(resets_at)) = (config["creditUsagePercent"].as_f64(), period["end"].as_str().and_then(iso_ms)) else {
        return Vec::new();
    };
    vec![Quota { window, used_percent: used.clamp(0.0, 100.0), resets_at, as_of: now, unlimited: false }]
}

/// Grok `usage.json` 的 `session` 汇总。
pub fn grok_usage(usage: &Value) -> Option<Tokens> {
    let s = usage.get("session")?;
    let (input, cached) = (n(&s["inputTokens"]), n(&s["cachedReadTokens"]));
    Some(Tokens {
        input: input.saturating_sub(cached),
        cache_read: cached,
        cache_write: n(&s["cacheCreationTokens"]),
        output: n(&s["outputTokens"]),
        total: s["totalTokens"].as_u64()?,
    })
}

/// Claude 官方 statusline stdin 的 `rate_limits.five_hour` 与 `seven_day`（`used_percentage` 0—100、`resets_at` Unix 秒；
/// 2026-10-31 起读 5 小时）；`now` 为收到的时间。没有的窗口不返回（非 Pro / Max 或首次响应前）。
pub fn claude_statusline_quotas(payload: &Value, now: i64) -> Vec<Quota> {
    [(Window::FiveHour, "five_hour"), (Window::SevenDay, "seven_day")]
        .into_iter()
        .filter_map(|(window, key)| {
            let w = &payload["rate_limits"][key];
            Some(Quota { window, used_percent: w["used_percentage"].as_f64()?, resets_at: w["resets_at"].as_i64()? * 1000, as_of: now, unlimited: false })
        })
        .collect()
}

/// Grok `summary.json` 的当前模型。
pub fn grok_summary_model(summary: &Value) -> Option<String> {
    summary["current_model_id"].as_str().filter(|m| !m.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Antigravity `/usage` 的输出（结构取自 2026-11-01 本机 1.4.12）：按当前模型取组，周额度已用 = 1 − 剩余比例，没有的
    /// 5 小时窗口不补；对不上模型时取剩余最少的组；认不出时为空。
    #[test]
    fn antigravity_usage_command() {
        let out = json!({"command": {"name": "usage", "data": {"groups": [
            {"name": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro",
             "buckets": [{"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.75, "reset_time": "2026-11-08T06:38:54Z"}]},
            {"name": "Claude and GPT models", "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
             "buckets": [{"id": "3p-weekly", "window": "weekly", "remaining_fraction": 0.4, "reset_time": "2026-11-08T12:51:45Z"}]}]}}});
        let q = antigravity_quotas(&out, Some("gemini-3.8-flash-low"), 7);
        assert_eq!(q.len(), 1, "只有 7 天，不补 5 小时");
        assert_eq!((q[0].window, q[0].used_percent, q[0].resets_at, q[0].as_of), (Window::SevenDay, 25.0, 1_794_119_934_000, 7));
        let c = antigravity_quotas(&out, Some("claude-sonnet-4"), 7);
        assert!((c[0].used_percent - 60.0).abs() < 1e-9, "Claude 模型用另一组");
        let any = antigravity_quotas(&out, None, 7);
        assert!((any[0].used_percent - 60.0).abs() < 1e-9, "不知道模型时取剩余最少的组");
        assert!(antigravity_quotas(&json!({"status": "SUCCESS"}), Some("gemini"), 7).is_empty());
        let odd = json!({"command": {"data": {"groups": [{"name": "Gemini", "buckets": [{"window": "daily", "remaining_fraction": 0.5}]}]}}});
        assert!(antigravity_quotas(&odd, Some("gemini"), 7).is_empty(), "认不出的窗口不用");
    }

    /// 时差写法与 `Z` 等价（Grok 的计费周期写 `+00:00`）。
    #[test]
    fn iso_with_offset() {
        assert_eq!(iso_ms("2026-11-06T04:54:05.164+00:00"), iso_ms("2026-11-06T04:54:05.164Z"));
        assert_eq!(iso_ms("2026-11-06T12:54:05+08:00"), iso_ms("2026-11-06T04:54:05Z"));
        assert_eq!(iso_ms("2026-11-05T23:54:05-05:00"), iso_ms("2026-11-06T04:54:05Z"));
        assert_eq!(iso_ms("2026-11-06T04:54:05"), None, "没有时区的不认");
    }

    /// Copilot 1.0.105 本机记录（免费账号：chat 每月 200 次，premium_interactions 为 0）。
    #[test]
    fn copilot_quota_from_snapshots() {
        let snap = |chat: Value, premium: Value| json!({"type": "model.model_call_success", "timestamp": "2026-11-01T14:07:30.726Z",
            "data": {"quotaSnapshots": {"chat": chat, "completions": {"isUnlimitedEntitlement": false, "entitlementRequests": 2000,
                "remainingPercentage": 100, "resetDate": "2026-12-01T00:00:00Z"}, "premium_interactions": premium}}});
        let limit = |n: u64, left: f64| json!({"isUnlimitedEntitlement": false, "entitlementRequests": n, "remainingPercentage": left,
                                               "resetDate": "2026-12-01T00:00:00Z"});
        let unlimited = json!({"isUnlimitedEntitlement": true, "entitlementRequests": 0, "remainingPercentage": 100, "resetDate": "2026-12-01T00:00:00Z"});
        let free = copilot_quota_line(&snap(limit(200, 99.4), limit(0, 0.0))).unwrap();
        assert_eq!((free.window, free.resets_at, free.unlimited), (Window::Month, iso_ms("2026-12-01T00:00:00Z").unwrap(), false));
        assert!((free.used_percent - 0.6).abs() < 1e-9, "免费账号计 chat，premium 为 0 不算");
        let pro = copilot_quota_line(&snap(unlimited.clone(), limit(300, 88.0))).unwrap();
        assert!((pro.used_percent - 12.0).abs() < 1e-9, "付费账号 chat 不限，计 premium_interactions");
        assert!(copilot_quota_line(&snap(unlimited.clone(), unlimited)).unwrap().unlimited, "都不限：没有限制");
        assert_eq!(copilot_quota_line(&json!({"type": "session.usage_record", "data": {}})), None);
        assert_eq!(copilot_quota_line(&json!({"type": "model.model_call_success", "timestamp": "2026-11-01T14:07:30Z", "data": {}})), None);
    }

    /// Kimi 2.4.1 `kimi web` 本机输出与 `/api/v1/oauth/usage` 返回。
    #[test]
    fn kimi_address_and_quotas() {
        assert_eq!(kimi_web_address("  Local:    http://127.0.0.1:51204/#token=abc-DEF_1.2\n"), Some((51204, "abc-DEF_1.2".into())));
        assert_eq!(kimi_web_address("  Kimi server ready  2.4.1"), None);
        assert_eq!(kimi_web_address("http://127.0.0.1:51204/"), None, "没有令牌");
        let body = json!({"code": 0, "data": {"kind": "ok", "quota": {"usages": {
            "limit5h": {"usedRatio": 0, "resetAt": "2026-11-01T19:05:38Z"},
            "limit7d": {"usedRatio": 0.2573, "resetAt": "2026-11-05T02:05:38Z"}}, "extraUsage": null}}});
        let q = kimi_quotas(&body, 7);
        assert_eq!(q.iter().map(|q| (q.window, q.unlimited)).collect::<Vec<_>>(), [(Window::FiveHour, false), (Window::SevenDay, false)]);
        assert_eq!((q[0].used_percent, q[0].resets_at), (0.0, iso_ms("2026-11-01T19:05:38Z").unwrap()));
        assert!((q[1].used_percent - 25.73).abs() < 1e-9);
        let weekly_only = json!({"data": {"kind": "ok", "quota": {"usages": {"limit7d": {"usedRatio": 0.5, "resetAt": "2026-11-05T02:05:38Z"},
            "monthTotal": {"usedRatio": 0.1, "resetAt": "2026-12-01T00:00:00Z"}}}}});
        assert_eq!(kimi_quotas(&weekly_only, 7).iter().map(|q| q.window).collect::<Vec<_>>(), [Window::SevenDay, Window::Month],
                   "没有 5 小时就不补");
        assert!(kimi_quotas(&json!({"data": {"kind": "error", "message": "Usage endpoint not available"}}), 7).is_empty());
    }

    /// Grok 1.0.70 `_x.ai/billing` 本机返回（SuperGrok，每周一期）。
    #[test]
    fn grok_quotas_from_billing() {
        let result = json!({"config": {"creditUsagePercent": 1.0, "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY",
            "start": "2026-10-30T04:54:05.164+00:00", "end": "2026-11-06T04:54:05.164+00:00"}, "prepaidBalance": {"val": 0}},
            "subscription_tier": "SuperGrok"});
        let q = grok_quotas(&result, 7);
        assert_eq!(q.iter().map(|q| (q.window, q.unlimited)).collect::<Vec<_>>(), [(Window::SevenDay, false)]);
        assert_eq!((q[0].used_percent, q[0].resets_at), (1.0, iso_ms("2026-11-06T04:54:05.164Z").unwrap()));
        let odd = json!({"config": {"creditUsagePercent": 1.0, "currentPeriod": {"type": "USAGE_PERIOD_TYPE_DAILY", "end": "2026-11-02T00:00:00Z"}}});
        assert!(grok_quotas(&odd, 7).is_empty(), "认不出的周期不用");
        assert!(grok_quotas(&json!({"config": {"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY"}}}), 7).is_empty());
    }

    /// 窗口按时长归类；按量计费只认记录里的依据（取自 2026-11-01 本机记录的字段）。
    #[test]
    fn windows_and_api_evidence() {
        assert_eq!([300, 10080, 40320, 43200, 44640, 1440, 0].map(Window::from_minutes),
                   [Some(Window::FiveHour), Some(Window::SevenDay), Some(Window::Month), Some(Window::Month), Some(Window::Month), None, None]);
        let meta = |p: &str| json!({"type": "session_meta", "payload": {"model_provider": p, "cli_version": "0.166.0"}});
        assert!(!codex_api(&meta("openai")) && codex_api(&meta("openrouter")) && codex_api(&meta("azure")));
        assert!(!codex_api(&json!({"type": "session_meta", "payload": {}})), "没有服务商字段：看不出");
        assert!(!codex_api(&json!({"type": "event_msg"})));
        assert!(!kimi_api("kimi-code/kimi-for-coding") && kimi_api("openrouter/anthropic/claude-sonnet-4"));
        assert!(!kimi_api("kimi-for-coding"), "没有服务商段：看不出");
        let rec = |byok: bool| json!({"type": "session.usage_record", "data": {"usage": {"model": "gpt-6-luna", "isByok": byok}}});
        assert_eq!((copilot_byok(&rec(false)), copilot_byok(&rec(true))), (Some(false), Some(true)));
        assert_eq!(copilot_byok(&json!({"type": "assistant.message"})), None);
    }

    /// Antigravity 1.4.12：每次调用模型一行，输入不含缓存（取自本机记录的数字）。
    #[test]
    fn antigravity_usage_lines() {
        let l = json!({"type": "PLANNER_RESPONSE", "input_tokens": 3263, "cache_read_tokens": 4043, "output_tokens": 13});
        assert_eq!(antigravity_usage_line(&l), Some(Tokens { input: 3263, cache_read: 4043, cache_write: 0, output: 13, total: 7319 }));
        assert_eq!(antigravity_usage_line(&json!({"type": "USER_INPUT"})), None);
        assert_eq!(antigravity_usage_line(&json!({"type": "PLANNER_RESPONSE", "content": "x"})), None, "没有用量的不算");
    }

    /// 结构取自本机会话记录（正文已删）。
    #[test]
    fn last_model_in_each_format() {
        let claude = [
            json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "content": []}}),
            json!({"type": "user", "message": {"content": "x"}}),
            json!({"type": "assistant", "message": {"model": "claude-sonnet-5-5"}}),
            json!({"type": "assistant", "message": {"model": "<synthetic>"}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(last_model(ModelSource::ClaudeTranscript, &claude).as_deref(), Some("claude-sonnet-5-5"));
        let kimi = [
            json!({"type": "usage.record", "model": "kimi-code/k3-256k", "usage": {}}),
            json!({"type": "context.append_loop_event"}),
            json!({"type": "usage.record", "model": "kimi-code/kimi-for-coding", "usageScope": "turn"}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(last_model(ModelSource::KimiWire, &kimi).as_deref(), Some("kimi-code/kimi-for-coding"));
        // 格式不认识、半行、别的来源：都是 None。
        assert_eq!(last_model(ModelSource::ClaudeTranscript, "{\"type\":\"assistant\",\"mess"), None);
        assert_eq!(last_model(ModelSource::KimiWire, &claude), None);
        assert_eq!(last_model(ModelSource::None, &claude), None);
        assert_eq!(grok_summary_model(&json!({"current_model_id": "grok-4.7"})).as_deref(), Some("grok-4.7"));
        assert_eq!(grok_summary_model(&json!({"current_model_id": ""})), None);
    }

    /// 推理强度（结构取自 2026-10-23 本机记录，正文已删）：后出现的覆盖先前的；类型不对、空串、别的来源都不算。
    #[test]
    fn last_effort_in_each_format() {
        let lines = |v: &[Value]| v.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let claude = lines(&[
            json!({"type": "assistant", "effort": "high", "message": {"model": "claude-opus-5-5"}}),
            json!({"type": "user", "effort": "low"}),
            json!({"type": "assistant", "effort": "xhigh", "message": {}}),
            json!({"type": "assistant", "message": {"model": "<synthetic>"}}),
        ]);
        assert_eq!(last_effort(EffortSource::ClaudeTranscript, &claude).as_deref(), Some("xhigh"));
        let codex = lines(&[
            json!({"type": "turn_context", "payload": {"model": "gpt-6-astra", "effort": "medium"}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "effort": "low"}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-6-astra", "effort": "xhigh"}}),
        ]);
        assert_eq!(last_effort(EffortSource::CodexRollout, &codex).as_deref(), Some("xhigh"));
        let kimi = lines(&[
            json!({"type": "profile.bind", "thinkingEffort": "high"}),
            json!({"type": "llm.request", "model": "kimi-for-coding", "thinkingEffort": "max"}),
            json!({"type": "llm.request", "thinkingEffort": null}),
        ]);
        assert_eq!(last_effort(EffortSource::KimiWire, &kimi).as_deref(), Some("max"));
        assert_eq!(last_effort(EffortSource::ClaudeTranscript, "{\"type\":\"assistant\",\"effort\":\"\"}"), None);
        assert_eq!(last_effort(EffortSource::CodexRollout, &claude), None);
        assert_eq!(last_effort(EffortSource::None, &claude), None);
        assert_eq!(last_effort(EffortSource::GrokSummary, &claude), None);
        assert_eq!(grok_summary_effort(&json!({"reasoning_effort": "low", "current_model_id": "grok-4.7"})).as_deref(), Some("low"));
        assert_eq!(grok_summary_effort(&json!({"reasoning_effort": null})), None);
    }

    /// Token 与周额度（结构取自 2026-10-16 本机记录，数值即当时的值）。
    #[test]
    fn tokens_and_quota_in_each_format() {
        let c = json!({"type": "assistant", "message": {"id": "msg_1", "usage": {"input_tokens": 2,
            "cache_creation_input_tokens": 1633, "cache_read_input_tokens": 181476, "output_tokens": 439}}});
        let (id, t) = claude_usage_line(&c).unwrap();
        assert_eq!((id.as_str(), t), ("msg_1", Tokens { input: 2, cache_read: 181476, cache_write: 1633, output: 439, total: 183550 }));
        assert_eq!(claude_usage_line(&json!({"type": "user"})), None);
        let k = json!({"type": "usage.record", "usageScope": "turn", "usage": {"inputOther": 11227, "output": 267,
            "inputCacheRead": 18944, "inputCacheCreation": 0}});
        assert_eq!(kimi_usage_line(&k), Some(Tokens { input: 11227, cache_read: 18944, cache_write: 0, output: 267, total: 30438 }));
        let rollout = [
            json!({"timestamp": "2026-10-15T13:31:40.500Z", "type": "event_msg", "payload": {"type": "token_count", "info": null}}),
            json!({"timestamp": "2026-10-15T13:31:42.250Z", "type": "event_msg", "payload": {"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 31139, "cached_input_tokens": 13056, "cache_write_input_tokens": 0,
                    "output_tokens": 12, "reasoning_output_tokens": 0, "total_tokens": 31151}},
                "rate_limits": {"primary": {"used_percent": 3.0, "window_minutes": 10080, "resets_at": 1792675611}, "secondary": null}}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let (t, q, unknown) = codex_last_usage(&rollout).unwrap();
        assert_eq!(t, Tokens { input: 18083, cache_read: 13056, cache_write: 0, output: 12, total: 31151 });
        let at = 1_792_071_102_250;
        // 只有 7 天窗口（本机 prolite 套餐）：只有这一项，不补 5 小时。
        assert_eq!(q, vec![Quota { window: Window::SevenDay, used_percent: 3.0, resets_at: 1_792_675_611_000, as_of: at, unlimited: false }]);
        assert!(unknown.is_empty());
        // primary 是 5 小时、secondary 是 7 天：两个都取。
        let both = rollout.replace("\"window_minutes\":10080", "\"window_minutes\":300")
            .replace("\"secondary\":null", "\"secondary\":{\"used_percent\":40.5,\"window_minutes\":10080,\"resets_at\":1792675611}");
        let q = codex_last_usage(&both).unwrap().1;
        assert_eq!(q.iter().map(|q| (q.window, q.used_percent, q.unlimited)).collect::<Vec<_>>(),
                   [(Window::FiveHour, 3.0, false), (Window::SevenDay, 40.5, false)]);
        let q = codex_last_usage(&rollout.replace("10080", "300")).unwrap().1;
        assert_eq!(q.iter().map(|q| q.window).collect::<Vec<_>>(), [Window::FiveHour]);
        let month = codex_last_usage(&rollout.replace("10080", "43200")).unwrap().1;
        assert_eq!(month.iter().map(|q| q.window).collect::<Vec<_>>(), [Window::Month], "以后有月额度也按时长归类");
        let (_, q, unknown) = codex_last_usage(&rollout.replace("10080", "1440")).unwrap();
        assert!(q.is_empty() && unknown == [1440], "认不出的时长不收，交给调用方记日志");
        assert!(codex_last_usage(&rollout.replace(",\"rate_limits\"", ",\"x\"")).unwrap().1.is_empty(), "没有 rate_limits");
        assert_eq!(codex_last_usage("{\"type\":\"session_meta\"}"), None);
        let g = json!({"session": {"inputTokens": 24601, "outputTokens": 1, "cachedReadTokens": 1152, "cacheCreationTokens": 0,
            "reasoningTokens": 80, "totalTokens": 24602}});
        assert_eq!(grok_usage(&g), Some(Tokens { input: 23449, cache_read: 1152, cache_write: 0, output: 1, total: 24602 }));
        let sl = json!({"rate_limits": {"five_hour": {"used_percentage": 10, "resets_at": 1}, "seven_day": {"used_percentage": 34.5, "resets_at": 1792675611}}});
        assert_eq!(claude_statusline_quotas(&sl, 7), [
            Quota { window: Window::FiveHour, used_percent: 10.0, resets_at: 1000, as_of: 7, unlimited: false },
            Quota { window: Window::SevenDay, used_percent: 34.5, resets_at: 1_792_675_611_000, as_of: 7, unlimited: false },
        ]);
        assert!(claude_statusline_quotas(&json!({"rate_limits": {}}), 7).is_empty(), "非 Pro / Max 或首次响应前没有该字段");
    }

    /// 结构取自 2026-10-30 本机 Copilot 1.0.105 与 OpenCode 1.19.5 的记录（数值照抄，其余字段已删）。
    #[test]
    fn copilot_and_opencode_usage() {
        let rec = json!({"type": "session.usage_record", "data": {"usage": {"model": "gpt-6-luna", "inputTokens": 20192,
                         "outputTokens": 28, "cacheReadTokens": 9893, "cacheWriteTokens": 10296, "reasoningTokens": 0}}});
        assert_eq!(copilot_usage_line(&rec), Some(Tokens { input: 3, cache_read: 9893, cache_write: 10296, output: 28, total: 20220 }),
                   "输入与同一会话 session.shutdown 的汇总（3）一致");
        assert_eq!(copilot_usage_line(&json!({"type": "assistant.message"})), None);
        let msgs = [json!({"type": "assistant.message", "data": {"model": "gpt-6-luna", "content": "en"}}),
                    json!({"type": "assistant.turn_end", "data": {}})]
            .iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        assert_eq!(last_model(ModelSource::CopilotEvents, &msgs).as_deref(), Some("gpt-6-luna"));
        let step = json!({"type": "step-finish", "tokens": {"total": 17041, "input": 15240, "output": 2, "reasoning": 7,
                          "cache": {"write": 0, "read": 1792}}, "cost": 0});
        assert_eq!(opencode_step_tokens(&step), Some(Tokens { input: 15240, cache_read: 1792, cache_write: 0, output: 9, total: 17041 }));
        assert_eq!(opencode_step_tokens(&json!({"type": "text", "text": "OK"})), None);
    }
}
