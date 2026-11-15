#!/usr/bin/env python3
"""侧栏测试夹具（M1 第 3 块）：在独立的测试数据目录里造多项目、多终端、大量最近任务与推送风暴。

只用于测试，不进入产品运行时。连接的是 App 以 SCYLLAUI_DATA_DIR 启动后拉起的测试后台，拒绝连接真实后台。
Hook 载荷取自 tests/hook-samples/ 中四款 CLI 的真实版本样本，只替换会话 ID、目录、提示词与轮次 ID。

用法（目录与参数见 README「侧栏测试数据」）：
  sidebar-fixture.py terminals --out output/sidebar-test/fixture.json --projects 12 [--git]
  sidebar-fixture.py seed  --sock output/sidebar-test/scyllauid.sock --fixture output/sidebar-test/fixture.json --conversations 600
  sidebar-fixture.py storm --sock output/sidebar-test/scyllauid.sock --fixture output/sidebar-test/fixture.json --rate 240 --seconds 30
  sidebar-fixture.py resume-cases --sock output/sidebar-test/scyllauid.sock [--terminal <终端 ID>]
  /usr/bin/log show --start <时间> --predicate 'subsystem == "dev.scyllaui.app"' --style compact > log.txt
  sidebar-fixture.py stats log.txt
"""

import argparse
import copy
import json
import random
import re
import socket
import sys
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SAMPLES = ROOT / "tests" / "hook-samples"
REAL_SOCK = Path.home() / "Library" / "Application Support" / "ScyllaUI" / "scyllauid.sock"
PROTOCOL_VERSION = 3
TOOLS = ["claude", "codex", "kimi", "grok"]
PROJECT_NAMES = [
    "api-gateway", "web-console", "billing", "search-index", "mobile-app", "data-pipeline",
    "auth-service", "design-system", "docs-site", "infra", "ml-ranking", "notifications",
    "reporting", "edge-cache", "cli-tools", "playground",
]


def now_us():
    return time.time_ns() // 1000


def templates():
    """每款工具每个原生事件取一份载荷模板，较新的版本优先。"""
    out = {}
    for f in sorted(SAMPLES.glob("*.jsonl"), reverse=True):
        for line in f.read_text(encoding="utf-8").splitlines():
            row = json.loads(line)
            if row.get("dropped"):
                continue
            p = row["payload"]
            ev = p.get("hook_event_name") or p.get("hookEventName")
            # 后台会话（transcript_path 显式为 null）与子代理事件不作模板。
            if "transcript_path" in p and p["transcript_path"] is None:
                continue
            if p.get("agent_id") not in (None, "main") or p.get("subagentType"):
                continue
            out.setdefault((row["source"], ev), p)
    return out


def fill(tpl, sid, cwd, prompt=None, turn=None, title=None):
    p = copy.deepcopy(tpl)
    for k in ("session_id", "sessionId"):
        if k in p:
            p[k] = sid
    for k in ("cwd", "workspaceRoot"):
        if k in p:
            p[k] = cwd
    for k in ("transcript_path", "transcriptPath"):
        if k in p:
            p[k] = f"/tmp/scyllaui-fixture/{sid}.jsonl"
    if prompt is not None and "prompt" in p:
        p["prompt"] = prompt
    if turn is not None:
        for k in ("prompt_id", "promptId", "turn_id"):
            if k in p:
                p[k] = turn
    if "session_title" in p:
        p["session_title"] = title
    return p


