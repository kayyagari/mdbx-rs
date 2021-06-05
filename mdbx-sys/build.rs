#[cfg(feature = "bindgen")]
#[path = "bindgen.rs"]
mod generate;

use cmake::Config;

fn main() {
    #[cfg(feature = "bindgen")]
    generate::generate();

    let mut dst = Config::new("libmdbx")
        .define("MDBX_INSTALL_STATIC", "ON")
        .define("MDBX_TXN_CHECKOWNER", "OFF")
        .build();
    dst.push("lib");
    println!("cargo:rustc-link-search=native={}", dst.display());
    println!("cargo:rustc-link-lib=static=mdbx-static");
}
