use std::collections::LinkedList;
use std::fs::{create_dir, File, Permissions, set_permissions};
use std::io::Error;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::PathBuf;

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
    ])?;

    create_dir(new_root)?;
    create_dir(old_root)?;

    utils::mount(Option::from(new_root), new_root, Option::from("tmpfs"), [])?;
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
    ]).expect("mounting proc fs");

    return Ok(());
}

fn mount_host(host_path: &str, container_path: Option<&str>, readonly: bool) -> Result<(), Error> {
    let real_host_path = get_canonical_path([OLD_ROOT_DIR, host_path]);
    let local_container_path = String::from("/") + NEW_ROOT_DIR + container_path.unwrap_or(host_path);

    info!("Creating dir {}", local_container_path);
    create_dir(local_container_path.as_str())?;
    set_permissions(local_container_path.as_str(), Permissions::from_mode(0o755))?;

    let mut mount_flags = LinkedList::from([
        utils::MountFlag::Recursive,
        utils::MountFlag::Silent
    ]);
    if readonly {
        mount_flags.push_back(utils::MountFlag::Readonly);
    }
    utils::mount(Option::from(real_host_path.as_str()), local_container_path.as_str(), Option::None, mount_flags)
        .expect("mounting host path");

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
    ])?;
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

    mount_host("/lib", Option::None, true).unwrap();
    mount_host("/lib32", Option::None, true).unwrap();
    mount_host("/lib64", Option::None, true).unwrap();
    mount_host("/bin", Option::None, true).unwrap();
    mount_host("/usr", Option::None, true).unwrap();

    mount_proc().unwrap();

    cleanup_newroot().unwrap();

    std::process::Command::new("/bin/ps").args(["aux"]).spawn().unwrap().wait().unwrap();
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
