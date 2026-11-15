#!/usr/bin/env python3
"""小型稳定性测试（M4b 块 9，开发用，不进入产品运行时）：真 CLI 在后台持有的终端里连续跑一段时间。

用法：SCYLLAUI_ACCEPT_REAL_HOME=claude,kimi,codex caffeinate -i python3 -I src/scripts/stability-run.py <测试目录> [分钟，默认 60]
      （临时 HOME 里没有 Codex 登录，Codex 与 Claude、Kimi 一样用真实 HOME；先 --make-fixture，再以生成的夹具启动测试 App）
测试目录与 acceptance-parallel.py --held 相同（home/、work/、opencode-config/、data/scyllauid.sock），测试 App 以
<测试目录>/fixture-held.json 启动（本脚本 --make-fixture 生成：每个会话一个终端，用真实 HOME 的工具登记在项目目录）。
会话：六款各 1 个，另加 OpenCode、Copilot 各 1 个（块 9 新接的两款各 2 个）；SCYLLAUI_STAB_SESSIONS（逗号分隔）与
SCYLLAUI_STAB_NEW 可改，如块 9b：claude,codex,kimi,grok,copilot,antigravity,antigravity 与 antigravity。每 5 分钟一轮：
  - 新两款每轮都问；现有四款隔轮问（同样的最短提示词）；
  - 每 3 轮新两款请求一次需审批的无害命令，等到「需确认」后轮流同意与拒绝；每 4 轮新两款问一个长回答、几秒后按 Esc 中断；
  - 每轮结束等 60 秒，按 docs/design.md 4.3 节推出的期望状态逐个比对（需确认时比「waiting」）；
  - 每分钟采样测试版 scyllauid 与 App 的内存（RSS）与 CPU、两份日志新增的出错行、新的崩溃报告。
结果写 <测试目录>/stability.json，打印每轮比对。output/longrun/stop-stability 存在时提前结束。
锁屏时 App 主线程被节流，「Hook → 侧栏」延迟不算数，只记录。
"""
import importlib.util
import json
import os
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("acc", ROOT / "src/scripts/acceptance-parallel.py")
acc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(acc)

SESSIONS = os.environ.get("SCYLLAUI_STAB_SESSIONS", "claude,codex,kimi,grok,opencode,copilot,opencode,copilot").split(",")
NEW = tuple(os.environ.get("SCYLLAUI_STAB_NEW", "opencode,copilot").split(","))
PROMPTS = ["只回复 OK", "2+3 等于几？只回答数字", "说一种原色，只用一个词", "只回复 ready", "7 是质数吗？只回答是或否"]
ASK = {"opencode": "请用 bash 执行这条命令，不要做别的：touch scyllaui-stab-{n}.txt",
       "copilot": "请用 shell 执行这条命令，不要做别的：touch scyllaui-stab-{n}.txt",
       "antigravity": "请执行这条 shell 命令，不要做别的：touch scyllaui-stab-{n}.txt"}
LONG = "从 1 数到 300，每行一个数字，不要用工具"
STOP = ROOT / "output/longrun/stop-stability"
CRASH = Path.home() / "Library/Logs/DiagnosticReports"
# 区分大小写：日志里正常的 `error=-`（notify_latency）与 `MessageAbortedError`（OpenCode 中断）不算出错。
ERROR_WORDS = re.compile(r"\bERROR\b|panic|错误|失败|[Ff]atal")


def make_fixture(base):
    fixture = json.loads((base / "fixture.json").read_text())[: len(SESSIONS)]
    for t, tool in zip(fixture, SESSIONS):
        t["cwd"] = acc.session_env(tool, t["terminal_id"], str(base / "home"), str(base / "work"))[0]
    out = base / "fixture-held.json"
    out.write_text(json.dumps(fixture, indent=2) + "\n")
    return out


def proc_stats(pid):
    ps = subprocess.run(["ps", "-o", "rss=,%cpu=,ppid=", "-p", str(pid)], capture_output=True, text=True).stdout.split()
    return {"pid": int(pid), "rss_mb": round(int(ps[0]) / 1024, 1), "cpu": float(ps[1]), "ppid": int(ps[2])} if len(ps) == 3 else None


