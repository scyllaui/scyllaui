#!/bin/sh
# 用项目局部 Zig 0.16.0 和当前 Xcode 构建 GhosttyKit.xcframework（ReleaseFast，本机架构），并校验产物。
# 用法：src/scripts/build-ghosttykit.sh（先运行 fetch-ghostty.sh）
# 构建参数参照固定 commit 的 .github/workflows/release-tip.yml：zig build -Doptimize=ReleaseFast -Demit-macos-app=false
# -Di18n=false：本机没有 gettext 的 msgfmt，关闭翻译文件生成，理由见 docs/design.md 第 9 节。
# 产物：vendor/ghostty/macos/GhosttyKit.xcframework
set -eu

ZIG_VERSION="0.16.0"
GHOSTTY_COMMIT="34f39002c6e3974b54e6a6d400bd83777c5ea558"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
ZIG="$PROJECT_DIR/.tools/zig/$ZIG_VERSION/zig"
GHOSTTY_DIR="$PROJECT_DIR/vendor/ghostty"
XCFW="$GHOSTTY_DIR/macos/GhosttyKit.xcframework"

fail() { echo "错误：$*" >&2; exit 1; }

[ -x "$ZIG" ] || fail "未找到 $ZIG"
[ "$("$ZIG" version)" = "$ZIG_VERSION" ] || fail "Zig 版本不是 $ZIG_VERSION"
[ -d "$GHOSTTY_DIR/.git" ] || fail "未找到 ${GHOSTTY_DIR}，请先运行 src/scripts/fetch-ghostty.sh"
[ "$(git -C "$GHOSTTY_DIR" rev-parse HEAD)" = "$GHOSTTY_COMMIT" ] || fail "vendor/ghostty 不在固定 commit $GHOSTTY_COMMIT"

echo "Zig:   $("$ZIG" version)（${ZIG}）"
echo "Xcode: $(xcodebuild -version | tr '\n' ' ')（$(xcode-select -p)）"
echo "SDK:   macOS $(xcrun --sdk macosx --show-sdk-version)（build $(xcrun --sdk macosx --show-sdk-build-version)）"

START=$(date +%s)
(cd "$GHOSTTY_DIR" && "$ZIG" build \
  -Doptimize=ReleaseFast \
  -Demit-macos-app=false \
  -Di18n=false \
  -Dxcframework-target=native)
END=$(date +%s)
echo "构建耗时：$((END - START)) 秒"

# 校验产物
[ -d "$XCFW" ] || fail "未生成 $XCFW"
LIB=$(find "$XCFW" -name '*.a' -type f | head -n 1)
[ -n "$LIB" ] || fail "$XCFW 中没有静态库"
ARCHS=$(xcrun lipo -archs "$LIB")
[ "$ARCHS" = "arm64" ] || fail "架构为「${ARCHS}」，期望 arm64"
HEADER=$(find "$XCFW" -name ghostty.h -type f | head -n 1)
[ -n "$HEADER" ] || fail "$XCFW 中没有 ghostty.h"
grep -q 'ghostty_surface_foreground_pid' "$HEADER" || fail "ghostty.h 中没有 ghostty_surface_foreground_pid"

echo "产物：$XCFW"
echo "静态库：${LIB}（架构 ${ARCHS}，$(du -h "$LIB" | cut -f1)）"
echo "xcframework 总大小：$(du -sh "$XCFW" | cut -f1)"
echo "头文件：${HEADER}（含 ghostty_surface_foreground_pid）"
