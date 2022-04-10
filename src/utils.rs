use std::ffi::{CStr, CString};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::fs::File;
use std::io::{Error, Seek, Write};
use std::ops::Add;
use std::path::Path;
use strum_macros::Display;

use libc::{c_char, CLONE_NEWIPC, CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWUSER, CLONE_NEWUTS, getgid, getuid, gid_t, MNT_DETACH, MS_BIND, MS_NODEV, MS_NOEXEC, MS_NOSUID, MS_PRIVATE, MS_RDONLY, MS_REC, MS_SILENT, pid_t, uid_t};
use log::info;

mod ffi {
    use libc::{c_char, c_int};

    extern {
        pub fn pivot_root(new_root: *const c_char, put_old: *const c_char) -> c_int;
    }
}

macro_rules! make_c_string {
    ($str:expr) => {
        CString::new($str).expect("new string")
    };
}

macro_rules! make_c_path {
    ($path:expr) => {
        make_c_string!($path.to_str().expect("path to string"))
    };
}

pub enum UmountFlag {
    Detach
}

pub fn pivot_root(new_root: &str, old_root: &str) -> Result<(), Error> {
    let c_new_root = make_c_string!(new_root);
    let c_old_root = make_c_string!(old_root);

    if unsafe {
        ffi::pivot_root(
            c_new_root.as_ptr(),
            c_old_root.as_ptr(),
        )
    } == -1 {
        return Err(Error::last_os_error());
    }

    return Ok(());
}

pub fn umount2(path: &str, flags: impl IntoIterator<Item=UmountFlag>) -> Result<(), Error> {
    let combined_flags = flags.into_iter().fold(0, |acc, flag: UmountFlag| {
        return acc | match flag { UmountFlag::Detach => MNT_DETACH };
    });
    let c_path = make_c_string!(path);

    info!("Unmounting {:?}", c_path);

    if unsafe { libc::umount2(c_path.as_ptr(), combined_flags) } == -1 {
        return Err(Error::last_os_error());
    }
    return Ok(());
}

// #[derive(strum_macros::EnumString)]
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

pub fn mount(source: Option<&str>, target: &str, fs: Option<&str>, flags: impl IntoIterator<Item=MountFlag>) -> Result<(), Error> {
    let mut combined_flags: nix::mount::MsFlags = nix::mount::MsFlags::empty();
    let mut combined_names = String::new();

    if fs.is_none() {
        combined_flags |= nix::mount::MsFlags::MS_BIND;
        combined_names += &*format!(", Bind");
    }

    for flag in flags {
        combined_flags |= match flag {
            MountFlag::Readonly => nix::mount::MsFlags::MS_RDONLY,
            MountFlag::NoExec => nix::mount::MsFlags::MS_NOEXEC,
            MountFlag::NoSuid => nix::mount::MsFlags::MS_NOSUID,
            MountFlag::NoDev => nix::mount::MsFlags::MS_NODEV,
            MountFlag::Recursive => nix::mount::MsFlags::MS_REC,
            MountFlag::Silent => nix::mount::MsFlags::MS_SILENT,
            MountFlag::Private => nix::mount::MsFlags::MS_PRIVATE
        };

        combined_names += &*format!(", {:?}", flag);
    }

    let c_src = make_c_string!(source.unwrap_or(target));
    let c_target = make_c_string!(target);
    let c_fs = fs.map_or(CString::default(), |t| { return make_c_string!(t) });

    log::info!("Mounting {:?} to {:?} with fs:{:?} and flags:[{}]",
        c_src,
        c_target,
        c_fs,
        combined_names.strip_prefix(", ").unwrap_or("")
    );

    nix::mount::mount(Option::from(source.unwrap_or(target)), target, fs, combined_flags, None::<&Path>)?;

    // if unsafe {
    //     libc::mount(
    //         c_src.as_ptr(),
    //         c_target.as_ptr(),
    //         c_fs.as_ptr(),
    //         combined_flags,
    //         std::ptr::null(),
    //     )
    // } == -1 {
    //     return Err(Error::last_os_error());
    // }
    return Ok(());
}

pub fn write_uid_gid_map(uid: uid_t, gid: gid_t) -> Result<(), Error> {
    {
        let mut f = File::options().write(true).open("/proc/self/setgroups")?;
        f.write_all("deny\n".as_ref())?;
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
    Uts,
    // separate hostname
    Ipc,
    // CLONE_NEWTIME
}

pub fn unshare(namespaces: impl IntoIterator<Item=Namespace>) -> Result<(), Error> {
    let combined_flags = namespaces.into_iter().fold(0, |acc, namespace: Namespace| {
        return acc | match namespace {
            Namespace::Pid => CLONE_NEWPID,
            Namespace::Mount => CLONE_NEWNS,
            Namespace::User => CLONE_NEWUSER,
            Namespace::Network => CLONE_NEWNET,
            Namespace::Uts => CLONE_NEWUTS,
            Namespace::Ipc => CLONE_NEWIPC
        };
    });
    if unsafe { libc::unshare(combined_flags) } == -1 {
        return Err(Error::last_os_error());
    }
    return Ok(());
}

pub fn fork(child_body: impl FnOnce()) -> Result<pid_t, Error> {
    let ret = unsafe { libc::fork() };
    if ret == -1 {
        return Err(Error::last_os_error());
    }
    if ret == 0 {
        child_body();
        std::process::exit(0);
    }
    return Ok(ret);
}
