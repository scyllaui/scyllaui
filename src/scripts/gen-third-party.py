#!/usr/bin/env python3
"""M4b 块 4：从实际产物生成随包的第三方许可清单（开发用，不进入产品运行时；docs/design.md 10.6 节「块 4 实现」）。

判定依据：
  - libghostty：读最近一次 ScyllaUI 构建的链接映射（project.yml 打开了 LD_GENERATE_MAP_FILE），只列真正链进程序的成员；
  - libghostty-vt（scyllauid 链接）：`ar t` 全部成员；
  - Zig 依赖都编进同一个对象文件，按 Ghostty 为本次构建取到的 Zig 包全部列出，构建工具与测试数据除外；
  - 内嵌字体：按字体文件的字节片段在 libghostty 的 Zig 对象里查找；
  - 随包资源（主题、bash-preexec）、Ghostty 本身、工具图标；
  - Rust：scyllauid、scyllaui 在 aarch64 上的正常依赖，许可文本读项目局部 registry。
任何成员对应不上包、包缺许可文本、字体没有登记，都失败（返回 1）。

产物 src/app/ScyllaUI/Resources/ThirdPartyNotices.txt 开头记输入指纹（两个静态库的成员表、Ghostty commit、Zig 包目录、
Rust 依赖、本脚本与 third-party/ 下的许可文本）。
用法：gen-third-party.py            生成（需要先构建过一次 ScyllaUI，以得到链接映射）
      gen-third-party.py --check    只重算指纹与产物比对，不需要链接映射；不一致返回 1（build-app.sh 构建前调用）
"""
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GH = ROOT / "vendor/ghostty"
ZPKG = GH / "zig-pkg"
LIB = GH / "macos/GhosttyKit.xcframework/macos-arm64/libghostty-internal.a"
VT = ROOT / "output/ghostty-vt/lib/libghostty-vt.a"
MAPS = ROOT / "output/build/Build/Intermediates.noindex/ScyllaUI.build"
EXTRA = ROOT / "src/scripts/third-party"
OUT = ROOT / "src/app/ScyllaUI/Resources/ThirdPartyNotices.txt"
REGISTRY = ROOT / ".tools/rust/cargo/registry/src"
ZIG = ROOT / ".tools/zig/0.16.0"
FINGERPRINT = "Fingerprint: "
# 对外显示的产品名：只出现在本文件的说明文字里，与各许可无关；块 2 改名 ScyllaUI 时只改这里（docs/design.md 10.6 节 D2）。
PRODUCT = "ScyllaUI"
# 真正的版权行：Copyright 后接 (c)、© 或年份；许可正文里的「copyright owner」「COPYRIGHT HOLDERS」不算。
COPYRIGHT = re.compile(r"^\s*(copyright\s*(\(c\)|©|\d{4})|\(c\)\s*\d{4}|©\s*\d{4}|portions of this software are copyright)", re.I | re.M)


class Fail(Exception):
    pass


def zdir(prefix):
    """zig-pkg 下以 prefix 开头的唯一目录（Ghostty 升级后目录名变了就失败，提醒更新本表）。"""
    hits = sorted(p for p in ZPKG.glob(prefix + "*") if p.is_dir())
    if len(hits) != 1:
        raise Fail(f"zig-pkg 里找不到唯一的 {prefix}*（找到 {len(hits)} 个），Ghostty 升级后要更新本脚本的包表")
    return hits[0]


