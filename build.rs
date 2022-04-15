use std::process::Command;
use std::env;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();

    if Command::new("meson")
        .args(&["setup", "--default-library", "static", "--buildtype", "release", &out_dir, "vendor/libslirp/"])
        .status().unwrap().code().unwrap() != 0 {
        panic!("meson failed");
    }

    if Command::new("ninja").args(&["-C", &out_dir])
        .status().unwrap().code().unwrap() != 0 {
        panic!("ninja failed");
    }

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=slirp");
    // println!("cargo:rerun-if-changed=src/hello.c");
}