class Hook:
    """以 hook 角色连接测试后台，一条连接发多帧（scyllauid 的 hook 连接按行读取）。"""

    def __init__(self, sock_path):
        real = REAL_SOCK.resolve() if REAL_SOCK.exists() else REAL_SOCK
        if Path(sock_path).resolve() == real:
            sys.exit("拒绝连接真实后台的 socket；请用 SCYLLAUI_DATA_DIR 下的测试后台")
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(str(sock_path))
        self.send({"type": "hello", "role": "hook", "protocol_version": PROTOCOL_VERSION, "build_id": "fixture"})
        self.frames = 0

    def send(self, obj):
        self.s.sendall((json.dumps(obj, ensure_ascii=False) + "\n").encode())

    def hook(self, terminal, tool, payload):
        t = now_us()
        self.send({"type": "hook", "config": tool, "source": tool, "dropped": False, "pane_id": terminal,
                   "hook_t0_us": t, "hook_send_us": t, "payload": payload, "env": {}})
        self.frames += 1

    def close(self):
        self.s.close()


def cli_snapshot(sock_path):
    """以 cli 角色取测试后台的快照（只读）。"""
    real = REAL_SOCK.resolve() if REAL_SOCK.exists() else REAL_SOCK
    if Path(sock_path).resolve() == real:
        sys.exit("拒绝连接真实后台的 socket；请用 SCYLLAUI_DATA_DIR 下的测试后台")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(str(sock_path))
    f = s.makefile("rb")
    for msg in ({"type": "hello", "role": "cli", "protocol_version": PROTOCOL_VERSION, "build_id": "fixture"},
                {"type": "snapshot.get", "id": 1}):
        s.sendall((json.dumps(msg) + "\n").encode())
    for line in f:
        m = json.loads(line)
        if m.get("type") == "reply":
            s.close()
            return m["snapshot"]
    sys.exit("测试后台没有应答")


class Pacer:
    def __init__(self, rate):
        self.interval = 1.0 / rate if rate > 0 else 0
        self.next = time.monotonic()

    def wait(self):
        if self.interval:
            self.next += self.interval
            d = self.next - time.monotonic()
            if d > 0:
                time.sleep(d)


def cmd_terminals(a):
    rng = random.Random(a.seed)
    lo, hi = (int(x) for x in a.per_project.split("-"))
    branches = ["main", "feature/sidebar-tree", "fix/hook-latency", "release/0.1", None, "4f1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c"]
    out = []
    for i in range(a.projects):
        name = PROJECT_NAMES[i % len(PROJECT_NAMES)] + ("" if i < len(PROJECT_NAMES) else f"-{i}")
        d = f"{a.root}/{name}"
        if a.git:
            # 只造一个 .git/HEAD（scyllauid 只读它）；None 表示不是仓库，40 位十六进制为 detached。
            b = branches[i % len(branches)]
            Path(d).mkdir(parents=True, exist_ok=True)
            if b is not None:
                (Path(d) / ".git").mkdir(exist_ok=True)
                head = b if len(b) == 40 else f"ref: refs/heads/{b}"
                (Path(d) / ".git" / "HEAD").write_text(head + "\n", encoding="utf-8")
        for _ in range(rng.randint(lo, hi)):
            out.append({"terminal_id": f"fx-{uuid.uuid4()}", "project_dir": d, "cwd": d})
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.out).write_text(json.dumps(out, indent=1, ensure_ascii=False), encoding="utf-8")
    print(f"夹具终端 {len(out)} 个，项目 {a.projects} 个 → {a.out}")


RESUME_CASES = {
    "gone": "会话记录已不存在",
    "dir": "目录已不存在",
    "unknown": "CLI 不认识的会话 ID",
}


