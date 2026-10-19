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

    println!("cargo:rustc-check-cfg=cfg(rcu_cell_wide)");
    println!("cargo:rustc-check-cfg=cfg(rcu_cell_atomic_u128)");
    println!("cargo:rustc-check-cfg=cfg(rcu_cell_cx16)");

    // `rcu_cell_wide`: a lock free 128-bit atomic is available, either the core one
    // (`rcu_cell_atomic_u128`) or our own `cmpxchg16b` one (`rcu_cell_cx16`)
    if has_atomic_u128() {
        println!("cargo:rustc-cfg=rcu_cell_atomic_u128");
        println!("cargo:rustc-cfg=rcu_cell_wide");
    } else if has_cx16() {
        println!("cargo:rustc-cfg=rcu_cell_cx16");
        println!("cargo:rustc-cfg=rcu_cell_wide");
    }
}

// our own atomic is not usable under Miri
fn has_cx16() -> bool {
    env::var_os("CARGO_CFG_MIRI").is_none()
        && env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "x86_64")
        && (env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|f| f.split(',').any(|f| f == "cmpxchg16b"))
            || has_host_cx16())
}

// `cmpxchg16b` is not part of the baseline x86_64 target, so enable it only when the
// machine doing the build supports it and it is also the target. The binary then needs
// a cpu with `cmpxchg16b` (every x86_64 cpu since ~2006 has it).
#[cfg(target_arch = "x86_64")]
fn has_host_cx16() -> bool {
    env::var_os("HOST") == env::var_os("TARGET")
        && std::arch::is_x86_feature_detected!("cmpxchg16b")
}

#[cfg(not(target_arch = "x86_64"))]
fn has_host_cx16() -> bool {
    false
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
