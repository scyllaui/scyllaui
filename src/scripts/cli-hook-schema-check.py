#!/usr/bin/env python3
"""核对本项目给各款 CLI 装的 Hook 事件，都在本机已装版本接受的事件清单里（开发用，不进入产品运行时）。

用法：python3 -I src/scripts/cli-hook-schema-check.py [claude|codex|kimi|grok|opencode|copilot|antigravity ...]   # 缺省为全部
退出码：0 全部通过（没装的跳过）；1 有事件不在清单里或读不出清单；2 用法错误。

起因（2026-11-15 新用户验收）：Kimi 有两份事件清单，运行时与 `kimi doctor` 用的那份含 TurnStarted，改设置时
（登录写入托管配置、/logout）用的那份不含，装上本项目的 TurnStarted 后 /login 报 config.invalid（上游
MoonshotAI/kimi-code#4962）。所以要核对 CLI 带的每一份清单，不能只看文档、运行时或 doctor 的结论。
CLI 升级后的维护冒烟（smoke-cli.py）会先跑这一项。

各款清单从哪里读（2026-11-15 本机版本逐款核对过包内代码，依据见 docs/design.md「外部文件清单」）：
  kimi        包内 JS 里含这些事件名的数组有两份（运行时、改设置），取交集——两处校验都要过。
  claude      包内 JS 的主事件清单（最长的那份；设置校验、宽松加载、报错提示都用它），其余较短的是别的用途。
  copilot     包内 schemas/api.schema.json 的 HookType 枚举（与设置项注册表一致）。
  grok        包内内嵌的 Hooks 文档事件表（与二进制里的事件名表一致，所有来源共用一套）。
  opencode    事件名不写进它的配置（插件只订阅），不核对；改为核对它校验的插件导出形状（default 带 id 与 setup）。
  codex、antigravity
              只有一套清单，但在编译后的二进制里取不出成形的列表：只核对事件名在包里出现（弱核对，结果标明）。
"""
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HOOKCFG = ROOT / "src/core/proto/src/hookcfg.rs"
HOME = Path.home()
TOOLS = ("claude", "codex", "kimi", "grok", "opencode", "copilot", "antigravity")
COMMAND = {"antigravity": "agy"}
# 用来认出「事件清单」数组的常见事件名：一个数组里至少有这么多个才算。
CORE = {"SessionStart", "UserPromptSubmit", "Stop", "SessionEnd", "PreToolUse", "PostToolUse", "Notification", "PreCompact"}


def our_events():
    """hookcfg.rs 里 `events()` 的各款事件。"""
    text = HOOKCFG.read_text(encoding="utf-8")
    body = text[text.index("pub fn events(tool: Tool)"):]
    body = body[: body.index("\n}\n")]
    body = re.sub(r"//[^\n]*", "", body)
    out = {}
    for name, items in re.findall(r"Tool::(\w+)\s*=>\s*&\[(.*?)\]", body, re.S):
        out[name.lower()] = re.findall(r'"([^"]+)"', items)
    if set(out) != set(TOOLS):
        sys.exit(f"读不出 hookcfg.rs 的事件清单：{sorted(out)}")
    return out


def which(tool):
    p = shutil.which(COMMAND.get(tool, tool))
    return Path(p).resolve() if p else None


def version(tool):
    try:
        out = subprocess.run([COMMAND.get(tool, tool), "--version"], capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.TimeoutExpired):
        return "?"
    m = re.search(r"\d+\.\d+\.\d+", out)
    return m.group(0) if m else "?"


def blob(path):
    return Path(path).read_bytes()


ARRAY = re.compile(rb'\[\s*((?:"[A-Za-z]+"\s*,\s*){5,}"[A-Za-z]+")\s*,?\s*\]')


def js_lists(path):
    """包内 JS 里像事件清单的字符串数组（去重）。包有近 200 MB：先按字节找 `"SessionStart"`，只在它附近解析。"""
    data, lists, pos = blob(path), set(), 0
    while (i := data.find(b'"SessionStart"', pos)) >= 0:
        pos = i + 1
        start = data.rfind(b"[", max(0, i - 2000), i)
        if start < 0:
            continue
        m = ARRAY.match(data, start, min(len(data), i + 2000))
        if not m:
            continue
        names = tuple(n.decode() for n in re.findall(rb'"([A-Za-z]+)"', m.group(1)))
        if len(CORE & set(names)) >= 5:
            lists.add(names)
    return [set(x) for x in lists]