def cmd_resume_cases(a):
    """一键恢复的失败场景（M1 第 6 块）：在测试后台给四款各造三条已结束的假对话，会话 ID 为 UUID 形（Kimi 为
    session_<UUID>）。gone：记录文件不存在（Kimi 没有记录路径，不造）；dir：启动目录不存在；unknown：记录文件存在
    （空文件）但 CLI 不认识该 ID。Hook 发给测试窗口里一个已有的终端，对话随即进入「最近任务」。"""
    tpl = templates()
    terms = [t["terminal_id"] for t in cli_snapshot(a.sock)["terminals"]]
    term = a.terminal or (terms[0] if terms else None)
    if term is None:
        sys.exit("测试后台没有存活终端：先在测试窗口里开一个终端")
    root = Path(a.root)
    work = root / "work"
    work.mkdir(parents=True, exist_ok=True)
    h = Hook(a.sock)
    for tool in TOOLS:
        for case, name in RESUME_CASES.items():
            if case == "gone" and tool == "kimi":
                continue
            sid = ("session_" if tool == "kimi" else "") + str(uuid.uuid4())
            cwd = str(root / f"no-such-dir-{tool}") if case == "dir" else str(work)
            transcript = root / (f"missing-{sid}.jsonl" if case == "gone" else f"{sid}.jsonl")
            if case != "gone":
                transcript.touch()
            prompt = f"恢复测试：{tool} {name}"
            turn = uuid.uuid4().hex
            for ev in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"]:
                if (tool, ev) not in tpl:
                    continue
                p = fill(tpl[(tool, ev)], sid, cwd, prompt, turn, prompt)
                for k in ("transcript_path", "transcriptPath"):
                    if k in p:
                        p[k] = str(transcript)
                h.hook(term, tool, p)
            print(f"{tool:6} {case:7} {sid}  {prompt}")
    h.close()


def load_fixture(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def cmd_seed(a):
    tpl = templates()
    terms = load_fixture(a.fixture)
    h = Hook(a.sock)
    pace = Pacer(a.rate)
    start = time.monotonic()
    for i in range(a.conversations):
        t = terms[i % len(terms)]
        tool = TOOLS[i % len(TOOLS)]
        sid = f"fx{i:05d}-{uuid.uuid4().hex[:12]}"
        turn = uuid.uuid4().hex
        prompt = f"夹具任务 #{i}：检查 {Path(t['cwd']).name} 的构建与测试"
        for ev in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"]:
            if (tool, ev) not in tpl:
                continue
            h.hook(t["terminal_id"], tool, fill(tpl[(tool, ev)], sid, t["cwd"], prompt, turn, f"任务 {i}"))
            pace.wait()
    h.close()
    dt = time.monotonic() - start
    print(f"seed：对话 {a.conversations} 条，帧 {h.frames}，{dt:.1f} s（{h.frames / dt:.0f} 帧/s）")


def cmd_storm(a):
    """每个夹具终端一个小状态机：开会话 → 提交 → [待审批 → 继续] → Stop，约一成概率结束会话。"""
    tpl = templates()
    terms = load_fixture(a.fixture)
    rng = random.Random(a.seed)
    h = Hook(a.sock)
    pace = Pacer(a.rate)
    state = {}
    for i, t in enumerate(terms):
        state[t["terminal_id"]] = {"tool": TOOLS[i % len(TOOLS)], "sid": None, "queue": [], "n": 0}

    def cycle(tool):
        evs = ["UserPromptSubmit"]
        if (tool, "PermissionRequest") in tpl and rng.random() < 0.4:
            evs.append("PermissionRequest")
            evs.append("PermissionResult" if (tool, "PermissionResult") in tpl else "PostToolUse")
        evs.append("Stop")
        return [e for e in evs if (tool, e) in tpl]

    end = time.monotonic() + a.seconds
    while time.monotonic() < end:
        t = rng.choice(terms)
        s = state[t["terminal_id"]]
        tool = s["tool"]
        if s["sid"] is None:
            s["sid"] = f"st-{uuid.uuid4().hex[:16]}"
            s["queue"] = ["SessionStart"]
        if not s["queue"]:
            if rng.random() < 0.1 and (tool, "SessionEnd") in tpl:
                s["queue"] = ["SessionEnd"]
            else:
                s["turn"] = uuid.uuid4().hex
                s["queue"] = cycle(tool)
        ev = s["queue"].pop(0)
        s["n"] += 1
        prompt = f"风暴第 {s['n']} 轮：{Path(t['cwd']).name}"
        h.hook(t["terminal_id"], tool, fill(tpl[(tool, ev)], s["sid"], t["cwd"], prompt, s.get("turn"), prompt))
        if ev == "SessionEnd":
            s["sid"] = None
        pace.wait()
    h.close()
    print(f"storm：帧 {h.frames}，{a.seconds} s（{h.frames / a.seconds:.0f} 帧/s），终端 {len(terms)} 个")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))] if xs else 0


