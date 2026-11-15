#!/usr/bin/env python3
"""新建终端链路计时：汇总测试实例的 App 日志、后台日志与进程启动时间，列出每个新建终端各段耗时。

用法：python3 -I src/scripts/launch-timing.py <测试数据目录> <起始时间，如 "2026-10-22 23:40:00"> [--app ScyllaUIBeta]
- App：统一日志中的 terminal new / open / show（微秒）。
- 后台：数据目录下 scyllauid.log 的「起 Shell」与「启动计时」（毫秒）。
- 进程：proc_pidinfo 取启动时间（微秒）：窗格一侧的 login 与 attach，后台一侧的 login、zsh 与 zsh 起的第一个命令。
终端须仍在运行（进程退出后取不到启动时间）。时间都相对「terminal new」，单位 ms。
"""

import ctypes
import json
import re
import subprocess
import sys
from datetime import datetime

PROC_PIDTBSDINFO = 3
libproc = ctypes.CDLL("/usr/lib/libproc.dylib")


def proc_start(pid):
    """进程启动时间（Unix 秒，微秒精度）；取不到为 None。struct proc_bsdinfo 中 pbi_start_tvsec 在偏移 120。"""
    buf = ctypes.create_string_buffer(136)
    if libproc.proc_pidinfo(pid, PROC_PIDTBSDINFO, ctypes.c_uint64(0), buf, 136) != 136:
        return None
    sec = int.from_bytes(buf.raw[120:128], "little")
    usec = int.from_bytes(buf.raw[128:136], "little")
    return sec + usec / 1e6


def processes():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        parts = line.split(None, 2) + [""]
        rows.append((int(parts[0]), int(parts[1]), parts[2]))
    return rows


def ts(text):
    return datetime.strptime(text, "%Y-%m-%d %H:%M:%S.%f").timestamp()


def daemon_lines(data_dir, since):
    spawn, trace = {}, {}
    pat = re.compile(r"^(\S+ \S+) scyllauid\[\d+\]: (起 Shell|启动计时)：terminal=(\S+?)[ ，](.*)$")
    with open(f"{data_dir}/scyllauid.log", encoding="utf-8", errors="replace") as f:
        for line in f:
            m = pat.match(line.rstrip("\n"))
            if not m or ts(m.group(1)) < since:
                continue
            when, kind, tid, rest = ts(m.group(1)), m.group(2), m.group(3), m.group(4)
            if kind == "起 Shell":
                spawn[tid] = (when, int(re.search(r"pid=(\d+)", rest).group(1)))
            else:
                fields = dict(re.findall(r"(\S+?)=(\S+)", rest))
                trace[tid] = {k: (None if v.startswith("-") else int(re.match(r"\d+", v).group())) for k, v in fields.items()
                              if k in ("首字节", "提示符", "命令开始", "命令首次输出", "画面稳定")}
    return spawn, trace


def app_lines(since, app):
    start = datetime.fromtimestamp(since).strftime("%Y-%m-%d %H:%M:%S")
    out = subprocess.run(["/usr/bin/log", "show", "--style", "ndjson", "--start", start, "--predicate",
                          'subsystem == "dev.scyllaui.app" AND eventMessage BEGINSWITH "terminal "'],
                         capture_output=True, text=True).stdout
    events = []
    for line in out.splitlines():
        try:
            e = json.loads(line)
        except ValueError:
            continue
        if not e.get("processImagePath", "").endswith(f"/{app}"):
            continue
        when = datetime.strptime(e["timestamp"][:26], "%Y-%m-%d %H:%M:%S.%f").timestamp()
        events.append((when, e["eventMessage"]))
    return events


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    data_dir, since = sys.argv[1], ts(sys.argv[2] + ".0")
    app = sys.argv[sys.argv.index("--app") + 1] if "--app" in sys.argv else "ScyllaUIBeta"
    spawn, trace = daemon_lines(data_dir, since)
    events = app_lines(since, app)
    procs = processes()
    kids = {}
    for pid, ppid, cmd in procs:
        kids.setdefault(ppid, []).append((pid, cmd))
    cols = ["tool", "open", "show", "窗格login", "attach", "起Shell", "后台login", "zsh", "提示符", "命令开始", "CLI进程",
            "CLI首次输出", "画面稳定"]
    print("\t".join(["terminal"] + cols))
    last_new = None
    for when, msg in events:
        if msg.startswith("terminal new "):
            last_new = (when, msg.split("tool=")[1])
            continue
        m = re.match(r"terminal open id=(\S+)", msg)
        if not m or last_new is None:
            continue
        tid, (t0, tool) = m.group(1), last_new
        last_new = None
        row = {"tool": tool, "open": when}
        row["show"] = next((w for w, s in events if w >= when and s.startswith(f"terminal show id={tid}")), None)
        attach = next(((p, pp) for p, pp, c in procs if f"--terminal {tid}" in c and "attach" in c), None)
        if attach:
            row["attach"], row["窗格login"] = proc_start(attach[0]), proc_start(attach[1])
        if tid in spawn:
            row["起Shell"], login = spawn[tid]
            row["后台login"] = proc_start(login)
            zsh = next((p for p, c in kids.get(login, [])), None)
            if zsh:
                row["zsh"] = proc_start(zsh)
                cli = next((p for p, c in kids.get(zsh, [])), None)
                row["CLI进程"] = proc_start(cli) if cli else None
            # 启动计时从 fork 前开始，「起 Shell」日志在 fork 之后约 1 ms 内写出（毫秒精度），以它为基准；
            # login 属 root，普通用户取不到它的启动时间。
            base = row["起Shell"]
            for key, name in (("提示符", "提示符"), ("命令开始", "命令开始"), ("CLI首次输出", "命令首次输出"), ("画面稳定", "画面稳定")):
                v = trace.get(tid, {}).get(name)
                row[key] = base + v / 1000 if base and v is not None else None
        fmt = lambda v: "-" if not isinstance(v, float) else f"{(v - t0) * 1000:.0f}"
        print("\t".join([tid[:8], tool] + [fmt(row.get(c)) for c in cols[1:]]))


if __name__ == "__main__":
    main()
