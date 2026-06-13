use std::path::PathBuf;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let canonical: PathBuf = [&manifest_dir, "..", "app", "__shell", "worklist-guard.py"]
        .iter()
        .collect();
    let installed: PathBuf = [&manifest_dir, "..", ".claude", "hooks", "worklist-guard.py"]
        .iter()
        .collect();

    if !canonical.exists() {
        panic!(
            "worklist-guard canonical source not found at {}; refusing to sync the installed hook from a stale or missing source",
            canonical.display()
        );
    }
    std::fs::copy(&canonical, &installed).unwrap_or_else(|e| {
        panic!(
            "failed to sync worklist-guard from {} to {}: {}",
            canonical.display(),
            installed.display(),
            e
        )
    });

    println!("cargo:rerun-if-changed=../app/__shell/worklist-guard.py");

    tauri_build::build()
}
