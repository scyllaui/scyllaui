//! 会话是否真的在同步（docs/design.md 4.7 节「会话是否真的在同步」，2026-11-20）：配置检查只回答「配置文件对不对」，
//! 这里回答「这个正在运行的 Agent 进程有没有在同步」。两部分：
//! - 时间线：每款 Hook 配置状态变化的时间，存在 `<数据目录>/hook-timeline.json`，后台重启后还在。用状态变化的时间、
//!   不用配置文件的修改时间：配置文件里还有别的内容，别人一改修改时间就变。
//! - 判断：按适配器声明的「何时读取 Hook」、进程的启动时间、它最近一次发来事件的时间，得出这个进程的同步情况。
//!   只认确定的事实（进程启动早于条目写入、变化之后还收到过事件），不按「多久没收到 Hook」判断。
//! - 对账（2026-11-21）：「读到过条目的进程，内存里的那份还在跑」是推断。靠它得出的结论由核心线程用这个会话的会话记录
//!   核对：记录里有它提交的提示词而事件没到（`Facts::silent`），就是没在同步。

use scyllaui_proto::adapters::HookLoad;
use scyllaui_proto::hookcfg::State;
use scyllaui_proto::Tool;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// 每款保留的条数。原为 8：2026-11-21 正式版一个会话存活期间 Codex 配置变了 8 次（开发调试），它启动那一刻的记录被删、
/// 判断落到「查不到不提示」，没读到条目的会话漏标「未同步」；正常使用不会变这么多次，改为 16（用户决定）。
const KEEP: usize = 16;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// 从这个时刻起确知是这个状态（Unix 毫秒）。
    at: i64,
    state: State,
    /// 变化发生在后台没在运行的时候（或这是第一次看到这款）：真正变化的时刻不可知，只知道不晚于 `at`。
    gap: bool,
}

#[derive(Default)]
pub struct Timeline {
    /// None：不落盘（测试）。
    path: Option<PathBuf>,
    tools: BTreeMap<&'static str, Vec<Entry>>,
}

fn parse_state(s: &str) -> Option<State> {
    [State::Ok, State::Missing, State::Disabled, State::Untrusted, State::Policy, State::Unreadable, State::Absent]
        .into_iter()
        .find(|x| x.as_str() == s)
}

impl Timeline {
    /// 读已有的文件；缺失或内容非法时从空开始。
    pub fn load(path: PathBuf) -> Timeline {
        let tools = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| {
                        let tool = Tool::parse(&k)?;
                        let list: Vec<Entry> = v
                            .as_array()?
                            .iter()
                            .filter_map(|e| {
                                Some(Entry {
                                    at: e.get("at")?.as_i64()?,
                                    state: parse_state(e.get("state")?.as_str()?)?,
                                    gap: e.get("gap").and_then(Value::as_bool).unwrap_or(false),
                                })
                            })
                            .collect();
                        (!list.is_empty()).then_some((tool.as_str(), list))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Timeline { path: Some(path), tools }
    }

    /// 记下这款此刻的状态；与最近一条相同则不记。`gap`：这次变化不是后台亲眼看到的（启动后的第一次分析）。
    /// 返回是否记了新的一条。
    pub fn observe(&mut self, tool: Tool, state: State, now: i64, gap: bool) -> bool {
        let list = self.tools.entry(tool.as_str()).or_default();
        if list.last().is_some_and(|e| e.state == state) {
            return false;
        }
        // 第一次看到这款：从什么时候起是这个状态不可知。
        let gap = gap || list.is_empty();
        list.push(Entry { at: now, state, gap });
        if list.len() > KEEP {
            let drop = list.len() - KEEP;
            list.drain(..drop);
        }
        self.save();
        true
    }

    /// 这款最近一次状态变化的时间。
    pub fn changed_at(&self, tool: Tool) -> Option<i64> {
        self.tools.get(tool.as_str()).and_then(|l| l.last()).map(|e| e.at)
    }

    /// `at` 那一刻这款的状态；不可知（早于最早一条，或落在一次时刻不明的变化之前）为 None。
    pub fn state_at(&self, tool: Tool, at: i64) -> Option<State> {
        let list = self.tools.get(tool.as_str())?;
        let i = list.iter().rposition(|e| e.at <= at)?;
        match list.get(i + 1) {
            Some(next) if next.gap => None,
            _ => Some(list[i].state),
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let map: Map<String, Value> = self
            .tools
            .iter()
            .map(|(k, l)| {
                let list: Vec<Value> = l.iter().map(|e| json!({"at": e.at, "state": e.state.as_str(), "gap": e.gap})).collect();
                ((*k).to_owned(), json!(list))
            })
            .collect();
        let text = serde_json::to_string(&map).unwrap_or_default();
        if fs::write(&tmp, text + "\n").and_then(|_| fs::rename(&tmp, path)).is_err() {
            let _ = fs::remove_file(&tmp);
            log!("写 {} 失败", path.display());
        }
    }
}

/// 一个前台 Agent 进程的同步情况；在同步且配置正常时为 None。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// 配置没生效，这个进程没在同步。
    Off,
    /// 配置已失效，但这个进程还在用启动时读到的条目同步；退出后就不同步了。
    Old,
    /// 配置正常，但这个进程启动时条目还没写入：开新会话或重开后才同步。
    NewSession,
    /// 同上，这款要退出重开才读得到。
    Restart,
}

impl Sync {
    pub fn as_str(self) -> &'static str {
        match self {
            Sync::Off => "off",
            Sync::Old => "old",
            Sync::NewSession => "new_session",
            Sync::Restart => "restart",
        }
    }
}

