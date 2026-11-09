use std::path::PathBuf;

fn main() {
    let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard/dist/index.html");

    println!("cargo:rerun-if-changed={}", bundle.display());
    println!("cargo:rerun-if-changed=build.rs");

    if !bundle.is_file() {
        panic!(
            "the admin dashboard has not been built: {} is missing.\n\
             Run ./scripts/build-dashboard.sh (requires bun, https://bun.sh)",
            bundle.display()
        );
    }
}
