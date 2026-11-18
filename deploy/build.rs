//! The μCore commit this helper is built from, for it to download the same
//! source to build (see SOURCE_REV in main.rs).

fn main() {
    // HEAD names the branch; the reflog is what changes with every commit.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/logs/HEAD");
    println!("cargo:rerun-if-env-changed=UCORE_DEPLOY_REV");
    let rev = std::env::var("UCORE_DEPLOY_REV").ok().filter(|r| !r.is_empty()).or_else(|| {
        let out = std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir("..").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=UCORE_DEPLOY_REV={}", rev.unwrap_or_else(|| "main".into()));
}