/// 本项目的条目在不在（不论有没有生效）。读不了的文件不算在，也不算不在：按不可知处理。
pub fn entries_present(state: State) -> Option<bool> {
    match state {
        State::Ok | State::Untrusted | State::Disabled | State::Policy => Some(true),
        State::Missing | State::Absent => Some(false),
        State::Unreadable => None,
    }
}

pub struct Facts {
    pub load: HookLoad,
    /// 这款现在的配置状态。
    pub now: State,
    /// 这款最近一次状态变化的时间。
    pub changed: Option<i64>,
    /// 这个进程的启动时间。
    pub started: Option<i64>,
    /// 这个进程最近一次发来 Hook 事件的时间。
    pub last_hook: Option<i64>,
    /// 这个进程启动那一刻这款的状态（时间线上查到的）。
    pub at_start: Option<State>,
    /// 这个进程的会话记录里有一条用户提交的提示词（这个时刻），而等过 `SUBMIT_WAIT_MS` 也没收到它的事件。
    pub silent: Option<i64>,
}

/// 核心线程在会话记录里读到一条提示词后等这么久，再看它的事件到了没有（我们写入的 Hook 条目超时是 2 秒，
/// `hookcfg::TIMEOUT`）。
pub const SUBMIT_WAIT_MS: i64 = 5_000;
/// 提交事件比会话记录里的那条提示词早到（2026-11-21 四款实测早 1—73 ms），记录里的时间有的只精确到秒（Grok）：
/// 提示词时刻之前这么久以内收到的事件也算这条提示词的。
pub const SUBMIT_EARLY_MS: i64 = 2_000;

/// 变化之后还收到过这个进程的事件：它在同步。
fn evidence(f: &Facts) -> bool {
    f.last_hook.is_some_and(|e| f.changed.is_none_or(|t| e > t) && f.started.is_none_or(|s| e >= s))
}

/// 提交了提示词而事件没到，之后也没再收到它的事件：这个进程没在同步，不论按读取时机怎么推断。
fn silent(f: &Facts) -> bool {
    f.load != HookLoad::Live && f.silent.is_some_and(|p| f.last_hook.is_none_or(|e| e < p - SUBMIT_EARLY_MS))
}

