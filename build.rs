use std::{env, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    })
}

fn main() {
    // A deliberately absent input rechecks commits and tracked/untracked edits on
    // every build, including clean reversions and changes to packed worktree refs.
    let out = env::var("OUT_DIR").expect("Cargo supplies OUT_DIR");
    println!("cargo:rerun-if-changed={out}/always-check-build-identity");

    let root = env::var("CARGO_MANIFEST_DIR").expect("Cargo supplies CARGO_MANIFEST_DIR");
    let root = Path::new(&root);
    let mut revision = "unknown".to_owned();
    let mut state = "unknown";
    // Do not inherit the enclosing repository's identity for an unpacked archive.
    if root.join(".git").exists() {
        if let Some(head) = git(root, &["rev-parse", "--verify", "HEAD"])
            && matches!(head.len(), 40 | 64)
            && head.bytes().all(|b| b.is_ascii_hexdigit())
        {
            revision = head;
        }
        if let Some(status) = git(root, &["status", "--porcelain", "--untracked-files=normal"]) {
            state = if status.is_empty() { "clean" } else { "dirty" };
        }
    }
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo supplies CARGO_PKG_VERSION");
    println!(
        "cargo:rustc-env=FAILTRACE_BUILD_VERSION={version} (product=failtrace-rust revision={revision} state={state})"
    );
}
