extern crate core;


mod sys;
mod rootfs;
mod path;
mod ipc;

fn main() {
    stderrlog::new().module(module_path!()).verbosity(4).init().unwrap();
    rootfs::run().unwrap();
}
