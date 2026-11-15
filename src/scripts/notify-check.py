#!/usr/bin/env python3
"""M2 第 1 块：系统通知与 Dock 角标实测（开发用，不进入产品运行时；docs/design.md 10.2 节第 1 块通过标准）。

在伪终端里运行四款真实 CLI 的交互界面（复用 acceptance-parallel.py 的 Session），挂到 App 以 SCYLLAUI_FIXTURE_TERMINALS
登记的夹具终端。运行前测试窗口须已隐藏（Cmd+H），屏幕不锁：此时 focus 为空，每次等待都应发通知。
每款先提问一次（Codex、Kimi 首次提交才有 Hook），再触发 6 次审批：前 4 次拒绝、第 5 次批准、第 6 次在等待中按
Ctrl+C 退出。Claude 每轮先 /clear 开新会话（同一会话里连续拒绝几次后 Claude 不再调用工具，2.1.301 实测）。
开始前用 NSRunningApplication.hide 隐藏测试 App（不是 Apple Event，不需要授权）。结束后读后台日志（通知与撤回）与测试 App 的统一日志（notify_latency、dock_badge）核对：
  ① 每次等待恰好一条通知，离开等待后撤回；② 通知延迟（Hook 触发 → 通知已提交）P95 / 最大；
  ③ 角标：每次进入、离开等待后，角标变为等待中的终端数所需时间；④ Claude 拒绝：Esc → Ready 用时。

用法：notify-check.py <测试目录> [工具,…]（默认四款）   测试目录结构同 acceptance-parallel.py（home/ work/ fixture.json data/）。
结果写 <测试目录>/notify-report.json。审批命令都没有副作用：联网只取响应头；Grok 删除临时 HOME 里不存在的文件。
"""
import importlib.util
import json
import os
import re
import signal
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("acceptance_parallel", HERE / "acceptance-parallel.py")
ap = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ap)

# 临时 HOME 里 Codex 没有信任记录，只在本次调用中跳过；关掉启动时的更新检查（回车会被当成确认去全局更新）。
ap.CMDS["codex"] = ["codex", "--no-daemon", "--dangerously-bypass-hook-trust", "-c", "check_for_update_on_startup=false"]

ASK = {
    "claude": "请用 Bash 工具执行这条命令，不要做别的：curl -sI https://example.com",
    # Codex 沙盒默认禁止联网，必须审批。
    "codex": "请执行这条 shell 命令，不要做别的：curl -sI https://example.com",
    "kimi": "请执行这条 shell 命令，不要做别的：curl -sI https://example.com",
    "grok": "请执行这条 shell 命令，不要做别的：rm -f ~/scyllaui-m2-nonexistent.txt",
}
ACTIONS = ["reject", "reject", "reject", "reject", "approve", "exit"]


def state(pane):
    terms, _ = ap.states()
    return ((terms.get(pane) or {}).get("agent") or {}).get("state", "absent")


def waiting_count():
    terms, _ = ap.states()
    return sum(1 for t in terms.values() if (t.get("agent") or {}).get("state") == "waiting")


def wait_for(pane, pred, timeout, step=0.05):
    end = time.time() + timeout
    while time.time() < end:
        st = state(pane)
        if pred(st):
            return time.time(), st
        time.sleep(step)
    return None, state(pane)


def act(s, action):
    if action == "reject":
        if s.tool == "grok":
            s.write(b"4")
            time.sleep(0.5)
            s.write(b"\r")
        else:
            s.write(b"\x1b")
    elif action == "approve":
        if s.tool == "grok":
            # Grok 1.0.52 的选项：1 always-approve、2 总是允许该命令、3 Yes, proceed、4 No, reject、5 Never allow。
            s.write(b"3")
            time.sleep(0.5)
        s.write(b"\r")
    else:
        for _ in range(2):
            if not s.running():
                break
            s.write(b"\x03")
            time.sleep(1.0)
        # Claude 2.1.301 的审批框里 Ctrl+C 不退出：5 秒后仍在等待就发 SIGHUP，相当于关闭终端。
        if s.running() and state(s.pane) == "waiting":
            time.sleep(4)
            if s.running() and state(s.pane) == "waiting":
                os.kill(s.pid, signal.SIGHUP)


