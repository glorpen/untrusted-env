mod sys;
mod rootfs;
mod path;


fn main() {
    stderrlog::new().module(module_path!()).verbosity(4).init().unwrap();
    rootfs::run().unwrap();
}
