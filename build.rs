fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=migrations");
    assert!(
        std::path::Path::new("web/dist/index.html").is_file(),
        "Build the SPA first: make install && make web"
    );
}