def accepted(tool):
    """返回（接受的事件集合或 None，方式说明，是否严格）。读不出时集合为 None，说明里写原因。"""
    exe = which(tool)
    if tool == "kimi":
        exe = HOME / ".kimi-code/bin/kimi"
        if not exe.exists():
            return None, "没装", True
        lists = js_lists(exe)
        if len(lists) < 2:
            return set(), f"包内只找到 {len(lists)} 份事件清单（预期运行时、改设置两份），提取规则可能失效", True
        return set.intersection(*lists), f"{len(lists)} 份清单的交集", True
    if exe is None:
        return None, "没装", True
    if tool == "claude":
        lists = js_lists(exe)
        if not lists:
            return set(), "包内找不到事件清单，提取规则可能失效", True
        return max(lists, key=len), "主事件清单", True
    if tool == "copilot":
        caches = sorted((HOME / "Library/Caches/copilot/pkg/darwin-arm64").glob("*/schemas/api.schema.json"),
                        key=lambda p: [int(x) for x in re.findall(r"\d+", p.parent.parent.name)])
        want = version(tool)
        hit = [p for p in caches if p.parent.parent.name == want] or caches[-1:]
        if not hit:
            return set(), "找不到 schemas/api.schema.json", True
        enums = []

        def walk(x):
            if isinstance(x, dict):
                if isinstance(x.get("HookType"), dict):
                    enums.append(x["HookType"].get("enum") or [])
                for v in x.values():
                    walk(v)
            elif isinstance(x, list):
                for v in x:
                    walk(v)
        walk(json.loads(hit[0].read_text(encoding="utf-8")))
        if not enums:
            return set(), "schema 里没有 HookType 枚举", True
        return set.intersection(*(set(e) for e in enums)), f"HookType 枚举（{hit[0].parent.parent.name}）", True
    if tool == "grok":
        data, names, pos = blob(exe), set(), 0
        while (i := data.find(b"\n| `", pos)) >= 0:
            pos = i + 1
            m = re.match(rb"\n\| `([A-Z][A-Za-z]+)` \|", data[i:i + 80])
            if m:
                names.add(m.group(1).decode())
        if len(CORE & names) < 5:
            return set(), "包内找不到 Hooks 文档的事件表，提取规则可能失效", True
        return names, "包内 Hooks 文档事件表", True
    if tool == "opencode":
        # 插件的事件名不写进 OpenCode 的配置（插件只订阅，其中有 OpenCode 1 的名字，2 里没有也无妨）；OpenCode 校验的是
        # 插件导出形状：default 带 id，加 effect 或 setup 函数。核对包里还是这条规则、我们的插件照此导出。
        rule = b"Plugin must export a default definition with an id and an effect or setup function"
        plugin = (ROOT / "src/core/proto/src/opencode-plugin.js").read_text(encoding="utf-8")
        shape = re.search(r"export default \{\s*id: \"scyllaui\",.*?\n  async setup\(", plugin, re.S)
        if rule not in blob(exe):
            return set(), "包里找不到插件导出形状的校验规则，规则可能变了，要重新核对", True
        if not shape:
            return set(), "我们的插件不是 default {id, setup} 导出", True
        return set(our_events()[tool]), "插件导出形状（事件名不进配置，不核对）", True
    # codex、antigravity：只核对名字在包里出现。
    data = blob(exe)
    return {n for n in our_events()[tool] if n.encode() in data}, "事件名在包里出现（弱核对）", False


def main(argv):
    tools = argv or list(TOOLS)
    if any(t not in TOOLS for t in tools):
        print(__doc__)
        return 2
    ours = our_events()
    failed = 0
    for t in tools:
        got, how, strict = accepted(t)
        if got is None:
            print(f"== {t}：{how}，跳过")
            continue
        missing = [e for e in ours[t] if e not in got]
        ver = version(t)
        if missing or not got:
            failed += 1
            detail = f"不在清单里：{', '.join(missing)}" if got else how
            print(f"== {t} {ver}：未通过（{how}）{detail}")
        else:
            count = "" if t == "opencode" else f"，{len(ours[t])} 个事件"
            print(f"== {t} {ver}：通过（{how}{count}）")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