# 包表：名称、来源、许可（SPDX）、许可文件（先到先用）、另加的版权行。`dir` 为包目录（相对路径按它解析）。
def packages():
    return {
        "ghostty": dict(name="Ghostty", source="https://github.com/ghostty-org/ghostty (commit {commit})", spdx="MIT",
                        dir=GH, files=["LICENSE"],
                        note="Terminal components, terminfo and shell integration scripts; portions of the app are derived "
                             "from its macOS sources."),
        "zig": dict(name="Zig standard library and compiler_rt", source="https://ziglang.org (0.16.0)", spdx="MIT",
                    dir=ZIG, files=["LICENSE"], note="Compiled into the terminal engine."),
        "freetype": dict(name="FreeType", source="https://freetype.org", spdx="FTL", dir=zdir("N-V-__8AAKLKpwC4H27Ps_0iL3bPkQb"),
                         files=["docs/FTL.TXT"],
                         copyright=["Portions of this software are copyright (c) The FreeType Project (www.freetype.org). All rights reserved."]),
        "libpng": dict(name="libpng", source="http://www.libpng.org/pub/png/libpng.html", spdx="libpng-2.0",
                       dir=zdir("N-V-__8AAJrvXQCqAT8Mg9o_tk6m0yf5Fz"), files=["LICENSE"]),
        "zlib": dict(name="zlib", source="https://zlib.net", spdx="Zlib", dir=zdir("N-V-__8AAB0eQwD-0MdOEBmz7intriBR"), files=["LICENSE"]),
        "oniguruma": dict(name="Oniguruma", source="https://github.com/kkos/oniguruma", spdx="BSD-2-Clause",
                          dir=zdir("N-V-__8AAHjwMQDBXnLq3Q2QhaivE0kE2aD"), files=["COPYING"]),
        "glslang": dict(name="glslang", source="https://github.com/KhronosGroup/glslang", spdx="BSD-3-Clause AND Apache-2.0 AND others (see text)",
                        dir=zdir("N-V-__8AABzkUgISeKGgXAzgtutgJsZc0"), files=["LICENSE.txt"]),
        "spirv-cross": dict(name="SPIRV-Cross", source="https://github.com/KhronosGroup/SPIRV-Cross", spdx="Apache-2.0",
                            dir=zdir("N-V-__8AANb6pwD7O1WG6L5nvD_rNMvnSc9"), files=["LICENSE"],
                            # LICENSE 只有 Apache 正文，版权行取自所链接源文件的文件头。
                            copyright=["Copyright 2015-2021 Arm Limited", "Copyright 2016-2021 The Brenwill Workshop Ltd.",
                                       "Copyright 2019-2021 Hans-Kristian Arntzen"]),
        "sentry": dict(name="sentry-native", source="https://github.com/getsentry/sentry-native", spdx="MIT",
                       dir=zdir("N-V-__8AAPlZGwBEa-gxrcypGBZ2R8Bse4J"), files=["LICENSE"],
                       note="Crash reporting library; {product} configures no upload transport."),
        "mpack": dict(name="MPack (vendored in sentry-native)", source="https://github.com/ludocode/mpack", spdx="MIT",
                      dir=zdir("N-V-__8AAPlZGwBEa-gxrcypGBZ2R8Bse4J"), header="vendor/mpack.h"),
        "breakpad": dict(name="Breakpad", source="https://chromium.googlesource.com/breakpad/breakpad", spdx="BSD-3-Clause AND others (see text)",
                         dir=zdir("N-V-__8AALw2uwF_03u4JRkZwRLc3Y9hakk"), files=["LICENSE"]),
        "simdutf": dict(name="simdutf", source="https://github.com/simdutf/simdutf", spdx="Apache-2.0 OR MIT (MIT chosen)",
                        dir=EXTRA, files=["simdutf-mit.txt"]),
        "highway": dict(name="Highway", source="https://github.com/google/highway", spdx="Apache-2.0 OR BSD-3-Clause (BSD-3-Clause chosen)",
                        dir=zdir("N-V-__8AAGmZhABbsPJLfbqrh6JTHsXhY6q"), files=["LICENSE-BSD3"]),
        "imgui": dict(name="Dear ImGui", source="https://github.com/ocornut/imgui", spdx="MIT",
                      dir=zdir("N-V-__8AAEbOfQBnvcFcCX2W5z7tDaN8vaN"), files=["LICENSE.txt"]),
        "dear_bindings": dict(name="Dear Bindings", source="https://github.com/dearimgui/dear_bindings (v0.17)", spdx="MIT",
                              dir=EXTRA, files=["dear-bindings.txt"]),
        "wuffs": dict(name="Wuffs", source="https://github.com/google/wuffs", spdx="Apache-2.0 OR MIT (MIT chosen)",
                      dir=zdir("N-V-__8AAP5JWgCGP_AD0teWpa4krRvE9VP"), files=["LICENSE-MIT"]),
        "libintl": dict(name="GNU libintl (gettext)", source="https://www.gnu.org/software/gettext/", spdx="LGPL-2.1-or-later",
                        dir=zdir("N-V-__8AADcZkgn4cMhTUpIz6mShCKyqqB"), files=["gettext-runtime/intl/COPYING.LIB"]),
        "libxev": dict(name="libxev", source="https://github.com/mitchellh/libxev", spdx="MIT", dir=zdir("libxev-"), files=["LICENSE"]),
        "uucode": dict(name="uucode", source="https://github.com/jacobsandlund/uucode", spdx="MIT AND Unicode-3.0",
                       dir=zdir("uucode-"), files=["LICENSE.md"]),
        "vaxis": dict(name="libvaxis", source="https://github.com/rockorager/libvaxis", spdx="MIT", dir=zdir("vaxis-"), files=["LICENSE"]),
        "z2d": dict(name="z2d", source="https://github.com/vancluever/z2d (v0.12.1)", spdx="MPL-2.0",
                    dir=EXTRA, files=["z2d-mpl-2.0.txt"],
                    note="Unmodified; source code is available at the address above."),
        "zf": dict(name="zf", source="https://github.com/natecraddock/zf", spdx="MIT", dir=zdir("zf-"), files=["LICENSE"]),
        "zig_objc": dict(name="zig-objc", source="https://github.com/mitchellh/zig-objc", spdx="MIT", dir=zdir("zig_objc-"), files=["LICENSE"]),
        "zigimg": dict(name="zigimg", source="https://github.com/zigimg/zigimg", spdx="MIT", dir=zdir("zigimg-"), files=["LICENSE"]),
        "themes": dict(name="iTerm2-Color-Schemes (terminal color themes)", source="https://github.com/mbadolato/iTerm2-Color-Schemes",
                       spdx="MIT", dir=EXTRA, files=["iterm2-color-schemes.txt"],
                       note="Bundled terminal color themes; individual themes credit their authors in the theme files."),
        "bash-preexec": dict(name="bash-preexec", source="https://github.com/rcaloras/bash-preexec (0.7.0)", spdx="MIT",
                             dir=EXTRA, files=["bash-preexec.txt"], note="Bundled with the bash shell integration."),
        "lobe-icons": dict(name="lobe-icons (tool icons)", source="https://github.com/lobehub/lobe-icons (@lobehub/icons-static-svg 1.95.1; tool-opencode from 1.101.0)",
                           spdx="MIT", dir=EXTRA, files=["lobe-icons.txt"],
                           note="The Grok, OpenCode and GitHub Copilot icons. The marks shown are trademarks of their respective owners "
                                "(xAI, the OpenCode project, GitHub); {product} uses them only to identify the corresponding command-line tools."),
        "sparkle": dict(name="Sparkle (software updates)", source="https://github.com/sparkle-project/Sparkle (2.10.1)", spdx="MIT",
                        dir=EXTRA, files=["sparkle.txt"],
                        note="Sparkle.framework is embedded for checking, downloading and installing updates; "
                             "the license text includes the notices of the components Sparkle bundles."),
        "jetbrains-mono": dict(name="JetBrains Mono (interface font)", source="https://github.com/JetBrains/JetBrainsMono (v2.304)",
                               spdx="OFL-1.1", dir=EXTRA, files=["jetbrains-mono-ofl.txt"],
                               note="JetBrainsMono-Regular.ttf and JetBrainsMono-SemiBold.ttf are shipped unmodified for the sidebar, "
                                    "prompt history and title bar."),
        "fusion-pixel-font": dict(name="Fusion Pixel Font (splash lettering)", source="https://github.com/TakWolf/fusion-pixel-font (release 2026.10.25, 12px proportional zh_hans)",
                                  spdx="OFL-1.1", dir=EXTRA, files=["fusion-pixel-font-ofl.txt"],
                                  note="The pixel glyphs of the splash subtitle, button and footer (Resources/pixel-text.json) are taken from this font; "
                                       "the font file itself is not shipped."),
    }


