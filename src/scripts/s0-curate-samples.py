#!/usr/bin/env python3
"""把 scyllauid 采集的原始 Hook 帧脱敏后存为版本样本（辅助脚本，不进入产品运行时）。

用法：src/scripts/s0-curate-samples.py <采集目录> <工具>=<版本> [...]
例：  src/scripts/s0-curate-samples.py output/hook-samples/2026-10-08-s0-3 claude=2.1.292 grok=1.0.46
     同一采集文件含多个版本时，用 <工具>=<版本>@<起始行>-<结束行>（1 起，含两端）切分。
输出：tests/hook-samples/<工具>-<版本>.jsonl，每行 {config, source, dropped, payload, env}（协议 v3 起帧中不再有 event，由 scyllauid 按 source 规范化）。

脱敏：家目录路径替换为 ~；删除工具输出与助手回复；后台会话（transcript_path 为 null）
的提示词与工具输入整体隐去；计时字段不保留。
"""
import json
import sys
from pathlib import Path

HOME = str(Path.home())
DROP_KEYS = {"tool_response", "toolResponse", "tool_result", "toolResult", "last_assistant_message", "lastAssistantMessage"}
BACKGROUND_REDACT = {"prompt", "tool_input", "toolInput"}


def scrub(v):
    if isinstance(v, str):
        return v.replace(HOME, "~")
    if isinstance(v, list):
        return [scrub(x) for x in v]
    if isinstance(v, dict):
        return {k: scrub(x) for k, x in v.items() if k not in DROP_KEYS}
    return v


def curate(frame):
    payload = dict(frame.get("payload") or {})
    background = any(k in payload and payload[k] is None for k in ("transcript_path", "transcriptPath"))
    if background:
        for k in BACKGROUND_REDACT & payload.keys():
            payload[k] = "<redacted: background session>"
    return scrub({k: frame.get(k) for k in ("config", "source", "dropped", "env")} | {"payload": payload})


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    src = Path(sys.argv[1])
    out_dir = Path(__file__).resolve().parents[2] / "tests/hook-samples"
    out_dir.mkdir(parents=True, exist_ok=True)
    for spec in sys.argv[2:]:
        tool, ver = spec.split("=", 1)
        lo, hi = 1, None
        if "@" in ver:
            ver, rng = ver.split("@", 1)
            lo, hi = (int(x) for x in rng.split("-"))
        lines = (src / f"{tool}.jsonl").read_text().splitlines()[lo - 1:hi]
        rows = [json.dumps(curate(json.loads(l)["frame"]), ensure_ascii=False, sort_keys=True) for l in lines]
        out = out_dir / f"{tool}-{ver}.jsonl"
        out.write_text("\n".join(rows) + "\n")
        print(f"{out.relative_to(out_dir.parents[1])}：{len(rows)} 条")
    return 0


if __name__ == "__main__":
    sys.exit(main())
