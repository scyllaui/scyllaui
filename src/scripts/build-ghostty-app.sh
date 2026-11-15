#!/bin/sh
# 用固定 commit 构建对照基线 Ghostty.app，同时产出 ScyllaUI 打包用的 terminfo 与 shell 集成脚本。
# 用法：src/scripts/build-ghostty-app.sh（先运行 fetch-ghostty.sh）
# 参数与 build-ghosttykit.sh 相同，只是不加 -Demit-macos-app=false，由 Ghostty 自己的 build.zig 调用 xcodebuild。
# 产物：vendor/ghostty/zig-out/Ghostty.app、vendor/ghostty/zig-out/share/{ghostty,terminfo}
set -eu

ZIG_VERSION="0.16.0"
GHOSTTY_COMMIT="34f39002c6e3974b54e6a6d400bd83777c5ea558"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
ZIG="$PROJECT_DIR/.tools/zig/$ZIG_VERSION/zig"
GHOSTTY_DIR="$PROJECT_DIR/vendor/ghostty"
APP="$GHOSTTY_DIR/zig-out/Ghostty.app"

fail() { echo "错误：$*" >&2; exit 1; }

[ -x "$ZIG" ] || fail "未找到 $ZIG"
[ "$(git -C "$GHOSTTY_DIR" rev-parse HEAD)" = "$GHOSTTY_COMMIT" ] || fail "vendor/ghostty 不在固定 commit $GHOSTTY_COMMIT"

# Ghostty 的 Xcode 工程固定引用 zig-out/share/locale；-Di18n=false 时不生成该目录。
# 建一个空目录（只在构建产物区，不改源码），两边都无翻译，对照条件一致。
mkdir -p "$GHOSTTY_DIR/zig-out/share/locale"

(cd "$GHOSTTY_DIR" && "$ZIG" build \
  -Doptimize=ReleaseFast \
  -Di18n=false \
  -Dxcframework-target=native)

[ -d "$APP" ] || fail "未生成 $APP"
[ -f "$GHOSTTY_DIR/zig-out/share/terminfo/78/xterm-ghostty" ] || fail "未生成 terminfo"
[ -d "$GHOSTTY_DIR/zig-out/share/ghostty/shell-integration" ] || fail "未生成 shell 集成脚本"
echo "Ghostty.app：$APP"
"$APP/Contents/MacOS/ghostty" +version | head -n 1