def daemon_and_app(data):
    """本测试目录的 scyllauid 与连着它的测试 App（后台的父进程；同时开着别的测试实例时不会认错）。"""
    out = subprocess.run(["pgrep", "-f", f"scyllauid --data-dir {data}"], capture_output=True, text=True).stdout.split()
    d = proc_stats(out[0]) if out else None
    a = proc_stats(d["ppid"]) if d and d["ppid"] > 1 else None
    return d, a


class Sampler(threading.Thread):
    def __init__(self, base, report):
        super().__init__(daemon=True)
        self.base, self.report, self.stop = base, report, False
        self.logs = {p: p.stat().st_size for p in (base / "data").glob("*.log")}
        self.crash = {p.name for p in CRASH.glob("*")}

    def run(self):
        data = str(self.base / "data")
        while not self.stop:
            d, a = daemon_and_app(data)
            row = {"at": time.strftime("%H:%M:%S"), "daemon": d, "app": a}
            for p in (self.base / "data").glob("*.log"):
                start = self.logs.get(p, 0)
                with open(p, "rb") as f:
                    f.seek(start)
                    new = f.read()
                self.logs[p] = start + len(new)
                for line in new.decode("utf-8", "replace").splitlines():
                    if ERROR_WORDS.search(line):
                        self.report["errors"].append(line[:300])
                    m = re.search(r"hook_latency .*tool=(\w+) .*total_us=(\d+)", line)
                    if m:
                        self.report["latency"].append({"tool": m.group(1), "ms": int(m.group(2)) / 1000})
            for p in CRASH.glob("*"):
                if p.name not in self.crash and re.match(r"(ScyllaUI|scyllauid)", p.name):
                    self.report["crashes"].append(p.name)
                self.crash.add(p.name)
            self.report["samples"].append(row)
            for _ in range(60):
                if self.stop:
                    break
                time.sleep(1)


def interrupt(s):
    s.write(b"\x1b")
    if s.tool != "antigravity":   # Antigravity 按一次即中断（2026-11-01 实测 1.4.12）
        time.sleep(0.2)
        s.write(b"\x1b")


def approve(s):
    s.write(b"\r")   # 默认选项都是「同意一次」（OpenCode Allow once、Copilot Yes、Antigravity Yes, run command）


def deny(s):
    if s.tool == "opencode":
        for b in (b"\x1b[C", b"\x1b[C", b"\r"):
            s.write(b)
            time.sleep(0.3)
    else:
        s.write(b"\x1b")


