use std::{env, path::PathBuf, process::Command};

const BLUEPRINTS: &[&str] = &[
    "browser-page.blp",
    "repo-dialog.blp",
    "repo-sidebar.blp",
    "restore-dialog.blp",
    "snapshots-page.blp",
    "unlock-dialog.blp",
    "window.blp",
];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ui_src = PathBuf::from("data/resources/ui");
    let ui_out = out_dir.join("ui");

    for blp in BLUEPRINTS {
        println!("cargo::rerun-if-changed={}", ui_src.join(blp).display());
    }

    // Compile .blp to .ui into OUT_DIR, the gresource bundle picks them up from there.
    let status = Command::new("blueprint-compiler")
        .arg("batch-compile")
        .arg(&ui_out)
        .arg(&ui_src)
        .args(BLUEPRINTS.iter().map(|blp| ui_src.join(blp)))
        .status()
        .expect("failed to run blueprint-compiler (is it in PATH?)");
    assert!(status.success(), "blueprint-compiler failed");

    glib_build_tools::compile_resources(
        &[out_dir.as_path(), "data/resources".as_ref()],
        "data/resources/resources.gresource.xml",
        "resources.gresource",
    );
}
