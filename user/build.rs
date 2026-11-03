//! Links the program at the user base address with `link.ld`, aligning
//! segments to 4 KiB pages rather than lld's default 64 KiB (which would
//! pad the file).

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{}/link.ld", dir);
    println!("cargo:rustc-link-arg=-zmax-page-size=4096");
    println!("cargo:rerun-if-changed=link.ld");
}
