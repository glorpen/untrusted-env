use std::collections::LinkedList;
use std::fs::{create_dir, create_dir_all, File, Permissions, set_permissions};
use std::io::Error;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::Path;

use log::info;

mod utils;

const NEW_ROOT_DIR: &str = "new-root";
const OLD_ROOT_DIR: &str = "old-root";
const ROOT: &str = "/tmp";


fn newroot() -> Result<(), Error> {
    let new_root = &*format!("{}/{}", ROOT, NEW_ROOT_DIR);
    let old_root = &*format!("{}/{}", ROOT, OLD_ROOT_DIR);

    utils::mount(Option::None, ROOT, Option::from("tmpfs"), [
        utils::MountFlag::NoSuid,
        utils::MountFlag::NoDev,
        utils::MountFlag::Recursive
    ], None)?;

    create_dir(new_root)?;
    create_dir(old_root)?;

    utils::mount(Option::from(new_root), new_root, Option::from("tmpfs"), [], None)?;
    utils::pivot_root(ROOT, old_root)?;

    return Ok(());
}

fn mount_proc() -> Result<(), Error> {
    // os.mkdir("/newroot/proc")
    // mount(target="/newroot/proc", fs="proc", no_exec=True, no_suid=True, no_dev=True)
    let target = String::from("/") + NEW_ROOT_DIR + "/proc";
    create_dir(target.as_str())?;
    utils::mount(Option::None, target.as_str(), Option::from("proc"), [
        utils::MountFlag::NoSuid,
        utils::MountFlag::NoDev,
        utils::MountFlag::NoExec
    ], None).expect("mounting proc fs");

    return Ok(());
}

fn assure_root_path5<F>(source_root: &str, target_root: &str, source_path: &str, target_path: Option<&str>, then: F) -> Result<(), Error>
    where F: FnOnce(&str, &str) -> Result<(), Error> {
    let local_source_path = String::from(source_root) + source_path;
    let local_target_path = String::from(target_root) + target_path.unwrap_or(source_path);

    let source = Path::new(local_source_path.as_str());

    if !source.exists() {
        info!("Source path {} does not exist, skipping.", local_source_path);
        return Ok(());
    }

    if source.is_dir() {
        info!("Creating dir {}.", local_target_path);
        create_dir_all(local_target_path.as_str())?;
        // set_permissions(path, Permissions::from_mode(0o755))?;
    } else {
        info!("Creating file {}.", local_target_path);
        create_dir_all(Path::new(local_target_path.as_str()).parent().unwrap().to_str().unwrap())?;
        File::create(local_target_path.as_str())?;
    }

    return then(local_source_path.as_str(), local_target_path.as_str());
}

macro_rules! assure_root_path {
    ($a1 : expr, $a2 : expr, $a3 : expr, $a4 : expr) => { assure_root_path5($a1, $a2, $a3, None, $a4) };
    ($a1 : expr, $a2 : expr, $a3 : expr, $a4 : expr, $a5 : expr) => { assure_root_path5($a1, $a2, $a3, $a4, $a5) };
}

fn mount_dev() -> Result<(), Error> {
    let target_dev = String::from("/") + NEW_ROOT_DIR + "/dev";
    let source_dev = String::from("/") + OLD_ROOT_DIR + "/dev";
    let target_pts = target_dev.to_owned() + "/pts";
    let target_shm = target_dev.to_owned() + "/shm";

    fn create_dev_dir(path: &str) -> Result<(), Error> {
        create_dir(path)?;
        set_permissions(path, Permissions::from_mode(0o755))?;
        return Ok(());
    }

    create_dev_dir(target_dev.as_str())?;
    create_dev_dir(target_shm.as_str())?;

    create_dev_dir(target_pts.as_str())?;
    utils::mount(None, target_pts.as_str(), Option::from("devpts"), [
        utils::MountFlag::NoSuid,
        utils::MountFlag::NoExec
    ], Option::Some("newinstance,ptmxmode=0666,mode=620"))?;

    // we cannot mknod as we don't have CAP_MKNOD, mount needed dirs/devices
    for name in ["dri", "null", "zero", "full", "random", "urandom", "tty"] {
        assure_root_path!(source_dev.as_str(), target_dev.as_str(), name, |source, target| {
            utils::mount(Option::from(source), target, None, [
                utils::MountFlag::Recursive,
                utils::MountFlag::Silent,
            ], None)?;

            return Ok(());
        })?;
    }

    return Ok(());
}

fn mount_host(host_path: &str, container_path: Option<&str>, readonly: bool) -> Result<(), Error> {
    // let real_host_path = get_canonical_path([OLD_ROOT_DIR, host_path]);
    // let local_container_path = String::from("/") + NEW_ROOT_DIR + container_path.unwrap_or(host_path);

    let source_root = String::from("/") + OLD_ROOT_DIR;
    let target_root = String::from("/") + NEW_ROOT_DIR;

    let mut mount_flags = LinkedList::from([
        utils::MountFlag::Recursive,
        utils::MountFlag::Silent,
        utils::MountFlag::NoDev
    ]);
    if readonly {
        mount_flags.push_back(utils::MountFlag::Readonly);
    }

    return assure_root_path!(source_root.as_str(), target_root.as_str(), host_path, container_path, |source, target| {
        utils::mount(Option::from(source), target, Option::None, mount_flags, None)?;
        return Ok(());
    });
}

fn cleanup_newroot() -> Result<(), Error> {
//     remove oldroot and make newroot a root
// # make old root private so it does umount not propagate
    let old_root = format!("/{}", OLD_ROOT_DIR);
    let new_root = format!("/{}", NEW_ROOT_DIR);

    utils::mount(Option::None, old_root.as_str(), Option::None, [
        utils::MountFlag::Silent,
        utils::MountFlag::Recursive,
        utils::MountFlag::Private
    ], None)?;
    utils::umount2(old_root.as_str(), [
        utils::UmountFlag::Detach
    ])?;

    let current_root = File::open("/")?.into_raw_fd();
    utils::pivot_root(new_root.as_str(), new_root.as_str())?;

    nix::unistd::fchdir(current_root)?;
    nix::mount::umount2(".", nix::mount::MntFlags::MNT_DETACH)?;
    nix::unistd::chdir("/")?;

    return Ok(());
}

fn child() {
    newroot().unwrap();

    mount_proc().unwrap();
    mount_dev().unwrap();

    for name in [
        "/lib", "/lib32", "/lib64", "/bin", "/usr",
        "/etc/ld.so.conf", "/etc/ld.so.cache", "/etc/ld.so.conf.d",
        "/etc/bash", "/etc/bash_completion.d"
    ] {
        info!("creating {}", name);
        mount_host(name, Option::None, true).unwrap();
    }

    cleanup_newroot().unwrap();

    std::process::Command::new("bash").spawn().unwrap().wait().unwrap();
}

fn main() {
    stderrlog::new().module(module_path!()).verbosity(4).init().unwrap();

    let (uid, gid) = utils::get_uid_gid();

    utils::unshare([
        utils::Namespace::Pid,
        utils::Namespace::Mount,
        utils::Namespace::User,
        utils::Namespace::Network,
        utils::Namespace::Uts,
        utils::Namespace::Ipc,
    ]).expect("unshare failed");

    utils::write_uid_gid_map(uid, gid).expect("setting uid/gid map failed");

    let child_pid = utils::fork(child).expect("could not fork");
    nix::sys::wait::waitpid(child_pid, None).unwrap();
}
