use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = Path::new("resources/resources.gresource.xml");
    let bundle = out_dir.join("lan-mouse.gresource");

    // Compile the bundle ourselves instead of via `glib_build_tools`.
    //
    // Its helper caches on the manifest alone, so adding or editing an icon
    // silently kept the previously compiled bundle — which is why icons added
    // later never appeared in the shipped binary. Running
    // `glib-compile-resources` on every build is cheap (a few milliseconds) and
    // always correct.
    let status = Command::new("glib-compile-resources")
        .arg(manifest)
        .arg("--target")
        .arg(&bundle)
        .arg("--sourcedir")
        .arg("resources")
        .status()
        .expect("failed to run glib-compile-resources");
    assert!(status.success(), "glib-compile-resources failed");

    println!("cargo::rerun-if-changed=resources");
}
