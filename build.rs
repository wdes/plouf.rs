//! Captures a build-identity string at compile time and exposes it as
//! `BUILD_HASH` via `env!`, so `plouf-rs --version` prints the exact build.
//! Identity is `git rev-parse --short=12 HEAD`, else a `t<unix-secs>` compile
//! timestamp (source-tarball builds with no `.git/`), else `"unknown"`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let identity = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| format!("t{}", d.as_secs())))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=BUILD_HASH={identity}");
    // Re-run when HEAD moves (commits + branch switches both append here).
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    println!("cargo:rerun-if-env-changed=BUILD_HASH");
}
