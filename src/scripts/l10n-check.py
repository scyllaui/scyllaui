#!/usr/bin/env python3
"""M2 第 6 块：界面文字的翻译检查（开发用，不进入产品运行时；docs/design.md 第 5 节「实现（M2 第 6 块）」）。

键由编译器从 String(localized:) 提取（构建时 SWIFT_EMIT_LOC_STRINGS 产出的 .stringsdata，格式符由编译器生成）。核对：
  1. 提取出的键在 Localizable.xcstrings 中都有；
  2. 每个键都有非空的英文翻译，状态为 translated；
  3. 英文译文的格式符与键一致（种类与个数，允许用 %1$@ 这类位置写法调换顺序）。
另报告 catalog 里已不在代码中的键（不算失败）。
另核对 Info.plist 的用途说明（M4b 块 1）：project.yml 里每个 `INFOPLIST_KEY_*UsageDescription` 在 InfoPlist.xcstrings 都有非空、
状态为 translated 的英文，且 catalog 的中文与 project.yml 一致。任一项不通过返回 1。
用法：l10n-check.py [Debug 构建的 Intermediates 目录]   缺省为 output/build/Build/Intermediates.noindex
同步新键：xcrun xcstringstool sync src/app/ScyllaUI/Localizable.xcstrings --stringsdata <上述目录下 ScyllaUI.build 的 *.stringsdata>
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / "src/app/ScyllaUI/Localizable.xcstrings"
INFOPLIST = ROOT / "src/app/ScyllaUI/InfoPlist.xcstrings"
PROJECT = ROOT / "src/app/project.yml"
SPEC = re.compile(r"%(?:(\d+)\$)?(lld|ld|d|@|f|lf|%)")


def specs(s):
    """格式符的种类（按位置排好）；`%%` 不算。"""
    out, auto = {}, 0
    for m in SPEC.finditer(s):
        if m.group(2) == "%":
            continue
        auto += 1
        out[int(m.group(1)) if m.group(1) else auto] = m.group(2)
    return [out[k] for k in sorted(out)]


def variants(loc):
    """一条英文翻译的全部写法，每项为 {state, value}。复数变体（2026-10-31 起）有两种写法：
    整句的 `variations.plural`；或 `stringUnit` 里用 `%#@名@` 占位、`substitutions` 给出该参数的单复数，
    此时把 `%arg` 换回 `%序号$格式`，展开成整句再核对格式符。"""
    plural = loc.get("variations", {}).get("plural")
    if plural:
        return [v.get("stringUnit", {}) for v in plural.values()]
    unit = loc.get("stringUnit")
    if not unit:
        return []
    subs = loc.get("substitutions", {})
    if not subs:
        return [unit]
    out = []
    for name, sub in subs.items():
        token = f"%#@{name}@"
        arg = f"%{sub.get('argNum', 1)}${sub.get('formatSpecifier', 'lld')}"
        for form in sub.get("variations", {}).get("plural", {}).values():
            u = form.get("stringUnit", {})
            if u.get("state") != "translated":
                out.append(u)
                continue
            value = unit.get("value", "").replace(token, u.get("value", "").replace("%arg", arg))
            out.append({"state": unit.get("state"), "value": value})
    return out


def main():
    inter = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "output/build/Build/Intermediates.noindex"
    files = sorted(inter.glob("ScyllaUI.build/Debug/ScyllaUI.build/**/*.stringsdata"))
    if not files:
        print(f"没有找到 .stringsdata（先用 build-app.sh Debug 构建）：{inter}")
        return 1
    used = {}
    for f in files:
        for entry in json.loads(f.read_text(encoding="utf-8"))["tables"].get("Localizable", []):
            used.setdefault(entry["key"], f"{Path(json.loads(f.read_text())['source']).name}:{entry['location']['startingLine']}")
    catalog = json.loads(CATALOG.read_text(encoding="utf-8"))["strings"]
    bad = 0
    for key, where in sorted(used.items()):
        if key not in catalog:
            print(f"缺键：{key!r}（{where}）")
            bad += 1
            continue
        units = variants(catalog[key].get("localizations", {}).get("en", {}))
        if not units:
            print(f"未翻译：{key!r}（{where}）")
            bad += 1
        for unit in units:
            value = unit.get("value", "")
            if unit.get("state") != "translated" or not value.strip():
                print(f"未翻译：{key!r}（{where}）")
                bad += 1
                break
            if specs(key) != specs(value):
                print(f"格式符不一致：{key!r} → {value!r}")
                bad += 1
                break
    stale = sorted(set(catalog) - set(used))
    for key in stale:
        print(f"代码里已不用（未计为失败）：{key!r}")
    # 用途说明：project.yml 的中文为源文，InfoPlist.xcstrings 给英文。
    usage = dict(re.findall(r'^\s+INFOPLIST_KEY_(NS\w+UsageDescription): "(.*)"$', PROJECT.read_text(encoding="utf-8"), re.M))
    plist = json.loads(INFOPLIST.read_text(encoding="utf-8"))["strings"]
    for key, zh in sorted(usage.items()):
        locs = plist.get(key, {}).get("localizations", {})
        en = locs.get("en", {}).get("stringUnit", {})
        if en.get("state") != "translated" or not en.get("value", "").strip():
            print(f"用途说明未翻译：{key}")
            bad += 1
        if locs.get("zh-Hans", {}).get("stringUnit", {}).get("value") != zh:
            print(f"用途说明中文与 project.yml 不一致：{key}")
            bad += 1
    for key in sorted(set(plist) - set(usage)):
        print(f"InfoPlist.xcstrings 里多余的键（未计为失败）：{key}")
    print(f"键 {len(used)} 个（来自 {len(files)} 个 .stringsdata），catalog {len(catalog)} 个，用途说明 {len(usage)} 个，问题 {bad} 个")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
