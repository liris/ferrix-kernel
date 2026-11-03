//! Builds the user program (`user/`) for `aarch64-unknown-none`, so that
//! `include_bytes!` can embed its ELF in the kernel.
//!
//! It gets its own target directory: the outer build holds the lock on ours.
//! Flags meant for the kernel's target must not leak into it.

use std::env;
use std::process::Command;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let target_dir = format!("{}/target/user", dir);
    let status = Command::new(env::var("CARGO").unwrap())
        .args(["build", "--release", "--target", "aarch64-unknown-none"])
        .args(["--manifest-path", &format!("{}/user/Cargo.toml", dir)])
        .args(["--target-dir", &target_dir])
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .status()
        .expect("failed to run cargo for the user program");
    assert!(status.success(), "building the user program failed");
    println!("cargo:rustc-env=USER_ELF={}/aarch64-unknown-none/release/ferrix-user", target_dir);
    println!("cargo:rerun-if-changed=user/src");
    println!("cargo:rerun-if-changed=user/Cargo.toml");
    println!("cargo:rerun-if-changed=user/build.rs");
    println!("cargo:rerun-if-changed=user/link.ld");
}
