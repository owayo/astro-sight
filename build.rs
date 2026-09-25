//! Windows の実行ファイルのメインスレッドのスタックを、Linux / macOS と同じ 8MB にする。
//!
//! Windows のメインスレッドのスタックはリンカの既定で 1MB しかなく、Linux / macOS (8MB) より
//! 小さい。debug ビルドはスタックフレームが大きいため、1MB では `astro-sight doctor` ですら
//! 起動直後に "thread 'main' has overflowed its stack" で落ちる (CI の Windows で結合テストを
//! 回して判明。macOS でも `ulimit -s 1024` で再現し、2MB あれば通る)。release でも AST を
//! 再帰でたどる処理は入れ子の深いソースでスタックを使うので、どの OS でも同じ大きさにそろえる。
//! スタックは予約するだけで、使った分しかメモリを消費しない。

/// Linux / macOS のメインスレッドの既定のスタックの大きさ。
const MAIN_THREAD_STACK_BYTES: u32 = 8 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => println!("cargo:rustc-link-arg-bins=/STACK:{MAIN_THREAD_STACK_BYTES}"),
        Ok("gnu") => println!("cargo:rustc-link-arg-bins=-Wl,--stack,{MAIN_THREAD_STACK_BYTES}"),
        _ => {}
    }
}