# 链接成员 → 包。按顺序匹配整个成员名（去掉 .o）。对应不上的成员让脚本失败。
MEMBERS = [
    (r"autofit|ft\w+|bdf|cff|type1cid|pcf|pfr|psaux|pshinter|psnames|raster|sdf|sfnt|smooth|svg|truetype|type1|type42|winfnt", "freetype"),
    (r"png\w*", "libpng"),
    (r"adler32|compress|crc32|deflate|gz\w+|inflate|infback|inftrees|inffast|trees|uncompr|zutil", "zlib"),
    (r"reg\w+|st|onig_init|unicode|ascii|utf8|utf16_[bl]e|utf32_[bl]e|euc_\w+|sjis\w*|iso8859_\d+|big5|gb18030|koi8_r|cp1251|"
     r"unicode_\w+_key", "oniguruma"),
    (r"CodeGen|Link|glslang_\w+|attribute|Constant|iomapper|InfoSink|Initialize|IntermTraverse|Intermediate|ParseContextBase|"
     r"ParseHelper|PoolAlloc|RemoveTree|Scan|ShaderLang|SpirvIntrinsics|SymbolTable|Versions|intermOut|limits|linkValidate|"
     r"parseConst|reflection|Pp\w*|propagateNoContraction|ResourceLimits|resource_limits_c|GlslangToSpv|InReadableOrder|Logger|"
     r"SpvBuilder|SpvPostProcess|doc|disassemble|spirv_c_interface|ossource", "glslang"),
    (r"spirv_cross\w*|spirv_parser|spirv_cfg|spirv_glsl|spirv_msl", "spirv-cross"),
    (r"sentry_\w+", "sentry"),
    (r"mpack", "mpack"),
    (r"convert_UTF|md5|string_conversion|arch_utilities|file_id|macho_\w+|string_utilities|minidump_\w+|breakpad_nlist_64|"
     r"dynamic_images|MachIPC|bootstrap_compat|exception_handler|crash_generation_client", "breakpad"),
    (r"simdutf", "simdutf"),
    (r"abort|per_target|targets|libhighway_zcu", "highway"),
    (r"bindtextdom|dcgettext|dcigettext|dcngettext|dgettext|dngettext|explodename|finddomain|gettext|hash-string|intl-compat|"
     r"l10nflist|langprefs|loadmsgcat|localealias|log|ngettext|plural-exp|plural|setlocale|textdomain|version|compat|"
     r"getlocalename_l-unsafe|localename|localename-environ|localename-unsafe|setlocale-lock|setlocale_null|setlocale_null-unlocked", "libintl"),
    (r"dcimgui\w*", "dear_bindings"),
    (r"imgui\w*", "imgui"),
    (r"wuffs-v[\d.]+", "wuffs"),
    (r"compiler_rt", "zig"),
    (r"zig_macos|ext|base64|codepoint_width|index_of|vt|libghostty_zcu|libghostty-vt-static_zcu", "ghostty"),
]
# 成员属于这些包时一并列出的包（Dear Bindings 生成的是 Dear ImGui 的 C 接口）。
ALSO = {"dear_bindings": ["imgui"]}
# 不能静默链进来的包：LGPL 要用户决定怎么履行义务（design.md 10.6 节「块 4 实现」）。
FORBIDDEN = {"libintl": "libintl（LGPL-2.1）链进了 ScyllaUI，需要用户决定履行 LGPL 义务的方式（开源、提供可重新链接的对象文件，或改构建去掉它）"}
# 按成员禁止（D15，2026-11-07，docs/design.md 10.6 节「D15 开源准备」）：包本身兼容 GPL-3.0、个别文件不兼容的。
# Breakpad 的 breakpad_nlist_64.cc 是 APSL-2.0，编进了 libghostty 静态库，但现在没有链进 ScyllaUI；Ghostty 升级后链进来就失败。
FORBIDDEN_MEMBERS = {"breakpad_nlist_64": "Breakpad 的 breakpad_nlist_64.o（APSL-2.0，与本项目的 GPL-3.0-or-later 不兼容）链进了 ScyllaUI，"
                                         "需要用户决定处理方式（改 Ghostty 的构建去掉它，或不再链接 Breakpad）"}
