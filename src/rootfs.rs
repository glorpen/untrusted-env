use std::collections::LinkedList;
use std::fs::{create_dir, create_dir_all, File, Permissions, set_permissions};
use std::io::Error;
use std::path::Path;
use log::info;
use crate::sys::{fork, get_uid_gid, mount, MountFlag, Namespace, pivot_root, umount2, UmountFlag, unshare, write_uid_gid_map};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use nix::unistd::{chdir, fchdir};

const NEW_ROOT_DIR: &str = "new-root";
const OLD_ROOT_DIR: &str = "old-root";
const ROOT: &str = "/tmp";


fn newroot() -> Result<(), Error> {
    let new_root = &*format!("{}/{}", ROOT, NEW_ROOT_DIR);
    let old_root = &*format!("{}/{}", ROOT, OLD_ROOT_DIR);

    mount(Option::None, ROOT, Option::from("tmpfs"), [
        MountFlag::NoSuid,
        MountFlag::NoDev,
        MountFlag::Recursive
    ], None)?;

    create_dir(new_root)?;
    create_dir(old_root)?;

    mount(Option::from(new_root), new_root, Option::from("tmpfs"), [], None)?;
    pivot_root(ROOT, old_root)?;

    return Ok(());
}

fn mount_proc() -> Result<(), Error> {
    // os.mkdir("/newroot/proc")
    // mount(target="/newroot/proc", fs="proc", no_exec=True, no_suid=True, no_dev=True)
    let target = String::from("/") + NEW_ROOT_DIR + "/proc";
    create_dir(target.as_str())?;
    mount(Option::None, target.as_str(), Option::from("proc"), [
        MountFlag::NoSuid,
        MountFlag::NoDev,
        MountFlag::NoExec
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
    mount(None, target_pts.as_str(), Option::from("devpts"), [
        MountFlag::NoSuid,
        MountFlag::NoExec
    ], Option::Some("newinstance,ptmxmode=0666,mode=620"))?;

    // we cannot mknod as we don't have CAP_MKNOD, mount needed dirs/devices
    for name in ["dri", "null", "zero", "full", "random", "urandom", "tty"] {
        assure_root_path!(source_dev.as_str(), target_dev.as_str(), name, |source, target| {
            mount(Option::from(source), target, None, [
                MountFlag::Recursive,
                MountFlag::Silent,
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
        MountFlag::Recursive,
        MountFlag::Silent,
        MountFlag::NoDev
    ]);
    if readonly {
        mount_flags.push_back(MountFlag::Readonly);
    }

    return assure_root_path!(source_root.as_str(), target_root.as_str(), host_path, container_path, |source, target| {
        mount(Option::from(source), target, None, mount_flags, None)?;
        return Ok(());
    });
}

fn cleanup_newroot() -> Result<(), Error> {
//     remove oldroot and make newroot a root
// # make old root private so it does umount not propagate
    let old_root = format!("/{}", OLD_ROOT_DIR);
    let new_root = format!("/{}", NEW_ROOT_DIR);

    mount(None, old_root.as_str(), None, [
        MountFlag::Silent,
        MountFlag::Recursive,
        MountFlag::Private
    ], None)?;
    umount2(old_root.as_str(), [
        UmountFlag::Detach
    ])?;

    let current_root = File::open("/")?.into_raw_fd();
    pivot_root(new_root.as_str(), new_root.as_str())?;

    fchdir(current_root)?;
    umount2(".", [UmountFlag::Detach])?;
    chdir("/")?;

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

pub fn run() -> Result<(), Error> {
    let (uid, gid) = get_uid_gid();

    unshare([
        Namespace::Pid,
        Namespace::Mount,
        Namespace::User,
        Namespace::Network,
        Namespace::Uts,
        Namespace::Ipc,
    ])?;

    write_uid_gid_map(uid, gid)?;

    let child_pid = fork(child)?;
    nix::sys::wait::waitpid(child_pid, None).unwrap();

    return Ok(());
}