#!/usr/bin/env python3
"""24 小时长跑：持续输出的终端（开发用，docs/design.md 10.6 节「24 小时长跑方案」）。
每秒一行，每 10 分钟一次约 3000 行的大段输出，给终端渲染加压力。output/longrun/stop-spam 或 stop-all 存在时退出。"""
import time
from pathlib import Path

OUT = Path(__file__).resolve().parents[3] / "output/longrun"
n = 0
while not ((OUT / "stop-spam").exists() or (OUT / "stop-all").exists()):
    n += 1
    print(time.strftime("%Y-%m-%d %H:%M:%S"), f"tick {n}", "·" * (n % 60), flush=True)
    if n % 600 == 0:
        for i in range(3000):
            print(f"  burst {n}/{i:04d} " + "abcdefghij" * (i % 12), flush=False)
        print(flush=True)
    time.sleep(1)