# Ghostty 取到的 Zig 包里不列的及原因；其余按全部编进 libghostty 处理。
ZIG_ALWAYS = ["libxev", "uucode", "vaxis", "z2d", "zf", "zig_objc", "zigimg"]
ZIG_SKIP = {"aro-": "build-time C translator", "translate_c-": "build-time C translator"}

# 可能内嵌的字体：文件名 → (名称, 版权, 许可文件)。查到内嵌却没登记的字体让脚本失败。
RES = GH / "src/font/res"
FONTS = {
    r"JetBrainsMono.*": ("JetBrains Mono", "Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)",
                         "jetbrains:OFL.txt"),
    r"SymbolsNerdFont.*": ("Symbols Nerd Font", "Copyright (c) 2014 Ryan L McIntyre and Nerd Fonts contributors", "nerd:LICENSE"),
    r"NotoColorEmoji\.ttf|NotoEmoji-Regular\.ttf": ("Noto Emoji", "Copyright 2013 Google LLC", "res:OFL.txt"),
    r"KawkabMono-Regular\.ttf": ("Kawkab Mono", "Copyright (c) 2015, Abdullah Arif. Copyright 2010, 2012, 2014 Adobe Systems "
                                 "Incorporated, with Reserved Font Name 'Source'.", "res:OFL.txt"),
}


