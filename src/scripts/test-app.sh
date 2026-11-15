#!/bin/sh
# App 侧单测（ScyllaUITests：侧栏模型与差分）。用法：src/scripts/test-app.sh
# 不用 `xcodebuild test`：它经 testmanagerd 拉起的 xctest 读不到「文稿」下的测试包
# （2026-10-10 统一日志：tccd 对 com.apple.xctest 的 kTCCServiceSystemPolicyDocumentsFolder 判定为 Deny），
# 所以先 build-for-testing，再由当前 shell 直接运行 xctest。
set -eu

XCODEGEN_VERSION="2.46.0"
PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
XCODEGEN="$PROJECT_DIR/.tools/xcodegen/$XCODEGEN_VERSION/bin/xcodegen"
APP_DIR="$PROJECT_DIR/src/app"
DERIVED="$PROJECT_DIR/output/build-tests"

[ -x "$XCODEGEN" ] || { echo "错误：未找到 $XCODEGEN" >&2; exit 1; }
"$XCODEGEN" generate --spec "$APP_DIR/project.yml" --project "$APP_DIR" --quiet
xcodebuild -project "$APP_DIR/ScyllaUI.xcodeproj" -scheme ScyllaUITests -derivedDataPath "$DERIVED" \
  build-for-testing | grep -E "^/.*(error|warning): |TEST BUILD (SUCCEEDED|FAILED)" || true
BUNDLE="$DERIVED/Build/Products/Debug/ScyllaUITests.xctest"
[ -d "$BUNDLE" ] || { echo "错误：构建失败，未生成 $BUNDLE" >&2; exit 1; }
xcrun xctest "$BUNDLE"