/// 结论靠的是推断（「读到过条目的进程，内存里的那份还在跑」），要用会话记录对账（docs/design.md 4.7 节）：
/// - 结论是 `old`：配置已失效，全靠内存里的那份，之后的每条提示词都核对；
/// - 配置正常、这个进程活着的期间配置变过、变化之后还没收到过它的事件：算它在同步只是因为它以前读到过条目。
///
/// 运行中重读的几款不靠推断；配置从没变过的会话也不核对。已经得出没在同步的不用再核对。
pub fn inferred(f: &Facts) -> bool {
    if f.load == HookLoad::Live {
        return false;
    }
    match sync_of(f) {
        Some(Sync::Old) => true,
        None => f.now == State::Ok && !evidence(f) && matches!((f.changed, f.started), (Some(t), Some(s)) if t > s),
        _ => false,
    }
}

pub fn sync_of(f: &Facts) -> Option<Sync> {
    // 没装这款（配置目录不存在）：不提示，同配置检查。
    if f.now == State::Absent {
        return None;
    }
    let ok = f.now == State::Ok;
    // 会话记录对账出来的事实优先于下面的推断，也优先于更早收到的事件（变化之后同步过一阵、后来不同步了）。
    if silent(f) {
        return Some(match (ok, f.load) {
            (false, _) => Sync::Off,
            (true, HookLoad::Process) => Sync::Restart,
            (true, _) => Sync::NewSession,
        });
    }
    let evidence = evidence(f);
    if evidence {
        return (!ok).then_some(Sync::Old);
    }
    if f.load == HookLoad::Live {
        return (!ok).then_some(Sync::Off);
    }
    // 这个进程发来过事件：它读到过能用的条目，内存里的那份还在跑（会话开始类在 /new 时读到的也算）。
    let ever = f.last_hook.is_some_and(|e| f.started.is_none_or(|s| e >= s));
    if ok {
        // 只有「启动时条目确实不在、之后也从没收到过它的事件」才提示；不可知时宁可不提示。
        // 发来过事件的不提示：条目丢了又修回来，它一直在用内存里的那份同步（2026-11-20 实机发现误报）。
        return match f.at_start.and_then(entries_present) {
            Some(false) if ever => None,
            Some(false) if f.load == HookLoad::Process => Some(Sync::Restart),
            Some(false) => Some(Sync::NewSession),
            _ => None,
        };
    }
    // 配置不正常：现在是条目没了或换了内容（未信任）或读不了，而这个进程读到过能用的条目——启动时配置正常，或者它
    // 发来过事件（启动时还待信任、随后在启动询问里信任的，时间线上启动那一刻不是正常，2026-11-20 实机发现）——
    // 内存里的旧条目还在跑。停用、策略锁定可能是在这个会话里当场生效的，按没在同步处理（之后又收到事件会改回来）。
    let lost_on_disk = matches!(f.now, State::Missing | State::Untrusted | State::Unreadable);
    if lost_on_disk && (f.at_start == Some(State::Ok) || ever) {
        Some(Sync::Old)
    } else {
        Some(Sync::Off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;

    #[test]
    fn timeline_answers_state_at_a_time_and_survives_reload() {
        let d = test_dir("hook-timeline");
        let path = d.join("hook-timeline.json");
        let mut t = Timeline::load(path.clone());
        assert_eq!(t.state_at(Tool::Codex, 100), None, "没有记录");
        assert!(t.observe(Tool::Codex, State::Missing, 1_000, false), "第一次看到");
        assert!(!t.observe(Tool::Codex, State::Missing, 1_500, false), "状态没变不记");
        assert!(t.observe(Tool::Codex, State::Untrusted, 2_000, false));
        assert!(t.observe(Tool::Codex, State::Ok, 3_000, false));
        assert_eq!(t.state_at(Tool::Codex, 999), None, "早于最早一条：不可知");
        assert_eq!(t.state_at(Tool::Codex, 1_000), Some(State::Missing));
        assert_eq!(t.state_at(Tool::Codex, 1_999), Some(State::Missing));
        assert_eq!(t.state_at(Tool::Codex, 2_000), Some(State::Untrusted));
        assert_eq!(t.state_at(Tool::Codex, 9_999), Some(State::Ok));
        assert_eq!(t.changed_at(Tool::Codex), Some(3_000));
        assert_eq!(t.state_at(Tool::Kimi, 2_000), None, "别的工具没有记录");
        let again = Timeline::load(path.clone());
        assert_eq!(again.state_at(Tool::Codex, 2_500), Some(State::Untrusted), "重启后还在");
        // 后台没在运行时发生的变化：时刻不明，它之前那段不可知。
        let mut again = again;
        assert!(again.observe(Tool::Codex, State::Missing, 5_000, true));
        assert_eq!(again.state_at(Tool::Codex, 4_000), None, "变化可能发生在 4000 之前");
        assert_eq!(again.state_at(Tool::Codex, 2_500), Some(State::Untrusted), "更早的记录不受影响");
        assert_eq!(again.state_at(Tool::Codex, 5_000), Some(State::Missing));
        fs::write(&path, "不是 JSON").unwrap();
        assert_eq!(Timeline::load(path).changed_at(Tool::Codex), None, "文件坏了从空开始");
    }

    #[test]
    fn timeline_keeps_only_recent_entries() {
        let mut t = Timeline::default();
        for i in 0..20 {
            t.observe(Tool::Kimi, if i % 2 == 0 { State::Missing } else { State::Ok }, 1_000 + i, false);
        }
        assert_eq!(t.tools["kimi"].len(), KEEP);
        assert_eq!(t.changed_at(Tool::Kimi), Some(1_019));
    }

    /// 回归（2026-11-21 正式版，Codex 上发现；时间线是每款一份、条数上限共用，七款都要覆盖）：条目不在时启动的会话，
    /// 之后同一款的配置撤销、修复来回 9 次，修复后仍查得到它启动那一刻条目不在：会话开始时读的提示开新会话，进程启动时读的
    /// 提示重开，运行中重读的已按新配置在同步、不提示。只留 8 条时那一刻的记录被删，前两类被判成在同步。
    #[test]
    fn a_session_outlives_nine_config_changes_and_still_needs_reopening() {
        use scyllaui_proto::adapters::adapter;
        let tools = [Tool::Claude, Tool::Codex, Tool::Kimi, Tool::Grok, Tool::OpenCode, Tool::Copilot, Tool::Antigravity];
        let mut t = Timeline::default();
        for &tool in &tools {
            t.observe(tool, State::Missing, 1_000, false);
            for i in 0..9 {
                t.observe(tool, if i % 2 == 0 { State::Ok } else { State::Missing }, 2_000 + i, false);
            }
        }
        let started = 1_500;
        for tool in tools {
            let load = adapter(tool).hook_load;
            let f = Facts {
                load,
                now: State::Ok,
                changed: t.changed_at(tool),
                started: Some(started),
                last_hook: None,
                at_start: t.state_at(tool, started),
                silent: None,
            };
            assert_eq!(f.at_start, Some(State::Missing), "{tool:?}");
            let want = match load {
                HookLoad::Session => Some(Sync::NewSession),
                HookLoad::Process => Some(Sync::Restart),
                HookLoad::Live => None,
            };
            assert_eq!(sync_of(&f), want, "{tool:?}");
        }
    }

    fn facts(load: HookLoad, now: State) -> Facts {
        Facts { load, now, changed: Some(2_000), started: Some(1_000), last_hook: None, at_start: None, silent: None }
    }

    /// 会话记录对账（2026-11-21 正式版实测：Codex 一个空闲 6 小时的会话撤销后不再发事件，却被判成 `old`）。
    #[test]
    fn a_prompt_in_the_record_without_its_event_means_not_syncing() {
        const P: i64 = 60_000;
        for load in [HookLoad::Session, HookLoad::Process] {
            // 撤销后靠内存里的条目同步（old）：提交了提示词而事件没到，改判没在同步。
            for now in [State::Missing, State::Untrusted, State::Unreadable] {
                let f = Facts { at_start: Some(State::Ok), last_hook: Some(1_500), ..facts(load, now) };
                assert_eq!(sync_of(&f), Some(Sync::Old));
                assert!(inferred(&f), "{load:?} {now:?} 靠推断，要核对");
                let f = Facts { silent: Some(P), ..f };
                assert_eq!(sync_of(&f), Some(Sync::Off), "{load:?} {now:?}");
                assert!(!inferred(&f), "已经得出没在同步，不用再核对");
            }
            // 撤销之后同步过一阵（变化之后收到过事件）、后来不同步了：以后来的事实为准。
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(30_000), ..facts(load, State::Missing) };
            assert_eq!(sync_of(&f), Some(Sync::Old));
            assert!(inferred(&f), "old 一直是推断，之后的提示词照样核对");
            assert_eq!(sync_of(&Facts { silent: Some(P), ..f }), Some(Sync::Off));
            // 修复回去以后它仍不发事件：配置正常，要开新会话或重开。
            let reopen = if load == HookLoad::Process { Sync::Restart } else { Sync::NewSession };
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(1_500), ..facts(load, State::Ok) };
            assert_eq!(sync_of(&f), None);
            assert!(inferred(&f), "配置变过、变化之后还没收到过它的事件");
            let f = Facts { silent: Some(P), ..f };
            assert_eq!(sync_of(&f), Some(reopen), "{load:?}");
            // 之后又收到它的事件（开了新会话、重开）：照事实改回在同步；这条提示词自己的事件（早到几毫秒）也算。
            assert_eq!(sync_of(&Facts { last_hook: Some(P + 90_000), ..f }), None);
            assert_eq!(sync_of(&Facts { last_hook: Some(P - 20), ..f }), None);
            assert_eq!(sync_of(&Facts { last_hook: Some(P - SUBMIT_EARLY_MS - 1), ..f }), Some(reopen), "更早的事件不是这条提示词的");
            // 配置不正常时也一样改回 old。
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(P + 1), silent: Some(P), ..facts(load, State::Missing) };
            assert_eq!(sync_of(&f), Some(Sync::Old));
            // 配置正常、变化之后收到过事件：是事实，不核对。
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(2_500), ..facts(load, State::Ok) };
            assert!(!inferred(&f));
            // 这个进程启动之后配置没变过：不核对。
            let f = Facts { changed: Some(500), at_start: Some(State::Ok), last_hook: Some(1_500), ..facts(load, State::Ok) };
            assert!(!inferred(&f));
            let f = Facts { changed: None, at_start: Some(State::Ok), ..facts(load, State::Ok) };
            assert!(!inferred(&f), "时间线上没有记录");
            // 已经在提示重开、或没装这款的：不核对。
            let f = Facts { at_start: Some(State::Missing), ..facts(load, State::Ok) };
            assert!(sync_of(&f).is_some() && !inferred(&f));
            assert!(!inferred(&Facts { silent: Some(P), ..facts(load, State::Absent) }));
            assert_eq!(sync_of(&Facts { silent: Some(P), ..facts(load, State::Absent) }), None);
        }
        // 运行中重读的几款不靠推断：不核对，记了也不理会。
        let f = Facts { last_hook: Some(1_500), ..facts(HookLoad::Live, State::Ok) };
        assert!(!inferred(&f));
        assert_eq!(sync_of(&Facts { silent: Some(P), ..f }), None);
        let f = Facts { last_hook: Some(2_500), ..facts(HookLoad::Live, State::Missing) };
        assert_eq!(sync_of(&f), Some(Sync::Old));
        assert!(!inferred(&f));
    }

    #[test]
    fn live_tools_follow_the_config_at_once() {
        assert_eq!(sync_of(&facts(HookLoad::Live, State::Ok)), None);
        assert_eq!(sync_of(&facts(HookLoad::Live, State::Missing)), Some(Sync::Off), "撤销后立即不同步");
        assert_eq!(sync_of(&facts(HookLoad::Live, State::Disabled)), Some(Sync::Off));
        assert_eq!(sync_of(&facts(HookLoad::Live, State::Absent)), None, "没装不提示");
        // 配置失效之后还收到过它的事件：按事实算，还在同步。
        let f = Facts { last_hook: Some(2_500), ..facts(HookLoad::Live, State::Missing) };
        assert_eq!(sync_of(&f), Some(Sync::Old));
    }

    #[test]
    fn a_session_started_before_the_entries_were_written_needs_reopening() {
        // 先开会话、之后才修复：配置正常，但这个进程启动时条目不在。
        let f = Facts { at_start: Some(State::Missing), ..facts(HookLoad::Session, State::Ok) };
        assert_eq!(sync_of(&f), Some(Sync::NewSession));
        let f = Facts { at_start: Some(State::Absent), ..facts(HookLoad::Process, State::Ok) };
        assert_eq!(sync_of(&f), Some(Sync::Restart));
        // 之后收到过它的事件（如 /new 之后提交了一条）：在同步，不再提示。
        let f = Facts { at_start: Some(State::Missing), last_hook: Some(2_001), ..facts(HookLoad::Session, State::Ok) };
        assert_eq!(sync_of(&f), None);
        // 这个进程发来过事件（哪怕在最近一次变化之前）：它读到过条目、内存里的还在跑，条目丢了又修回来也不提示。
        let f = Facts { at_start: Some(State::Missing), last_hook: Some(1_500), ..facts(HookLoad::Session, State::Ok) };
        assert_eq!(sync_of(&f), None);
        // 上一个进程留下的事件（早于这个进程启动）不算。
        let f = Facts { at_start: Some(State::Missing), last_hook: Some(900), ..facts(HookLoad::Session, State::Ok) };
        assert_eq!(sync_of(&f), Some(Sync::NewSession));
        // 启动时条目在（未信任、停用也算在），或查不到：不提示。
        for at in [Some(State::Ok), Some(State::Untrusted), Some(State::Disabled), Some(State::Unreadable), None] {
            let f = Facts { at_start: at, ..facts(HookLoad::Session, State::Ok) };
            assert_eq!(sync_of(&f), None, "{at:?}");
        }
    }

    #[test]
    fn a_session_that_loaded_the_entries_keeps_syncing_after_they_are_removed() {
        // 会话中途撤销：会话开始类、进程启动类还在用启动时读到的条目。
        for load in [HookLoad::Session, HookLoad::Process] {
            for now in [State::Missing, State::Untrusted, State::Unreadable] {
                let f = Facts { at_start: Some(State::Ok), ..facts(load, now) };
                assert_eq!(sync_of(&f), Some(Sync::Old), "{load:?} {now:?}");
            }
            // 停用、策略锁定：按没在同步处理。
            for now in [State::Disabled, State::Policy] {
                let f = Facts { at_start: Some(State::Ok), ..facts(load, now) };
                assert_eq!(sync_of(&f), Some(Sync::Off), "{load:?} {now:?}");
            }
            // 失效之后才启动的，或启动时就没生效、查不到的：没在同步。
            for at in [Some(State::Missing), Some(State::Untrusted), None] {
                let f = Facts { at_start: at, ..facts(load, State::Missing) };
                assert_eq!(sync_of(&f), Some(Sync::Off), "{load:?} {at:?}");
            }
            // 启动时还待信任、在启动询问里信任后发来过事件（变化之前的事件也算「读到过能用的条目」）：还在同步。
            let f = Facts { at_start: Some(State::Untrusted), last_hook: Some(1_500), ..facts(load, State::Missing) };
            assert_eq!(sync_of(&f), Some(Sync::Old), "{load:?} 发来过事件");
            // 上一个进程留下的事件（早于这个进程启动）不算。
            let f = Facts { at_start: Some(State::Untrusted), last_hook: Some(900), ..facts(load, State::Missing) };
            assert_eq!(sync_of(&f), Some(Sync::Off), "{load:?} 别的进程的事件");
            // 停用：发来过事件也按没在同步（除非停用之后又收到）。
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(1_500), ..facts(load, State::Disabled) };
            assert_eq!(sync_of(&f), Some(Sync::Off));
            // 停用之后又收到它的事件：还在同步。
            let f = Facts { at_start: Some(State::Ok), last_hook: Some(2_100), ..facts(load, State::Disabled) };
            assert_eq!(sync_of(&f), Some(Sync::Old));
        }
    }
}
