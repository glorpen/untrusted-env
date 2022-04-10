use std::fmt::Debug;
use std::fs::File;
use std::io::{Error, Write};

use libc::{getgid, getuid, gid_t, uid_t};
use log::info;
use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags};
use nix::sched::CloneFlags;
use nix::unistd::{ForkResult, Pid};
use strum_macros::Display;

pub enum UmountFlag {
    Detach
}

pub fn pivot_root(new_root: &str, old_root: &str) -> nix::Result<()> {
    info!("Changing root to {} and moving current one to {}", new_root, old_root);
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

pub fn mount(source: Option<&str>, target: &str, fs: Option<&str>, flags: impl IntoIterator<Item=MountFlag>, data: Option<&str>) -> nix::Result<()> {
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

pub fn write_uid_gid_map(uid: uid_t, gid: gid_t) -> Result<(), Error> {
    {
        let f = File::options().write(true).open("/proc/self/setgroups");
        if f.is_ok() {
            f.unwrap().write_all("deny\n".as_ref())?;
        }
    }

    for (name, id) in [("uid_map", uid), ("gid_map", gid)] {
        let mut map = File::options().write(true).open(format!("/proc/self/{}", name))?;
        map.write_all(format!("{} {} 1\n", id, id).as_ref())?;
    }

    return Ok(());
}

pub fn get_uid_gid() -> (uid_t, gid_t) {
    unsafe {
        return (getuid(), getgid());
    }
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