def main():
    base = Path(sys.argv[1])
    if "--make-fixture" in sys.argv[2:]:
        print(make_fixture(base))
        return 0
    minutes = float(sys.argv[2]) if len(sys.argv) > 2 else 60
    acc.SOCK = str(base / "data/scyllauid.sock")
    home, work = str(base / "home"), str(base / "work")
    log_dir = base / "pty-stab"
    log_dir.mkdir(exist_ok=True)
    acc.prepare_home(home)
    acc.prepare_opencode(base)
    panes = [t["terminal_id"] for t in json.loads((base / "fixture-held.json").read_text())]
    report = {"started": time.strftime("%Y-%m-%d %H:%M:%S"), "minutes": minutes, "rounds": [], "samples": [],
              "errors": [], "crashes": [], "latency": [], "restarts": []}
    sampler = Sampler(base, report)
    sampler.start()
    sessions = []
    end = time.time() + minutes * 60
    try:
        for tool, pane in zip(SESSIONS, panes):
            sessions.append(acc.HeldSession(tool, pane, home, work, log_dir))
            time.sleep(1)
        time.sleep(15)
        acc.accept_trust(sessions)
        time.sleep(3)
        n = 0
        while time.time() < end - 90 and not STOP.exists():
            n += 1
            t0 = time.time()
            new = [s for s in sessions if s.tool in NEW]
            approvers = new if n % 3 == 0 else []
            interrupters = new if n % 4 == 0 and not approvers else []
            askers = [s for s in sessions if (s.tool in NEW or n % 2 == 1) and s not in approvers + interrupters]
            for s in sessions:
                if s.exited:
                    report["restarts"].append({"round": n, "tool": s.tool, "pane": s.pane[:11], "what": "CLI 已退出"})
            live = [s for s in sessions if not s.exited]
            for s in askers:
                if s in live:
                    s.say(PROMPTS[n % len(PROMPTS)])
                    time.sleep(0.2)
            waits = {}
            if approvers:
                for s in approvers:
                    s.say(ASK[s.tool].format(n=n))
                time.sleep(40)
                terms, _ = acc.states()
                waits = {s.pane: (terms.get(s.pane) or {}).get("agent", {}).get("state") for s in approvers}
                for i, s in enumerate(approvers):
                    (approve if (n // 3 + i) % 2 == 0 else deny)(s)
            for s in interrupters:
                s.say(LONG)
            if interrupters:
                time.sleep(6)
                for s in interrupters:
                    interrupt(s)
            time.sleep(60)
            terms, snap = acc.states()
            rows, bad = [], 0
            for s in live:
                got = (terms.get(s.pane) or {}).get("agent", {}).get("state", "absent")
                want = {"ready", "error"} if s in approvers + interrupters else {"ready"}
                ok = got in want and (s not in approvers or waits.get(s.pane) == "waiting")
                bad += not ok
                rows.append({"tool": s.tool, "pane": s.pane[:11], "state": got, "waited": waits.get(s.pane), "ok": ok})
            kinds = "审批" if approvers else ("中断" if interrupters else "提问")
            report["rounds"].append({"round": n, "at": time.strftime("%H:%M:%S"), "kind": kinds, "mismatch": bad, "rows": rows,
                                     "recent_total": snap.get("recent_total")})
            print(f"== 第 {n} 轮（{kinds}） {time.strftime('%H:%M:%S')}  不一致 {bad} / {len(rows)}", flush=True)
            for r in rows:
                if not r["ok"]:
                    print(f"  ✗ {r['tool']:8} {r['pane']}  {r['state']}  需确认时 {r['waited']}", flush=True)
            (base / "stability.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            rest = 300 - (time.time() - t0)
            while rest > 0 and time.time() < end - 90 and not STOP.exists():
                time.sleep(min(5, rest))
                rest -= 5
    finally:
        for s in sessions:
            if not s.exited:
                s.exit()
        time.sleep(15)
        for s in sessions:
            s.kill()
        sampler.stop = True
        sampler.join(timeout=70)
        report["finished"] = time.strftime("%Y-%m-%d %H:%M:%S")
        rss = {k: [r[k]["rss_mb"] for r in report["samples"] if r.get(k)] for k in ("daemon", "app")}
        cpu = {k: [r[k]["cpu"] for r in report["samples"] if r.get(k)] for k in ("daemon", "app")}
        lat = sorted(x["ms"] for x in report["latency"])
        report["summary"] = {
            "rounds": len(report["rounds"]), "mismatch": sum(r["mismatch"] for r in report["rounds"]),
            "rss_mb": {k: {"first": v[0], "last": v[-1], "max": max(v)} for k, v in rss.items() if v},
            "cpu_median": {k: statistics.median(v) for k, v in cpu.items() if v},
            "pids_changed": {k: len({r[k]["pid"] for r in report["samples"] if r.get(k)}) > 1 for k in ("daemon", "app")},
            "errors": len(report["errors"]), "crashes": len(report["crashes"]),
            "latency_ms": {"n": len(lat), "p50": lat[len(lat) // 2], "p95": lat[int(len(lat) * 0.95)], "max": lat[-1]} if lat else None,
        }
        (base / "stability.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(report["summary"], ensure_ascii=False, indent=2))
    return 0 if report["summary"]["mismatch"] == 0 and not report["crashes"] else 1


if __name__ == "__main__":
    sys.exit(main())
