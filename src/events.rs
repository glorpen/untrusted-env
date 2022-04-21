use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use nix::errno::Errno;
use nix::Error;
use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

type EventCallback = fn(fd: RawFd) -> Result<(), Error>;
type FdInfo = HashMap<ReactorEvent, EventCallback>;
type RawFdCallbackMap = HashMap<RawFd, FdInfo>;

struct Reactor {
    epoll_fd: RawFd,
    fds: RawFdCallbackMap
}

struct ScopedReactor {
    name: String
}


#[derive(EnumIter, Debug, PartialEq, Eq, Hash)]
enum ReactorEvent {
    ReadyRead,
    ReadyWrite
}

impl ReactorEvent {
    fn to_epoll_flag(&self) -> EpollFlags {
        match self {
            ReactorEvent::ReadyRead => EpollFlags::EPOLLIN,
            ReactorEvent::ReadyWrite => EpollFlags::EPOLLOUT
        }
    }
    fn epoll_flags<'x>(events: impl Iterator<Item=&'x ReactorEvent>) -> EpollFlags {
        events.fold(EpollFlags::empty(), |acc, e| {
            acc | e.to_epoll_flag()
        })
    }
}

impl Reactor {
    pub fn create() -> Reactor {
        Reactor {
            epoll_fd: epoll_create().unwrap(),
            fds: HashMap::<RawFd, FdInfo>::new()
        }
    }

    pub fn add_input(&mut self, fd: RawFd, handler: EventCallback) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyRead, handler)
    }

    pub fn add_output(&mut self, fd: RawFd, handler: EventCallback) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyWrite, handler)
    }

    pub fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback) -> Result<(), Error> {
        if ! self.fds.contains_key(&fd) {
            self.fds.insert(fd, FdInfo::new());
        }

        let infos = self.fds.get_mut(&fd).unwrap();

        if infos.insert(revent, handler).is_none() {
            let mut event = Some(EpollEvent::new(ReactorEvent::epoll_flags(infos.keys()), 0));

            if infos.len() > 1 {
                epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, fd, &mut event)?;
            } else {
                epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, fd, &mut event)?;
            }
        }

        return Ok(());
    }

    pub fn remove(&mut self, fd: RawFd, event: ReactorEvent) -> Result<(), Error> {
        if ! self.fds.contains_key(&fd) { return Ok(()); }

        let info = self.fds.get_mut(&fd).unwrap();

        if info.len() > 1 {
            // remove watching for single event
            info.remove(&event);
            let mut event = Some(EpollEvent::new(ReactorEvent::epoll_flags(info.keys()), 0));
            epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, fd, &mut event)?;
        } else {
            // delete whole fd
            epoll_ctl(self.epoll_fd, EpollOp::EpollCtlDel, fd, None)?;
        }

        return Ok(());
    }

    /// to handle closing one should call remove() in callback and close fd
    pub fn run(&mut self) -> Result<(), Error> {
        let event = EpollEvent::new(EpollFlags::empty(), 0);
        let mut events: Vec<EpollEvent> = Vec::new();

        // when writing: write nonblocking until EAGAIN, add EPOLLOUT to watchlist, repeat until done sending, remove EPOLLOUT

        loop {
            events.resize(self.fds.len(), EpollEvent::empty());

            let events_count = epoll_wait(self.epoll_fd, &mut events, 1000).expect("epoll wait");
            for i in 0..events_count {
                let fd = events[i].data() as RawFd;
                let flags = events[i].events();

                // TODO: gracefully handle errors
                if self.fds.contains_key(&fd) {
                    let items = self.fds.get(&fd).unwrap();
                    for (event, callback) in items.iter() {
                        if flags.contains(event.to_epoll_flag()) {
                            (*callback)(fd).expect("fd event");
                        }
                    }
                }
            }
        }
    }

    pub fn update_scope(scope: &ScopedReactor) {

    }
    fn new_scoped(name: String) -> ScopedReactor {
        ScopedReactor {
            name
        }
    }
}