def run(cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode != 0:
        raise Fail(f"{' '.join(map(str, cmd))} 失败：{r.stderr.strip()[:300]}")
    return r.stdout


def ar_members(lib):
    if not lib.exists():
        raise Fail(f"缺少 {lib}")
    return sorted(m[:-2] for m in run(["ar", "t", str(lib)]).split() if m.endswith(".o"))


def package_of(member):
    for pattern, key in MEMBERS:
        if re.fullmatch(pattern, member):
            return key
    raise Fail(f"静态库成员 {member}.o 对应不上任何包，要在本脚本的 MEMBERS 里登记")


def linked_members():
    """最近一次 ScyllaUI 构建的链接映射里，libghostty 实际链接的成员。"""
    maps = sorted(MAPS.glob("*/ScyllaUI.build/*-LinkMap-*.txt"), key=lambda p: p.stat().st_mtime)
    if not maps:
        raise Fail("没有链接映射：先运行 src/scripts/build-app.sh Debug 构建一次")
    m = maps[-1]
    if m.stat().st_mtime < LIB.stat().st_mtime:
        raise Fail(f"链接映射 {m.name} 比 libghostty 旧：先重新构建 ScyllaUI")
    text = m.read_text(encoding="utf-8", errors="replace")
    objects = text.split("# Object files:", 1)[1].split("# Sections:", 1)[0]
    found = sorted(set(re.findall(r"libghostty-internal\.a\((.+?)\.o\)", objects)))
    if not found:
        raise Fail(f"链接映射 {m} 里没有 libghostty 的成员")
    return found, m


def rust_deps():
    out = run([str(ROOT / "src/scripts/with-rust.sh"), "cargo", "tree", "--manifest-path", str(ROOT / "src/core/Cargo.toml"),
               "-e", "normal", "--prefix", "none", "--target", "aarch64-apple-darwin", "-p", "scyllauid", "-p", "scyllaui",
               "--format", "{p}|{l}"])
    deps = {}
    for line in out.splitlines():
        p, _, lic = line.partition("|")
        if not p.strip() or "(/" in p:  # 空行；本项目自己的 crate
            continue
        name, version = p.replace(" (*)", "").replace(" (proc-macro)", "").split()[:2]
        deps[(name, version.lstrip("v"))] = lic.strip()
    return sorted((n, v, lic) for (n, v), lic in deps.items())


def zig_dirs():
    return sorted(p.name for p in ZPKG.iterdir() if p.is_dir())


def fingerprint(lib_members, vt_members, deps):
    h = hashlib.sha256()
    commit = run(["git", "-C", str(GH), "rev-parse", "HEAD"]).strip()
    h.update(json.dumps([lib_members, vt_members, commit, zig_dirs(), deps], ensure_ascii=False).encode())
    for f in [Path(__file__)] + sorted(EXTRA.iterdir()):
        h.update(f.name.encode() + f.read_bytes())
    return h.hexdigest(), commit


def license_text(pkg):
    d = pkg["dir"]
    if "header" in pkg:
        text = (d / pkg["header"]).read_text(encoding="utf-8")
        block = text.split("*/", 1)[0].lstrip("/*")
        return "\n".join(line.strip()[2:] if line.strip().startswith("* ") else line.strip().lstrip("*") for line in block.splitlines()).strip()
    for f in pkg["files"]:
        if (d / f).is_file():
            return (d / f).read_text(encoding="utf-8", errors="replace").strip()
    raise Fail(f"{pkg['name']}：找不到许可文件 {pkg['files']}（在 {d}）")


def embedded_fonts():
    """libghostty 的 Zig 对象里内嵌了哪些字体：字体文件开头、中段、末尾各取 64 字节，三段都找到才算（同一家族的字体
    表结构相近，只查一段会把没内嵌的字重误判进来，2026-10-28 实测）。"""
    zcu = subprocess.run(["ar", "p", str(LIB), "libghostty_zcu.o"], capture_output=True, check=True).stdout
    roots = {"jetbrains": zdir("N-V-__8AAIC5lwAVPJJzxnCAahSvZTIlG"), "nerd": zdir("N-V-__8AAMVLTABmYkLqhZPLXnMl"), "res": RES}
    found = {}
    for root in roots.values():
        for f in sorted(list(root.rglob("*.ttf")) + list(root.rglob("*.otf"))):
            if f.name.startswith("._"):
                continue
            data = f.read_bytes()
            if not all(data[o:o + 64] in zcu for o in (1024, len(data) // 2, len(data) - 200)):
                continue
            key = next((k for k in FONTS if re.fullmatch(k, f.name)), None)
            if key is None:
                raise Fail(f"内嵌了没有登记的字体 {f.name}，要在本脚本的 FONTS 里登记名称、版权与许可")
            found.setdefault(key, []).append(f.name)
    out = []
    for key, files in found.items():
        name, copyright, lic = FONTS[key]
        root, _, rel = lic.partition(":")
        text = (roots[root] / rel).read_text(encoding="utf-8", errors="replace").strip()
        out.append((name, ", ".join(sorted(files)), copyright, text))
    return sorted(out)


def rust_license(name, version, spdx):
    d = next(iter(REGISTRY.glob(f"*/{name}-{version}")), None)
    if d is None:
        raise Fail(f"registry 里没有 {name}-{version}")
    prefer = ["LICENSE-MIT", "LICENSE.md", "LICENSE", "LICENSE.txt"] if "MIT" in spdx else ["LICENSE", "LICENSE-APACHE", "LICENSE.md"]
    f = next((d / p for p in prefer if (d / p).is_file()), None)
    if f is None:
        raise Fail(f"{name}-{version}：找不到许可文件")
    text = f.read_text(encoding="utf-8", errors="replace").strip()
    copyright = []
    if not COPYRIGHT.search(text):
        authors = re.search(r"^authors\s*=\s*\[(.*?)\]", (d / "Cargo.toml").read_text(encoding="utf-8"), re.M | re.S)
        names = re.findall(r'"([^"]+)"', authors.group(1)) if authors else []
        copyright = [f"Copyright (c) {', '.join(re.sub(r' <[^>]*>', '', a) for a in names) or f'the {name} developers'}"]
    chosen = " (MIT chosen)" if " OR " in spdx or "/" in spdx else ""
    return copyright, text, chosen


def section(title, lines, text):
    if not COPYRIGHT.search("\n".join(lines + [text])):
        raise Fail(f"{title}：没有版权行，要在包表里补 copyright")
    return "\n".join(["=" * 78, title, "-" * 78] + lines + ([""] if lines else []) + [text, ""])


def generate():
    lib_members, vt_members, deps = ar_members(LIB), ar_members(VT), rust_deps()
    fp, commit = fingerprint(lib_members, vt_members, deps)
    linked, linkmap = linked_members()
    unknown = set(linked) - set(lib_members)
    if unknown:
        raise Fail(f"链接映射里的成员不在当前 libghostty 里：{sorted(unknown)[:5]}，先重新构建 ScyllaUI")
    used = {}
    for member in linked + vt_members:
        key = package_of(member)
        used.setdefault(key, set()).add(member)
        for extra in ALSO.get(key, []):
            used.setdefault(extra, set())
    for key, why in FORBIDDEN.items():
        if key in used:
            raise Fail(why)
    for member, why in FORBIDDEN_MEMBERS.items():
        if member in linked or member in vt_members:
            raise Fail(why)
    pk = packages()
    for key in ZIG_ALWAYS + ["ghostty", "themes", "bash-preexec", "lobe-icons", "fusion-pixel-font", "jetbrains-mono", "sparkle"]:
        used.setdefault(key, set())
    skipped = [f"{p.name}: {why}" for p in sorted(ZPKG.iterdir()) for pre, why in ZIG_SKIP.items() if p.name.startswith(pre)]

    order = ["ghostty", "zig"] + sorted((k for k in used if k not in ("ghostty", "zig")), key=lambda k: pk[k]["name"].lower())
    parts = [
        f"{PRODUCT} — Third-Party Notices / 第三方许可",
        "Generated by src/scripts/gen-third-party.py from the shipped build; do not edit by hand.",
        FINGERPRINT + fp,
        "",
        f"{PRODUCT} includes the third-party software listed below. Each entry gives its source, license and the full",
        "license text. Trademarks belong to their respective owners.",
        "",
    ]
    for key in order:
        p = pk[key]
        lines = [f"Source: {p['source'].format(commit=commit[:12])}", f"License: {p['spdx']}"]
        if p.get("note"):
            lines.append(p["note"].format(product=PRODUCT))
        lines += p.get("copyright", [])
        parts.append(section(p["name"], lines, license_text(p)))
    for name, files, copyright, text in embedded_fonts():
        parts.append(section(f"{name} (font)", [f"Files: {files}", "License: OFL-1.1" if "Open Font" in text
                                                                               else "License: MIT", copyright], text))
    for name, version, spdx in deps:
        copyright, text, chosen = rust_license(name, version, spdx)
        parts.append(section(f"{name} {version}", [f"Source: https://crates.io/crates/{name}", f"License: {spdx}{chosen}"]
                             + copyright, text))
    OUT.write_text("\n".join(parts).rstrip() + "\n", encoding="utf-8")
    print(f"已生成 {OUT.relative_to(ROOT)}：{len(order)} 个包、{len(embedded_fonts())} 款字体、{len(deps)} 个 Rust crate；"
          f"链接映射 {linkmap.name}（libghostty 链接 {len(linked)}／{len(lib_members)} 个成员）")
    if skipped:
        print("未列（构建时工具）：" + "；".join(skipped))
    return 0


def check():
    fp, _ = fingerprint(ar_members(LIB), ar_members(VT), rust_deps())
    current = OUT.read_text(encoding="utf-8").splitlines()[2] if OUT.exists() else ""
    if current == FINGERPRINT + fp:
        print("第三方许可清单与依赖一致")
        return 0
    print(f"错误：第三方许可清单已过期（依赖、Ghostty 或许可文本有变化）。先构建一次 ScyllaUI（Debug 即可），再运行 {Path(__file__).name}，"
          "提交生成的 ThirdPartyNotices.txt", file=sys.stderr)
    return 1


def main(argv):
    try:
        if argv == ["--check"]:
            return check()
        if argv:
            print(__doc__.strip(), file=sys.stderr)
            return 2
        return generate()
    except Fail as e:
        print(f"错误：{e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
