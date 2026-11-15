#!/usr/bin/env python3
"""M2 第 3 块：状态行 Total 与周额度的实测（开发用，不进入产品运行时；docs/design.md 10.2 节第 3 块 ①②④）。

在伪终端里运行四款 CLI 的交互界面（复用 acceptance-parallel.py 的 Session；四款都用真实 HOME、
在已信任的项目目录里运行），挂到 App 以 SCYLLAUI_FIXTURE_TERMINALS 登记的夹具终端。每款提问两轮，每轮结束（后台状态回到 ready）
后轮询后台快照的 `usage.total_tokens`，与本脚本独立地从同一份会话记录算出的值比对，记录「一轮结束 → 后台值与独立计算一致」的
延迟。Codex 另在最后发 `/status`，截下界面上的用量与周额度原文，与后台快照的 `quotas` 一并写入报告供核对。
独立计算按 docs/design.md 第 5 节「实现（M2 第 3 块）」的口径，不调用 Rust 代码。
用法：usage-check.py <测试目录> [工具…]   结果写 <测试目录>/usage-report.json。
"""
import importlib.util
import json
import re
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("acceptance_parallel", HERE / "acceptance-parallel.py")
ap = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ap)
# 2026-10-16：临时 HOME 里已没有 Codex 登录，复制凭据有续期使真实凭据失效的风险（同 Kimi），Codex 也改用真实 HOME
# （真实配置里 ScyllaUI 的 Hook 已信任，不再跳过信任），关掉启动时的更新检查。
ap.REAL_HOME_TOOLS = ("claude", "codex", "kimi", "grok")
ap.CMDS["codex"] = ["codex", "--no-daemon", "-c", "check_for_update_on_startup=false"]
REAL = Path(ap.REAL_HOME)


def lines(path):
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    out = []
    for l in text.split("\n")[:-1]:   # 末尾未写完的半行不算
        try:
            out.append(json.loads(l))
        except ValueError:
            pass
    return out


def claude_total(conv):
    seen, total = set(), 0
    for v in lines(conv.get("transcript_path") or ""):
        m = v.get("message") or {}
        if v.get("type") != "assistant" or "usage" not in m or m.get("id") in seen:
            continue
        seen.add(m.get("id"))
        u = m["usage"]
        total += sum(u.get(k) or 0 for k in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"))
    return total or None


def codex_last(conv):
    last = None
    for v in lines(conv.get("transcript_path") or ""):
        p = v.get("payload") or {}
        if v.get("type") == "event_msg" and p.get("type") == "token_count" and isinstance(p.get("info"), dict):
            last = v
    return last


def codex_total(conv):
    last = codex_last(conv)
    return last and last["payload"]["info"]["total_token_usage"]["total_tokens"]


def kimi_total(conv):
    idx = [json.loads(l) for l in (REAL / ".kimi-code/session_index.jsonl").read_text().splitlines() if l.strip()]
    rows = [r for r in idx if r.get("sessionId") == conv.get("native_id")]
    if not rows:
        return None
    d = rows[-1]["sessionDir"]
    d = REAL / d[2:] if d.startswith("~/") else Path(d)
    total = 0
    for v in lines(d / "agents/main/wire.jsonl"):
        if v.get("type") == "usage.record":
            u = v.get("usage") or {}
            total += sum(u.get(k) or 0 for k in ("inputOther", "inputCacheRead", "inputCacheCreation", "output"))
    return total or None


def grok_total(conv):
    for p in (REAL / ".grok/sessions").glob(f"*/{conv.get('native_id')}/usage.json"):
        return (json.loads(p.read_text()).get("session") or {}).get("totalTokens")
    return None


TOTAL = {"claude": claude_total, "codex": codex_total, "kimi": kimi_total, "grok": grok_total}


def look(pane):
    terms, snap = ap.states()
    t = terms.get(pane) or {}
    cid = t.get("conversation_id")
    conv = next((c for c in snap["conversations"] if c["conversation_id"] == cid), None)
    usage = next((u["usage"] for u in snap.get("usage", []) if u["conversation_id"] == cid), None)
    return (t.get("agent") or {}).get("state"), conv, usage, snap


def screen(s):
    s.log.flush()
    t = re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)", b"", open(s.log.name, "rb").read())
    return " ".join(t.decode("utf-8", "replace").split())


def turn(s, n, report):
    """提一轮问题：等后台进入 running 再回到 ready，之后每 0.25 s 比对后台值与独立计算，最多 10 s。"""
    before = (look(s.pane)[2] or {}).get("total_tokens")
    s.say("只回复 OK")
    end = time.time() + 120
    busy = False
    while time.time() < end:
        state = look(s.pane)[0]
        busy = busy or state == "running"
        if busy and state == "ready":
            break
        time.sleep(0.25)
    ready = time.time()
    row = {"round": n, "ready_seen": busy}
    while time.time() < ready + 10:
        _, conv, usage, _ = look(s.pane)
        got = (usage or {}).get("total_tokens")
        want = TOTAL[s.tool](conv) if conv else None
        if got is not None and got == want and got != before:
            row.update(daemon=got, independent=want, delay_s=round(time.time() - ready, 2), tokens=usage.get("tokens"))
            break
        time.sleep(0.25)
    else:
        row.update(daemon=got, independent=want, delay_s=None)
    time.sleep(7)   # 5 秒后的补读之后再核对一次
    _, conv, usage, _ = look(s.pane)
    row["after_7s"] = {"daemon": (usage or {}).get("total_tokens"), "independent": TOTAL[s.tool](conv) if conv else None}
    report.setdefault(s.tool, []).append(row)
    print(f"  {s.tool} 第 {n} 轮：{row}")


def main():
    base = Path(sys.argv[1])
    tools = sys.argv[2:] or ["claude", "codex", "kimi", "grok"]
    ap.SOCK = str(base / "data/scyllauid.sock")
    ap.prepare_home(str(base / "home"))
    panes = [t["terminal_id"] for t in json.loads((base / "fixture.json").read_text())]
    log_dir = base / "pty"
    log_dir.mkdir(exist_ok=True)
    report = {"started": time.strftime("%Y-%m-%d %H:%M:%S")}
    sessions = [ap.Session(tool, panes[i], str(base / "home"), str(base / "work"), log_dir) for i, tool in enumerate(tools)]
    try:
        time.sleep(12)
        ap.accept_trust(sessions)
        time.sleep(2)
        for n in (1, 2):
            for s in sessions:
                turn(s, n, report)
        for s in sessions:
            if s.tool == "codex":
                s.say("/status")
                time.sleep(4)
                text = screen(s)
                i = text.rfind("/status")
                report["codex_status_screen"] = text[i:i + 1500]
                last = codex_last(look(s.pane)[1] or {})
                report["codex_rollout_rate_limits"] = last and last["payload"].get("rate_limits")
                s.write(b"\x1b")
        report["quotas"] = look(sessions[0].pane)[3].get("quotas")
        print("  quotas：", report["quotas"])
    finally:
        for s in sessions:
            if s.running():
                s.exit()
        time.sleep(8)
        for s in sessions:
            s.kill()
    (base / "usage-report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
