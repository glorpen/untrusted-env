use std::borrow::{Borrow, BorrowMut};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::ptr;

use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

#[derive(Debug)]
pub enum Error {
    DuplicatedFdWithDifferentSource,
    IoError(std::io::Error),
    Errno(nix::errno::Errno),
}

impl From<nix::errno::Errno> for Error {
    fn from(err: nix::errno::Errno) -> Self {
        Error::Errno(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IoError(err)
    }
}

type EventCallback = fn(fd: RawFd) -> Result<(), Error>;

type EventMap = HashMap<ReactorEvent, EventCallback>;

struct ScopedReactor {
    fds: HashMap<RawFd, EventMap>,
}

struct EventHandler {
    callback: EventCallback,
    scope: Option<usize>,
}

type EventHandlerMap = HashMap<ReactorEvent, EventHandler>;

struct Reactor {
    epoll_fd: RawFd,
    fds: HashMap<RawFd, EventHandlerMap>,
}

#[derive(EnumIter, Debug, PartialEq, Eq, Hash, Copy, Clone)]
enum ReactorEvent {
    ReadyRead,
    ReadyWrite,
}

impl ScopedReactor {
    fn new() -> Self {
        Self {
            fds: Default::default(),
        }
    }

    pub fn contains(&self, fd: &RawFd, event: &ReactorEvent) -> bool {
        if !self.fds.contains_key(fd) {
            return false;
        }
        if !self.fds.get(fd).unwrap().contains_key(event) {
            return false;
        }

        return true;
    }

    pub fn clear(&mut self) {
        self.fds.clear()
    }
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

trait MutableReactor {
    fn add_input(&mut self, fd: RawFd, handler: EventCallback) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyRead, handler)
    }

    fn add_output(&mut self, fd: RawFd, handler: EventCallback) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyWrite, handler)
    }

    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback) -> Result<(), Error>;

    fn remove(&mut self, fd: RawFd, event: ReactorEvent) -> Result<(), Error>;
}

impl Reactor {
    pub fn create() -> Reactor {
        Reactor {
            epoll_fd: epoll_create().unwrap(),
            fds: Default::default(),
        }
    }

    fn add_fd(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback, source: Option<&ScopedReactor>) -> Result<(), Error> {
        if !self.fds.contains_key(&fd) {
            self.fds.insert(fd, EventHandlerMap::new());
        }

        let infos = self.fds.get_mut(&fd).unwrap();
        let event_handler = EventHandler {
            callback: handler,
            scope: source.map(|s| { ptr::addr_of!(*s) as *const i32 as usize }),
        };

        {
            let old_handler = infos.get(&revent);
            if old_handler.is_some() {
                let old_source = old_handler.unwrap().scope;

                if old_source.is_some() && source.is_some() {
                    if old_source.unwrap() != event_handler.scope.unwrap() {
                        return Err(Error::DuplicatedFdWithDifferentSource);
                    }
                } else if !old_source.is_none() || !source.is_none() {
                    return Err(Error::DuplicatedFdWithDifferentSource);
                }
            }
        }

        if infos.insert(revent, event_handler).is_none() {
            let mut event = Some(EpollEvent::new(ReactorEvent::epoll_flags(infos.keys()), 0));

            if infos.len() > 1 {
                epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, fd, &mut event)?;
            } else {
                epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, fd, &mut event)?;
            }
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

