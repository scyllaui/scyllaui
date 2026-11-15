#!/usr/bin/env python3
"""24 小时长跑：在本终端的后台按节奏往前台程序（真 CLI）输入提问（开发用，docs/design.md 10.6 节「24 小时长跑方案」）。

用法（在 ScyllaUI 的终端里，先起本脚本到后台，再在前台运行 CLI）：
  python3 -I <本文件> <工具>[:long] <间隔分钟> [首次等待秒数] &! ; <CLI 命令>
工具名后加 :long 改用只读的多步任务提问（第二轮长跑，docs/design.md 10.6 节「第二轮长跑方案」），停止文件仍按工具名。
原理：用 TIOCSTI 往自己的控制终端注入按键，只有前台程序能读到（2026-10-28 在 macOS 27.0.1 实测）；不打印任何东西到终端，
记录写 output/longrun/inject-<工具>-<进程号>.log。output/longrun/stop-<工具> 或 stop-all 存在时退出。
"""
import fcntl
import os
import random
import signal
import sys
import termios
import time
from pathlib import Path

OUT = Path(__file__).resolve().parents[3] / "output/longrun"
# 最短、无害、不会让 CLI 执行命令的问题，轮换使用。
PROMPTS = [
    "Reply with just OK.",
    "What is 2+3? Answer with the number only.",
    "Name one primary color. One word only.",
    "Reply with the single word: ready",
    "Is 7 a prime number? Answer yes or no only.",
]
# 只读的多步任务：要读文件、列目录（会调用工具），但明确不改任何文件；最后一句给有子代理的 CLI（OpenCode）。
LONG_PROMPTS = [
    "List the files in the current directory, then summarize README.md in one sentence if it exists. Do not modify any files.",
    "Count the directories under the current directory up to depth 3 and report the number. Do not modify any files.",
    "Use a subagent to read the files in the current directory and report the name of the largest one. Do not modify any files.",
]


def log(path, text):
    with open(path, "a", encoding="utf-8") as f:
        f.write(time.strftime("%Y-%m-%d %H:%M:%S ") + text + "\n")


def stopped(tool):
    return (OUT / f"stop-{tool}").exists() or (OUT / "stop-all").exists()


def type_line(fd, text):
    for ch in text.encode():
        fcntl.ioctl(fd, termios.TIOCSTI, bytes([ch]))
        time.sleep(0.01)
    time.sleep(0.5)
    fcntl.ioctl(fd, termios.TIOCSTI, b"\r")


def main(tool, every_min, first_wait=None):
    tool, _, mode = tool.partition(":")
    prompts = LONG_PROMPTS if mode == "long" else PROMPTS
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / f"inject-{tool}-{os.getpid()}.log"
    # 本脚本在后台进程组，改终端的 ioctl 会收到 SIGTTOU 而被暂停（2026-10-28 测试版实测）；忽略它，内核即放行。
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    fd = os.open("/dev/tty", os.O_RDWR)
    log(path, f"start tool={tool} mode={mode or 'short'} every={every_min}m cwd={os.getcwd()}")
    # 等 CLI 启动完（含首次的信任确认由人工处理），再随机错开第一问。
    time.sleep(first_wait if first_wait is not None else random.uniform(90, 240))
    n = 0
    while not stopped(tool):
        prompt = prompts[n % len(prompts)]
        try:
            type_line(fd, prompt)
        except OSError as e:
            log(path, f"inject failed: {e}")
            return 1
        n += 1
        log(path, f"prompt #{n}: {prompt}")
        wait = every_min * 60 + random.uniform(-300, 300)
        end = time.time() + max(60, wait)
        while time.time() < end:
            if stopped(tool):
                break
            time.sleep(20)
    log(path, f"stop after {n} prompts")
    return 0


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)
    sys.exit(main(sys.argv[1], float(sys.argv[2]), float(sys.argv[3]) if len(sys.argv) == 4 else None))
