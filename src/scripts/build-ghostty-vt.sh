#!/bin/sh
# 用固定 commit 的 vendor/ghostty 构建 libghostty-vt 静态库：M3 起 scyllauid 链接它持有保活终端的屏幕。
# 用法：src/scripts/build-ghostty-vt.sh（先运行 fetch-ghostty.sh）
# 产物：output/ghostty-vt/lib/libghostty-vt.a 与 output/ghostty-vt/include（不覆盖 vendor/ghostty/zig-out）
set -eu

ZIG_VERSION="0.16.0"
GHOSTTY_COMMIT="34f39002c6e3974b54e6a6d400bd83777c5ea558"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
ZIG="$PROJECT_DIR/.tools/zig/$ZIG_VERSION/zig"
GHOSTTY_DIR="$PROJECT_DIR/vendor/ghostty"
OUT="$PROJECT_DIR/output/ghostty-vt"

fail() { echo "错误：$*" >&2; exit 1; }

[ -x "$ZIG" ] || fail "未找到 $ZIG"
[ "$("$ZIG" version)" = "$ZIG_VERSION" ] || fail "Zig 版本不是 $ZIG_VERSION"
[ "$(git -C "$GHOSTTY_DIR" rev-parse HEAD)" = "$GHOSTTY_COMMIT" ] || fail "vendor/ghostty 不在固定 commit $GHOSTTY_COMMIT"

(cd "$GHOSTTY_DIR" && "$ZIG" build -Demit-lib-vt=true -Doptimize=ReleaseFast -p "$OUT")

[ -f "$OUT/lib/libghostty-vt.a" ] || fail "未生成 $OUT/lib/libghostty-vt.a"
[ -f "$OUT/include/ghostty/vt/formatter.h" ] || fail "未生成头文件"
nm -g "$OUT/lib/libghostty-vt.a" 2>/dev/null | grep -q ' T _ghostty_terminal_continuation_buf$' || fail "静态库缺少 ghostty_terminal_continuation_buf"
echo "libghostty-vt：$OUT/lib/libghostty-vt.a（$(du -h "$OUT/lib/libghostty-vt.a" | cut -f1)）"
