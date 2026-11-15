#!/bin/sh
# 用项目局部 Rust 工具链执行命令，不改调用方环境，也不读写 ~/.cargo、~/.rustup。
# 用法：src/scripts/with-rust.sh {cargo|rustc|rustup|rustfmt} [参数...]
set -eu

RUST_VERSION="1.99.0"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
export CARGO_HOME="$PROJECT_DIR/.tools/rust/cargo"
export RUSTUP_HOME="$PROJECT_DIR/.tools/rust/rustup"
export RUSTUP_TOOLCHAIN="$RUST_VERSION-aarch64-apple-darwin"
export RUSTUP_AUTO_INSTALL=0
export PATH="$CARGO_HOME/bin:$PATH"

case "${1:-}" in
  cargo|rustc|rustup|rustfmt) tool=$1; shift ;;
  *) echo "用法：$0 {cargo|rustc|rustup|rustfmt} [参数...]" >&2; exit 2 ;;
esac
[ -x "$CARGO_HOME/bin/$tool" ] || { echo "错误：未安装项目局部 Rust（${CARGO_HOME}）" >&2; exit 1; }
exec "$CARGO_HOME/bin/$tool" "$@"