def cmd_stats(a):
    """从统一日志统计侧栏主线程耗时与「Hook → 侧栏」延迟（单位 µs）。"""
    series = {}
    checks = {"ok": 0, "mismatch": 0}
    for line in Path(a.log).read_text(encoding="utf-8", errors="replace").splitlines():
        if "sidebar_apply " in line:
            for k in ("msgs", "ops", "rows", "model_us", "apply_us", "display_us"):
                m = re.search(rf"\b{k}=(\d+)", line)
                if m:
                    series.setdefault(f"sidebar_apply.{k}", []).append(int(m.group(1)))
        elif "hook_latency " in line:
            for k in ("total_us", "hook_to_daemon_us", "daemon_to_app_us", "app_ui_us"):
                m = re.search(rf"\b{k}=(\d+)", line)
                if m:
                    series.setdefault(f"hook_latency.{k}", []).append(int(m.group(1)))
        elif "sidebar_check ok" in line:
            checks["ok"] += 1
            m = re.search(r"\bus=(\d+)", line)
            if m:
                series.setdefault("sidebar_check.us", []).append(int(m.group(1)))
        elif "sidebar_check mismatch" in line:
            checks["mismatch"] += 1
            print(line)
    print(f"{'指标':32} {'样本':>6} {'P50':>8} {'P95':>8} {'最大':>8}")
    for k, xs in series.items():
        print(f"{k:32} {len(xs):>6} {pct(xs, .5):>8} {pct(xs, .95):>8} {max(xs):>8}")
    print(f"sidebar_check ok={checks['ok']} mismatch={checks['mismatch']}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("terminals", help="生成夹具终端列表（App 以 SCYLLAUI_FIXTURE_TERMINALS 读取）")
    p.add_argument("--out", required=True)
    p.add_argument("--projects", type=int, default=12)
    p.add_argument("--per-project", default="1-3")
    p.add_argument("--root", default="/tmp/scyllaui-fixture")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--git", action="store_true", help="在项目目录下造 .git/HEAD（多种分支、detached、非仓库）")
    p = sub.add_parser("seed", help="批量生成已结束的对话（最近任务）")
    p.add_argument("--sock", required=True)
    p.add_argument("--fixture", required=True)
    p.add_argument("--conversations", type=int, default=600)
    p.add_argument("--rate", type=float, default=500, help="帧/秒，0 为不限速")
    p = sub.add_parser("storm", help="多终端随机推进状态（推送风暴）")
    p.add_argument("--sock", required=True)
    p.add_argument("--fixture", required=True)
    p.add_argument("--rate", type=float, default=240)
    p.add_argument("--seconds", type=float, default=30)
    p.add_argument("--seed", type=int, default=7)
    p = sub.add_parser("stats", help="统计 /usr/bin/log show 输出中的侧栏耗时与延迟")
    p.add_argument("log")
    p = sub.add_parser("resume-cases", help="一键恢复的失败场景：四款各造几条无法恢复的已结束对话")
    p.add_argument("--sock", required=True)
    p.add_argument("--terminal", help="发 Hook 的终端（默认取测试后台的第一个存活终端）")
    p.add_argument("--root", default="/tmp/scyllaui-fixture/resume")
    a = ap.parse_args()
    {"terminals": cmd_terminals, "seed": cmd_seed, "storm": cmd_storm, "stats": cmd_stats,
     "resume-cases": cmd_resume_cases}[a.cmd](a)


if __name__ == "__main__":
    main()
