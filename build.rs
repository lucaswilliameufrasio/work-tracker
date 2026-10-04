fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=migrations");
    assert!(
        std::path::Path::new("web/dist/index.html").is_file(),
        "Build the SPA first: pnpm --dir web install --frozen-lockfile && pnpm --dir web build"
    );
}
