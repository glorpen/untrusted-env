use std::borrow::Borrow;
use std::collections::LinkedList;
use std::fs::{create_dir, create_dir_all, File, Permissions, set_permissions};
use std::io::{Error, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::Path;

use log::info;
use nix::unistd::{chdir, fchdir};

use crate::ipc::{Api, ChildSide, Reactor, ReactorHandler};
use crate::path::{_Path, NEW_ROOT_DIR, OLD_ROOT_DIR, ROOT_INIT_PATH};
use crate::sys::{fork, get_user_info, mount, MountFlag, Namespace, pivot_root, umount2, UmountFlag, unshare, UserInfo, write_uid_gid_map};

pub struct RootFsView<'x> {
    init_path: &'x _Path,
    init_new_path: &'x _Path,
    init_old_path: &'x _Path,
    new_path: &'x _Path,
    old_path: &'x _Path,
}

impl<'x> RootFsView<'x> {
    fn newroot(&self) -> Result<(), Error> {
        mount(Option::None, self.init_path.into(), Option::from("tmpfs"), [
            MountFlag::NoSuid,
            MountFlag::NoDev,
            MountFlag::Recursive
        ], None)?;

        create_dir(self.init_new_path.as_path())?;
        create_dir(self.init_old_path.as_path())?;

        mount(
            Some(self.init_new_path.as_path()),
            self.init_new_path.into(),
            Option::from("tmpfs"),
            [],
            None,
        )?;
        pivot_root(self.init_path.into(), self.init_old_path.into())?;

        return Ok(());
    }


    fn mount_proc(&self) -> Result<(), Error> {
        let target = &(self.new_path / "proc");
        create_dir(target.as_path())?;
        mount(Option::None, target.into(), Option::from("proc"), [
            MountFlag::NoSuid,
            MountFlag::NoDev,
            MountFlag::NoExec
        ], None)?;

        return Ok(());
    }


    fn assure_root_path<F>(&self, source_path: &Path, target_path: Option<&Path>, then: F) -> Result<(), Error>
        where F: FnOnce(&_Path, &_Path) -> Result<(), Error> {
        fn make_relative(path: &Path) -> &Path {
            return if path.is_absolute() {
                path.strip_prefix("/").unwrap()
            } else { path };
        }

        let local_source_path = &(self.old_path / make_relative(source_path));
        let local_target_path = &(self.new_path / make_relative(target_path.unwrap_or(source_path)));

        if !local_source_path.as_path().exists() {
            info!("Source path {} does not exist, skipping.", local_source_path);
            return Ok(());
        }

        if local_source_path.as_path().is_dir() {
            info!("Creating dir {}.", local_target_path);
            create_dir_all(local_target_path.as_path())?;
            // set_permissions(path, Permissions::from_mode(0o755))?;
        } else {
            info!("Creating file {}.", local_target_path);
            create_dir_all(local_target_path.as_path().parent().unwrap().to_str().unwrap())?;
            File::create(&local_target_path.as_path())?;
        }

        return then(&local_source_path, &local_target_path);
    }

    fn mount_dev(&self) -> Result<(), Error> {
        let target_dev = &(self.new_path / "dev");
        let target_pts = &(target_dev / "pts");
        let target_shm = &(target_dev / "shm");

        fn create_dev_dir(path: &str) -> Result<(), Error> {
            create_dir(path)?;
            set_permissions(path, Permissions::from_mode(0o755))?;
            return Ok(());
        }

        create_dev_dir(target_dev.into())?;
        create_dev_dir(target_shm.into())?;

        create_dev_dir(target_pts.into())?;
        mount(None, target_pts.into(), Option::from("devpts"), [
            MountFlag::NoSuid,
            MountFlag::NoExec
        ], Option::Some("newinstance,ptmxmode=0666,mode=620"))?;

        // we cannot mknod as we don't have CAP_MKNOD, mount needed dirs/devices
        for name in ["dri", "null", "zero", "full", "random", "urandom", "tty"] {
            let path = &(_Path::from("/") / "dev" / name);
            self.assure_root_path(path.into(), None, |source, target| {
                mount(Option::from(source.as_path()), target.into(), None, [
                    MountFlag::Recursive,
                    MountFlag::Silent,
                ], None)?;

                return Ok(());
            })?;
        }

        return Ok(());
    }


    fn mount_host(&self, host_path: &Path, container_path: Option<&Path>, readonly: bool) -> Result<(), Error> {
        let mut mount_flags = LinkedList::from([
            MountFlag::Recursive,
            MountFlag::Silent,
            MountFlag::NoDev
        ]);
        if readonly {
            mount_flags.push_back(MountFlag::Readonly);
        }

        return self.assure_root_path(host_path, container_path, |source, target| {
            mount(Option::from(source.as_path()), target.into(), None, mount_flags, None)?;
            return Ok(());
        });
    }

