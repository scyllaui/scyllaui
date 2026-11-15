#!/usr/bin/env python3
"""M2 第 2 块：状态行的模型实测（开发用，不进入产品运行时；docs/design.md 10.2 节第 2 块 ③）。

在伪终端里运行 Claude Code 与 Kimi Code 的交互界面（复用 acceptance-parallel.py 的 Session，真实 HOME、已信任的项目目录），
挂到 App 以 SCYLLAUI_FIXTURE_TERMINALS 登记的夹具终端。各提问一次，读后台快照里该对话的 `usage`（模型与来源），
并从 CLI 自己的界面输出里找它显示的模型名。
不做会话中换模型（`/model`）：Claude Code 会把 `/model` 的选择写进用户级 `~/.claude/settings.json`（2026-10-16 实测，
用真实 HOME 跑这一步改到了用户配置），而用户级配置只允许 diff / status。换模型的跟随已于当天实测一次。
用法：model-check.py <测试目录>   结果写 <测试目录>/model-report.json。
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


def usage_of(pane):
    terms, snap = ap.states()
    cid = (terms.get(pane) or {}).get("conversation_id")
    for u in snap.get("usage", []):
        if u["conversation_id"] == cid:
            return u["usage"]
    return None


def wait_usage(pane, pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        u = usage_of(pane)
        if pred(u):
            return u
        time.sleep(0.5)
    return usage_of(pane)


def screen(s):
    s.log.flush()
    t = re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)", b"", open(s.log.name, "rb").read())
    return " ".join(t.decode("utf-8", "replace").split())


def main():
    base = Path(sys.argv[1])
    ap.SOCK = str(base / "data/scyllauid.sock")
    panes = [t["terminal_id"] for t in json.loads((base / "fixture.json").read_text())]
    log_dir = base / "pty"
    log_dir.mkdir(exist_ok=True)
    report = {}
    sessions = [ap.Session("claude", panes[0], str(base / "home"), str(base / "work"), log_dir),
                ap.Session("kimi", panes[2], str(base / "home"), str(base / "work"), log_dir)]
    try:
        time.sleep(12)
        ap.accept_trust(sessions)
        time.sleep(2)
        for s in sessions:
            s.say("只回复 OK")
        for s in sessions:
            u = wait_usage(s.pane, lambda u: bool(u and u.get("model")), 60)
            report[s.tool] = {"first": u}
            print(f"  {s.tool}：{u}")
        for s in sessions:
            text = screen(s)
            report[s.tool]["screen_models"] = sorted(set(re.findall(
                r"(Opus [\d.]+|Sonnet [\d.]+|Haiku [\d.]+|K[\d.]+ ?\w*|kimi-[\w-]+)", text)))[:10]
            print(f"  {s.tool} 界面上出现的模型名：{report[s.tool]['screen_models']}")
    finally:
        for s in sessions:
            if s.running():
                s.exit()
        time.sleep(8)
        for s in sessions:
            s.kill()
    (base / "model-report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
