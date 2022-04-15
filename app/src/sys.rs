use std::fmt::Debug;
use std::fs::File;
use std::io::{Error, Write};
use std::path::{Path, PathBuf};

use log::info;
use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags};
use nix::sched::CloneFlags;
use nix::unistd::{ForkResult, Gid, Pid, Uid};
use strum_macros::Display;

pub enum UmountFlag {
    Detach
}

pub fn pivot_root(new_root: &Path, old_root: &Path) -> nix::Result<()> {
    info!("Changing root to {:?} and moving current one to {:?}", new_root, old_root);
    return nix::unistd::pivot_root(new_root, old_root);
}

pub fn umount2(path: &str, flags: impl IntoIterator<Item=UmountFlag>) -> nix::Result<()> {
    let combined_flags = flags.into_iter().fold(MntFlags::empty(), |acc, flag: UmountFlag| {
        return acc | match flag { UmountFlag::Detach => MntFlags::MNT_DETACH };
    });

    info!("Unmounting {}", path);

    return nix::mount::umount2(path, combined_flags);
}

#[derive(Display, Debug)]
pub enum MountFlag {
    Readonly,
    NoExec,
    NoSuid,
    NoDev,
    Recursive,
    Silent,
    Private,
}

pub fn mount(source: Option<&Path>, target: &Path, fs: Option<&str>, flags: impl IntoIterator<Item=MountFlag>, data: Option<&str>) -> nix::Result<()> {
    fn as_mount_flag(flag: MountFlag) -> MsFlags {
        return match flag {
            MountFlag::Readonly => MsFlags::MS_RDONLY,
            MountFlag::NoExec => MsFlags::MS_NOEXEC,
            MountFlag::NoSuid => MsFlags::MS_NOSUID,
            MountFlag::NoDev => MsFlags::MS_NODEV,
            MountFlag::Recursive => MsFlags::MS_REC,
            MountFlag::Silent => MsFlags::MS_SILENT,
            MountFlag::Private => MsFlags::MS_PRIVATE
        };
    }

    let mut combined_flags: MsFlags = flags.into_iter().fold(MsFlags::empty(), |acc, f: MountFlag| {
        return acc | as_mount_flag(f);
    });

    if fs.is_none() {
        combined_flags |= MsFlags::MS_BIND;
    }

    let local_source = source.unwrap_or(target);

    log::info!("Mounting {:?} to {:?} with fs:{:?}", local_source, target, fs);

    return nix::mount::mount(
        Option::from(local_source),
        target,
        fs,
        combined_flags,
        data,
    );
}

pub fn write_uid_gid_map(uid: Uid, gid: Gid) -> Result<(), Error> {
    {
        let f = File::options().write(true).open("/proc/self/setgroups");
        if f.is_ok() {
            f.unwrap().write_all("deny\n".as_bytes())?;
        }
    }

    File::options().write(true).open("/proc/self/uid_map")?
        .write_all(format!("{} {} 1\n", uid, uid).as_bytes())?;

    File::options().write(true).open("/proc/self/gid_map")?
        .write_all(format!("{} {} 1\n", gid, gid).as_bytes())?;

    return Ok(());
}

pub struct Group {
    pub id: Gid,
    pub name: String,
}

pub struct User {
    pub id: Uid,
    pub name: String,
}

pub struct UserInfo {
    pub user: User,
    pub group: Group,
    pub shell: PathBuf,
}

pub fn get_user_info() -> Result<UserInfo, Error> {
    let uid = nix::unistd::getuid();
    let user = nix::unistd::User::from_uid(uid)?
        .expect("Current user does not exist");
    let group = nix::unistd::Group::from_gid(user.gid)?
        .expect("Current user group does not exist");

    return Ok(UserInfo {
        shell: user.shell,
        user: User {
            id: uid,
            name: user.name,
        },
        group: Group {
            id: group.gid,
            name: group.name,
        },
    });
}

pub enum Namespace {
    Pid,
    Mount,
    User,
    Network,
    // separate hostname
    Uts,
    Ipc,
    // CLONE_NEWTIME
}


pub fn unshare(namespaces: impl IntoIterator<Item=Namespace>) -> nix::Result<()> {
    fn as_clone_flag(namespace: Namespace) -> CloneFlags {
        return match namespace {
            Namespace::Pid => CloneFlags::CLONE_NEWPID,
            Namespace::Mount => CloneFlags::CLONE_NEWNS,
            Namespace::User => CloneFlags::CLONE_NEWUSER,
            Namespace::Network => CloneFlags::CLONE_NEWNET,
            Namespace::Uts => CloneFlags::CLONE_NEWUTS,
            Namespace::Ipc => CloneFlags::CLONE_NEWIPC
        };
    }
    info!("Unsharing");
    return nix::sched::unshare(namespaces.into_iter()
        .fold(CloneFlags::empty(), |acc, namespace: Namespace| {
            return acc | as_clone_flag(namespace);
        }));
}

pub fn fork(child_body: impl FnOnce()) -> Result<Pid, Errno> {
    match unsafe { nix::unistd::fork() } {
        Ok(ForkResult::Child) => {
            child_body();
            std::process::exit(0);
        }
        Ok(ForkResult::Parent { child, .. }) => {
            return Ok(child);
        }
        Err(err) => {
            return Err(err);
        }
    }
}
