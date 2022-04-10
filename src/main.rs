use std::borrow::Borrow;
use std::collections::LinkedList;
use std::fs::{create_dir, create_dir_all, File, Permissions, set_permissions};
use std::io::Error;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::{Path, PathBuf};

use log::info;
use nix::sys::stat::{Mode, SFlag};

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

fn get_canonical_path<'x>(items: impl IntoIterator<Item=&'x str>) -> String {
    let mut path = PathBuf::from("/");
    path.extend(items.into_iter().map(|p: &str| {
        return p.strip_prefix("/").unwrap_or(p);
    }));
    return String::from(path.canonicalize().unwrap().to_str().unwrap());
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

    for name in ["dri", "null", "zero", "full", "random", "urandom", "tty"] {
        let source = format!("{}/{}", source_dev, name);
        let source_path = Path::new(source.as_str());

        if ! source_path.exists() {
            continue;
        }
        let target = format!("{}/{}", target_dev, name);

        if source_path.is_dir() {
            create_dev_dir(target.as_str())?;
        } else {
            File::create(target.as_str())?;
        }
        utils::mount(Option::from(source.as_str()), target.as_str(), None, [
            utils::MountFlag::Recursive,
            utils::MountFlag::Silent,
        ], None)?;
    }

    return Ok(());
}

fn mount_host(host_path: &str, container_path: Option<&str>, readonly: bool) -> Result<(), Error> {
    let real_host_path = get_canonical_path([OLD_ROOT_DIR, host_path]);
    let local_container_path = String::from("/") + NEW_ROOT_DIR + container_path.unwrap_or(host_path);

    if Path::new(real_host_path.as_str()).is_dir() {
        info!("Creating dir {}", local_container_path);
        create_dir_all(local_container_path.as_str())?;
    } else {
        info!("Creating file {}", local_container_path);
        create_dir_all(Path::new(local_container_path.as_str()).parent().unwrap().to_str().unwrap())?;
        File::create(local_container_path.as_str())?;
    }

    let mut mount_flags = LinkedList::from([
        utils::MountFlag::Recursive,
        utils::MountFlag::Silent,
        utils::MountFlag::NoDev
    ]);
    if readonly {
        mount_flags.push_back(utils::MountFlag::Readonly);
    }
    utils::mount(Option::from(real_host_path.as_str()), local_container_path.as_str(), Option::None, mount_flags, None)?;

    return Ok(());
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