def hide_app(pid):
    js = f'ObjC.import("AppKit"); var a = $.NSRunningApplication.runningApplicationWithProcessIdentifier({pid}); a.hide; a.hidden'
    return subprocess.run(["osascript", "-l", "JavaScript", "-e", js], capture_output=True, text=True).stdout.strip()


def app_log(start, pid):
    out = subprocess.run(["/usr/bin/log", "show", "--start", start, "--style", "compact", "--predicate",
                          f'subsystem == "dev.scyllaui.app" AND processIdentifier == {pid}'],
                         capture_output=True, text=True).stdout
    events = []
    for line in out.splitlines():
        m = re.match(r"(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3})", line)
        if m and ("notify_" in line or "dock_badge" in line or "hook_latency" in line):
            events.append((datetime.strptime(m.group(1), "%Y-%m-%d %H:%M:%S.%f").timestamp(), line))
    return events


def daemon_log(path, since):
    posts, withdrawn = [], []
    for line in Path(path).read_text(errors="replace").splitlines():
        m = re.match(r"(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3}) scyllauid\[\d+\]: (.*)", line)
        if not m:
            continue
        t = datetime.strptime(m.group(1), "%Y-%m-%d %H:%M:%S.%f").timestamp()
        if t < since:
            continue
        if m.group(2).startswith("通知："):
            posts.append((t, m.group(2)[3:]))
        elif m.group(2).startswith("撤回通知："):
            withdrawn += [(t, i) for i in m.group(2)[5:].split()]
    return posts, withdrawn


def pct(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))] if v else None


