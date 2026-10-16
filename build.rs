use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// `core::sync::atomic::AtomicU128` is unstable and only exists on some targets, so
// probe the compiler: when the probe builds, enable the `rcu_cell_atomic_u128` cfg.
const PROBE: &str = r#"
#![no_std]
#![feature(integer_atomics)]
pub static A: core::sync::atomic::AtomicU128 = core::sync::atomic::AtomicU128::new(0);
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(rcu_cell_atomic_u128)");

    if has_atomic_u128() {
        println!("cargo:rustc-cfg=rcu_cell_atomic_u128");
    }
}

fn has_atomic_u128() -> bool {
    let (Some(rustc), Some(out_dir), Some(target)) = (
        env::var_os("RUSTC"),
        env::var_os("OUT_DIR").map(PathBuf::from),
        env::var_os("TARGET"),
    ) else {
        return false;
    };

    let src = out_dir.join("probe_atomic_u128.rs");
    if fs::write(&src, PROBE).is_err() {
        return false;
    }

    // a stable compiler rejects the feature gate, so it never enables the cfg
    Command::new(rustc)
        .arg("--crate-type=lib")
        .arg("--emit=metadata")
        .arg("--target")
        .arg(target)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
