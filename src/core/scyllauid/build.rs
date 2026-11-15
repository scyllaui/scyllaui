// 链接 src/scripts/build-ghostty-vt.sh 构建的 libghostty-vt 静态库（固定 Ghostty commit，不入库）。
// M3 起 scyllauid 持有保活终端的屏幕（src/pty/vt.rs）；做法同 S0 的 keepd/build.rs。
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib_dir = manifest.join("../../../output/ghostty-vt/lib");
    let lib = lib_dir.join("libghostty-vt.a");
    if !lib.exists() {
        panic!(
            "缺少 {}，先运行 src/scripts/build-ghostty-vt.sh",
            lib.display()
        );
    }
    println!("cargo:rerun-if-changed={}", lib.display());
    // 同目录还有 libghostty-vt.dylib，链接器会优先选它；只把 .a 拷进 OUT_DIR 再链接，保证静态链接。
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::copy(&lib, out.join("libghostty-vt.a")).expect("拷贝 libghostty-vt.a 失败");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}