            let events_count = epoll_wait(self.epoll_fd, &mut events, 1000)?;
            for i in 0..events_count {
                let fd = events[i].data() as RawFd;
                let flags = events[i].events();

                // TODO: gracefully handle errors
                if self.fds.contains_key(&fd) {
                    let items = self.fds.get(&fd).unwrap();
                    for (event, handler) in items.iter() {
                        if flags.contains(event.to_epoll_flag()) {
                            (handler.callback)(fd).expect("fd event");
                        }
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn update_scope(&mut self, scope: &ScopedReactor) -> Result<(), Error> {
        let scope_ptr = ptr::addr_of!(*scope) as *const i32 as usize;

        let pending_removal = {
            let mut items: HashMap<RawFd, Vec<ReactorEvent>> = Default::default();

            for (fd, events_handler_map) in self.fds.iter() {
                for (event, handler) in events_handler_map.iter() {
                    if handler.scope.is_none() {
                        continue;
                    }

                    if handler.scope.unwrap() == scope_ptr {
                        if !scope.contains(fd, event) {
                            if !items.contains_key(fd) {
                                items.insert(*fd, Vec::new());
                            }
                            items.get_mut(fd).unwrap().push(*event);
                        }
                    }
                }
            }

            items
        };

        for (fd, events) in pending_removal {
            for event in events {
                self.remove(fd, event)?;
            }
        }


        for (fd, events_map) in scope.fds.iter() {
            for (event, callback) in events_map {
                self.add_fd(*fd.borrow(), *event, *callback, Some(scope))?;
            }
        }

        return Ok(());
    }
}

impl MutableReactor for Reactor {
    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback) -> Result<(), Error> {
        self.add_fd(fd, revent, handler, None)
    }

    fn remove(&mut self, fd: RawFd, event: ReactorEvent) -> Result<(), Error> {
        if !self.fds.contains_key(&fd) { return Ok(()); }

        let info = self.fds.get_mut(&fd).unwrap();
        info.remove(&event);

        if info.len() > 0 {
            // remove watching for single event
            let mut epoll_event = Some(EpollEvent::new(ReactorEvent::epoll_flags(info.keys()), 0));
            epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, fd, &mut epoll_event)?;
        } else {
            // delete whole fd
            epoll_ctl(self.epoll_fd, EpollOp::EpollCtlDel, fd, None)?;
        }

        return Ok(());
    }
}

impl MutableReactor for ScopedReactor {
    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback) -> Result<(), Error> {
        if !self.fds.contains_key(&fd) {
            self.fds.insert(fd, EventMap::new());
        }
        self.fds.get_mut(&fd).unwrap().insert(revent, handler);

        return Ok(());
    }

    fn remove(&mut self, fd: RawFd, event: ReactorEvent) -> Result<(), Error> {
        if self.fds.contains_key(&fd) {
            self.fds.get_mut(&fd).unwrap().remove(&event);
        }

        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::io::RawFd;

    use crate::events::{Error, MutableReactor, Reactor, ReactorEvent, ScopedReactor};

    fn assert_fd_listeners_count(reactor: &Reactor, fd: RawFd, count: usize) {
        assert!(reactor.fds.contains_key(&fd));
        assert_eq!(reactor.fds.get(&fd).unwrap().len(), count);
    }

    #[test]
    fn reactor_crud() {
        let mut r = Reactor::create();
        r.add(1, ReactorEvent::ReadyRead, |fd| { Ok(()) });
        assert_fd_listeners_count(&r, 1, 1);
        r.add(1, ReactorEvent::ReadyWrite, |fd| { Ok(()) });
        assert_fd_listeners_count(&r, 1, 2);
        r.remove(1, ReactorEvent::ReadyRead);
        assert_fd_listeners_count(&r, 1, 1);
        r.remove(1, ReactorEvent::ReadyWrite);
        assert_fd_listeners_count(&r, 1, 0);
    }

    #[test]
    fn scopes() {
        let global_fd = 1;
        let scope_fd = 2;

        let mut r = Reactor::create();
        let cb = |fd| { Ok(()) };
        r.add(global_fd, ReactorEvent::ReadyRead, cb);

        let mut s = ScopedReactor::new();

        s.add_output(global_fd, cb);
        s.add_output(2, cb);

        r.update_scope(&s).unwrap();
        assert_fd_listeners_count(&r, global_fd, 2);
        assert_fd_listeners_count(&r, scope_fd, 1);

        // remove existing fd and add a new one
        s.remove(2, ReactorEvent::ReadyWrite);
        r.update_scope(&s).unwrap();

        assert_fd_listeners_count(&r, scope_fd, 0);

        // add duplicated global fd
        s.add_input(1, cb).unwrap();
        assert!(matches!(r.update_scope(&s), Err(Error::DuplicatedFdWithDifferentSource)));

        // add dublicated fd in another scope
        let ns = ScopedReactor::new();
        s.add_input(1, cb).unwrap();
        assert!(matches!(r.update_scope(&s), Err(Error::DuplicatedFdWithDifferentSource)));
    }
}
