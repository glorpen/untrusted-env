use std::collections::LinkedList;
use std::fs::{create_dir, create_dir_all, File, Permissions, set_permissions};
use std::io::Error;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::Path;

use log::info;

mod sys;
mod rootfs;


fn main() {
    stderrlog::new().module(module_path!()).verbosity(4).init().unwrap();
    rootfs::run().unwrap();
}
