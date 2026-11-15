#!/usr/bin/env python3
"""24 小时长跑：回放 Hook 的假 AI（开发用，docs/design.md 10.6 节「24 小时长跑方案」）。

在 ScyllaUI 的终端里前台运行：python3 -I <本文件> <claude|codex|kimi|grok|opencode|copilot|antigravity>
取 tests/hook-samples/ 里该工具最新的冒烟样本，按真实顺序经本终端环境里的 $SCYLLAUI_BIN hook --config <工具>
（与真 CLI 的 Hook 同一条通路；OpenCode 的插件调的也是它）送进后台：开场事件，之后每 2—5 分钟一轮
「提交 → 等几秒 → 结束」，每 10 轮换一个新会话。各款的事件名与会话字段见 TOOLS（第二轮长跑加入新三款，
docs/design.md 10.6 节「第二轮长跑方案」）。会话 ID 改成 longrun-fake-…，工作目录改为当前目录，
去掉记录文件与产物目录路径（不读任何会话文件）。不花额度；每轮打印一行到终端。
output/longrun/stop-fake 或 stop-all 存在时退出。
"""
import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SAMPLES = ROOT / "tests/hook-samples"
OUT = ROOT / "output/longrun"
# 轮间隔（秒）。SCYLLAUI_FAKE_GAP=低,高 可改短，用于在测试版里快速积累延迟样本（2026-11-15 验证侧栏尖峰）。
GAP = tuple(float(x) for x in os.environ.get("SCYLLAUI_FAKE_GAP", "120,300").split(","))

# 每款：开场事件（None 表示没有，第一轮的提交即开始）、每轮事件（按顺序，样本里有的才发）、结束事件、会话字段。
FIRST_FOUR = ("SessionStart", ("UserPromptSubmit", "TurnStarted", "Stop"), "SessionEnd", ("session_id", "sessionId"))
TOOLS = {
    "claude": FIRST_FOUR,
    "codex": FIRST_FOUR,
    "kimi": FIRST_FOUR,
    "grok": FIRST_FOUR,
    "opencode": (None, ("session.prompt", "session.execution.started", "session.execution.succeeded"), None, ("sessionID",)),
    "copilot": ("sessionStart", ("userPromptSubmitted", "agentStop"), "sessionEnd", ("sessionId",)),
    "antigravity": ("SessionStart", ("PreInvocation", "Stop"), None, ("conversationId",)),
}
DROP = ("transcript_path", "transcriptPath", "artifactDirectoryPath")
PROMPT_KEYS = ("prompt", "user_prompt", "initialPrompt", "lastUserInput")


def sid_of(e, keys):
    return next((e[k] for k in keys if e.get(k)), None)


def sample_events(tool):
    keys = TOOLS[tool][3]
    files = sorted(SAMPLES.glob(f"{tool}-*-smoke.jsonl"), key=lambda p: [int(x) for x in p.name.split("-")[1].split(".")])
    lines = [json.loads(l) for l in files[-1].read_text(encoding="utf-8").splitlines() if l.strip()]
    events = [l["payload"] for l in lines if not l.get("dropped") and isinstance(l.get("payload"), dict)]
    sid = next(s for s in (sid_of(e, keys) for e in events) if s)
    mine = [e for e in events if sid_of(e, keys) == sid]
    return files[-1].name, sid, mine


def rewrite(obj, old_sid, new_sid, n):
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if k in DROP:
                continue
            if k in ("cwd", "directory"):
                v = os.getcwd()
            elif k == "workspacePaths":
                v = [os.getcwd()]
            elif k in PROMPT_KEYS:
                v = f"longrun fake prompt {n}"
            elif k == "timestamp" and isinstance(v, int):
                v = int(time.time() * 1000)
            out[k] = rewrite(v, old_sid, new_sid, n)
        return out
    if isinstance(obj, list):
        return [rewrite(v, old_sid, new_sid, n) for v in obj]
    return new_sid if obj == old_sid else obj


def send(tool, payload):
    r = subprocess.run([os.environ["SCYLLAUI_BIN"], "hook", "--config", tool], input=json.dumps(payload).encode(),
                       capture_output=True, timeout=10)
    return r.returncode


def main(tool):
    if not (os.environ.get("SCYLLAUI_SOCK") and os.environ.get("SCYLLAUI_BIN")):
        print("不在 ScyllaUI 的终端里（没有 SCYLLAUI_SOCK / SCYLLAUI_BIN），退出")
        return 1
    start, turn_names, end, _ = TOOLS[tool]
    name, old_sid, events = sample_events(tool)
    by_name = {}
    for e in events:
        by_name.setdefault(e.get("hook_event_name"), e)
    turn = [k for k in turn_names if k in by_name]
    if not turn or (start and start not in by_name):
        print(f"样本 {name} 里缺少需要的事件（开场 {start}，每轮 {turn_names}），退出")
        return 1
    print(f"假 AI：{tool}，样本 {name}，每轮事件 {turn}")
    session, n = 0, 0
    while not ((OUT / "stop-fake").exists() or (OUT / "stop-all").exists()):
        if n % 10 == 0:
            if n and end in by_name:
                send(tool, rewrite(by_name[end], old_sid, sid, n))
            session += 1
            sid = f"longrun-fake-{tool}-{os.getpid()}-{session}"
            if start:
                send(tool, rewrite(by_name[start], old_sid, sid, n))
        n += 1
        for i, k in enumerate(turn):
            if i:
                # 最后一个是「结束」事件：模拟思考几秒；中间的事件间隔 1 秒。
                time.sleep(random.uniform(3, 20) if i == len(turn) - 1 else 1)
            code = send(tool, rewrite(by_name[k], old_sid, sid, n))
            if code:
                print(time.strftime("%H:%M:%S"), f"hook {k} 退出码 {code}")
        print(time.strftime("%H:%M:%S"), f"第 {n} 轮完成（会话 {sid}）")
        time.sleep(random.uniform(*GAP))
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in TOOLS:
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