def main():
    base = Path(sys.argv[1])
    ap.SOCK = str(base / "data/scyllauid.sock")
    home, work = str(base / "home"), str(base / "work")
    panes = [t["terminal_id"] for t in json.loads((base / "fixture.json").read_text())]
    log_dir = base / "pty"
    log_dir.mkdir(exist_ok=True)
    ap.prepare_home(home)
    app_pid = subprocess.run(["pgrep", "-f", "Debug/ScyllaUIBeta.app/Contents/MacOS/ScyllaUIBeta"],
                             capture_output=True, text=True).stdout.split()[0]
    print(f"  隐藏测试 App：hidden={hide_app(app_pid)}")
    started = time.time()
    start_text = datetime.fromtimestamp(started).strftime("%Y-%m-%d %H:%M:%S")
    waits, checkpoints = [], []
    tools = sys.argv[2].split(",") if len(sys.argv) > 2 else ap.TOOLS
    sessions = [ap.Session(tool, panes[ap.TOOLS.index(tool)], home, work, log_dir) for tool in tools]
    try:
        time.sleep(15)
        for i, s in enumerate(sessions):
            if not s.running():
                print(f"  重启 {s.tool}：启动后已退出")
                sessions[i] = ap.Session(s.tool, s.pane, home, work, log_dir)
                time.sleep(15)
        ap.accept_trust(sessions)
        time.sleep(3)
        for s in sessions:
            s.say("只回复 OK")
        for s in sessions:
            wait_for(s.pane, lambda st: st == "ready", 90)
        for rnd, action in enumerate(ACTIONS, 1):
            for s in sessions:
                if not s.running():
                    continue
                if s.tool == "claude":
                    s.say("/clear")
                    time.sleep(3)
                # 每轮换一个文件名：同一会话里重复同一条命令时 Grok 有时不再执行（1.0.52 实测）。
                s.say(ASK[s.tool].replace("nonexistent.txt", f"nonexistent-{rnd}.txt"))
                entered, st = wait_for(s.pane, lambda st: st == "waiting", 90)
                rec = {"tool": s.tool, "pane": s.pane, "round": rnd, "action": action, "entered": entered}
                if entered is None:
                    rec["error"] = f"90 秒内没有进入等待（{st}）"
                    print(f"  ✗ 第 {rnd} 轮 {s.tool}：{rec['error']}")
                    waits.append(rec)
                    wait_for(s.pane, lambda st: st in ("ready", "error"), 60)
                    continue
                checkpoints.append({"at": entered, "count": waiting_count()})
                time.sleep(1.5)
                rec["acted"] = time.time()
                act(s, action)
                left, st = wait_for(s.pane, lambda st: st != "waiting", 20)
                rec["left"], rec["left_state"] = left, st
                if left:
                    checkpoints.append({"at": left, "count": waiting_count()})
                if action == "approve":
                    wait_for(s.pane, lambda st: st in ("ready", "error"), 60)
                if action == "exit":
                    time.sleep(3)
                    if s.running():
                        s.exit()
                ms = (left - rec["acted"]) * 1000 if left else None
                print(f"  {'✓' if left else '✗'} 第 {rnd} 轮 {s.tool:6} {action:7} → {st}"
                      + (f"（操作后 {ms:.0f} ms 离开等待）" if ms is not None else ""))
                waits.append(rec)
                time.sleep(2)
    finally:
        for s in sessions:
            if s.running():
                s.exit()
        time.sleep(10)
        for s in sessions:
            s.kill()
    time.sleep(2)

    posts, withdrawn = daemon_log(base / "data/scyllauid.log", started)
    events = app_log(start_text, app_pid)
    post_ids = [i for _, i in posts]
    withdrawn_ids = [i for _, i in withdrawn]
    ok_waits = [w for w in waits if w.get("entered")]
    by_pane = {}
    for w in ok_waits:
        by_pane[w["pane"]] = by_pane.get(w["pane"], 0) + 1
    per_pane_posts = {p: sum(1 for i in post_ids if i.startswith(p + ":")) for p in by_pane}
    dup = len(post_ids) - len(set(post_ids))
    left_waits = [w for w in ok_waits if w.get("left")]
    not_withdrawn = [i for i in post_ids if i not in withdrawn_ids]
    # 同期侧栏刷新的主线程耗时：App 被隐藏、App Nap 生效时明显升高（M1 锁屏时 P95 约 203 ms），用来佐证测试条件。
    ui = [int(m.group(1)) / 1000 for _, l in events if (m := re.search(r"hook_latency .*app_ui_us=(\d+)", l))]
    latency = [int(m.group(1)) / 1000 for _, l in events if (m := re.search(r"notify_latency .*total_us=(\d+)", l))]
    errors = [l for _, l in events if "notify_latency" in l and "error=-" not in l]
    badges = [(t, int(re.search(r"count=(\d+)", l).group(1))) for t, l in events if "dock_badge" in l]
    badge_delays, badge_bad = [], []
    for c in checkpoints:
        prior = [n for t, n in badges if t <= c["at"]]
        if prior and prior[-1] == c["count"]:
            badge_delays.append(0.0)
            continue
        hit = next((t for t, n in badges if t > c["at"] and n == c["count"]), None)
        if hit is None:
            badge_bad.append(c)
        else:
            badge_delays.append((hit - c["at"]) * 1000)
    rejects = [(w["left"] - w["acted"]) * 1000 for w in ok_waits
               if w["tool"] == "claude" and w["action"] == "reject" and w.get("left")]
    report = {
        "started": start_text, "app_pid": app_pid, "waits": waits, "checkpoints": checkpoints,
        "posts": post_ids, "withdrawn": withdrawn_ids,
        "summary": {
            "waits_entered": len(ok_waits), "waits_left": len(left_waits), "posts": len(post_ids),
            "per_pane_waits": by_pane, "per_pane_posts": per_pane_posts, "duplicate_posts": dup,
            "not_withdrawn": not_withdrawn, "post_errors": errors,
            "notify_latency_ms": {"n": len(latency), "p50": pct(latency, 50), "p95": pct(latency, 95), "max": max(latency, default=None)},
            "badge_delay_ms": {"n": len(badge_delays), "p95": pct(badge_delays, 95), "max": max(badge_delays, default=None),
                               "never": len(badge_bad)},
            "claude_reject_ms": rejects,
            "sidebar_main_thread_ms": {"n": len(ui), "p50": pct(ui, 50), "p95": pct(ui, 95)},
        },
    }
    (base / "notify-report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(report["summary"], ensure_ascii=False, indent=2))
    ok = (by_pane == per_pane_posts and dup == 0 and not not_withdrawn and not errors and not badge_bad
          and len(left_waits) == len(ok_waits))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