    fn cleanup_newroot(&self) -> Result<(), Error> {
//     remove oldroot and make newroot a root
// # make old root private so it does umount not propagate

        mount(None, self.old_path.into(), None, [
            MountFlag::Silent,
            MountFlag::Recursive,
            MountFlag::Private
        ], None)?;
        umount2(self.old_path.into(), [
            UmountFlag::Detach
        ])?;

        let current_root = File::open("/")?.into_raw_fd();
        pivot_root(self.new_path.into(), self.new_path.into())?;

        fchdir(current_root)?;
        umount2(".", [UmountFlag::Detach])?;
        chdir("/")?;

        return Ok(());
    }

    fn write_user_info(&self, info: &UserInfo) -> Result<(), Error> {
        let mut passwd_file = File::create((self.new_path / "etc" / "passwd").as_path())?;
        write!(passwd_file, "{}:x:{}:{}:user:/home:{}\n", info.user.name, info.user.id, info.group.id, info.shell.to_str().unwrap())?;
        write!(passwd_file, "nobody:x:65534:65534:nobody:/var/empty:/bin/false\n")?;

        let mut group_file = File::create((self.new_path / "etc" / "group").as_path())?;
        write!(group_file, "{}:x:{}:{}\n", info.group.name, info.group.id, info.user.name)?;
        write!(group_file, "nobody:x:65534:{}\n", info.user.name)?;

        return Ok(());
    }
}

fn run_stage3(rootfs: &RootFsView, user_info: &UserInfo, api: &mut ChildSide) -> Result<(), Error> {
    info!("stage3: setup stdio");
    api.setup_stdio()?;

    rootfs.newroot()?;

    rootfs.mount_proc()?;
    rootfs.mount_dev()?;

    for name in [
        "/lib", "/lib32", "/lib64", "/bin", "/usr",
        "etc/ld.so.conf", "etc/ld.so.cache", "etc/ld.so.conf.d",
        "etc/bash", "etc/bash_completion.d"
    ] {
        info!("creating {}", name);
        rootfs.mount_host(Path::new(name), Option::None, true)?;
    }

    rootfs.write_user_info(user_info.borrow())?;

    rootfs.cleanup_newroot()?;

    // nix::mqueue::mq_send(queue, CString::new("test").unwrap().as_bytes(), 1)
    //     .unwrap();
    // nix::unistd::write(queue, CString::new("test test").unwrap().as_bytes()).unwrap();


    std::process::Command::new(user_info.shell.to_str().unwrap()).spawn()?.wait()?;
    // TODO: handle SIGCHLD as PID1
    let mut reactor = Reactor::create();
    reactor.run(api).expect("asd");

    // nix::unistd::close(queue).unwrap();

    return Ok(());
}

fn run_stage2(api: &mut ChildSide) -> Result<(), Error> {
    let info = get_user_info()?;

    unshare([
        Namespace::Pid,
        Namespace::Mount,
        Namespace::User,
        Namespace::Network,
        Namespace::Uts,
        Namespace::Ipc,
    ])?;

    write_uid_gid_map(
        info.user.id,
        info.group.id,
    )?;

    let pid = fork(|| {
        let init_path = _Path::from_iter([ROOT_INIT_PATH]);
        let init_new_path = _Path::from_iter([ROOT_INIT_PATH, NEW_ROOT_DIR]);
        let init_old_path = _Path::from_iter([ROOT_INIT_PATH, OLD_ROOT_DIR]);
        let new_path = _Path::from_iter(["/", NEW_ROOT_DIR]);
        let old_path = _Path::from_iter(["/", OLD_ROOT_DIR]);

        let rootfs = RootFsView {
            init_path: &init_path,
            init_new_path: &init_new_path,
            init_old_path: &init_old_path,
            new_path: &new_path,
            old_path: &old_path,
        };

        run_stage3(&rootfs, info.borrow(), api).unwrap();
    })?;

    nix::sys::wait::waitpid(pid, None)?;

    return Ok(());
}

pub fn run() -> Result<(), Error> {
    let mut api = Api::init();

    api.parent.setup_stdio()?;

    let stage2_pid = fork(|| {
        api.parent.close().unwrap();
        run_stage2(&mut api.child).expect("stage2");
    }).unwrap();

    api.child.close()?;

    let mut reactor = Reactor::create();
    reactor.run(&mut api.parent).expect("asd");

    nix::sys::wait::waitpid(stage2_pid, None)?;

    return Ok(());
}
