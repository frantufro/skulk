fn main() {
    // Cargo sets TARGET for build scripts; re-export it so the crate can read
    // it at compile time via `env!("TARGET")`.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=TARGET={target}");
}
