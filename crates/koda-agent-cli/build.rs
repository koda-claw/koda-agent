fn main() {
    // Windows executables get a 1 MiB main-thread stack by default. The clap
    // command graph is intentionally broad, so give the CLI the same headroom
    // CI test threads have and avoid startup overflows on windows-latest.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bin=koda-agent=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bin=koda-agent=-Wl,--stack,8388608");
        }
    }
}
