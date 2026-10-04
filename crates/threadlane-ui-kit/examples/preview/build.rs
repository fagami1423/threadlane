fn main() {
    // A private local snapshot is optional. Clean checkouts use the sample.
    println!("cargo:rerun-if-changed=session.local.json");
    println!("cargo:rerun-if-changed=session.sample.json");
    let source = if std::path::Path::new("session.local.json").is_file() {
        "session.local.json"
    } else {
        "session.sample.json"
    };
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::copy(source, output.join("session.json")).expect("bundle preview session");
}
