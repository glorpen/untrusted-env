use std::borrow::{Borrow, BorrowMut};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::ptr;

use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use nix::unistd::close;
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
    epoll_fd: Option<RawFd>,
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

pub fn reactor_noop_config<T>(arg: T) -> isize {
    -1
}

impl Reactor {
    pub fn create() -> Reactor {
        Reactor {
            epoll_fd: Some(epoll_create().unwrap()),
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
                epoll_ctl(self.epoll_fd.unwrap(), EpollOp::EpollCtlMod, fd, &mut event)?;
            } else {
                epoll_ctl(self.epoll_fd.unwrap(), EpollOp::EpollCtlAdd, fd, &mut event)?;
            }
        }

        return Ok(());
    }

    /// Run event loop.
    ///
    /// Use `config` param to change timeout and/or manage fd on each loop
    /// To handle closing one should call remove() in callback and close fd.
    #[must_use]
    fn run<T>(&mut self, mut config: T) -> Result<(), Error>
        where T: FnMut(&mut Self) -> isize
    {
        let mut events: Vec<EpollEvent> = Vec::new();

        loop {
            let timeout = config(self);
            // configurator can stop reactor
            if self.epoll_fd.is_none() { break; }

            events.resize(self.fds.len(), EpollEvent::empty());

            let events_count = {
                let wait_result = epoll_wait(self.epoll_fd.unwrap(), &mut events, timeout);
                if Err(nix::errno::Errno::EINTR) == wait_result && self.epoll_fd.is_none() { break; }

                wait_result.unwrap()
            };

            for i in 0..events_count {
                let fd = events[i].data() as RawFd;
                let flags = events[i].events();

                // TODO: gracefully handle errors
                if self.fds.contains_key(&fd) {
                    let items = self.fds.get(&fd).unwrap();
                    for (event, handler) in items.iter() {
                        if flags.contains(event.to_epoll_flag()) {
                            (handler.callback)(fd)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Updates reactor descriptors from given scope.
    /// To add, change or remove descriptors you have to call `update_scope` with mutated `ScopedReactor`.
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

    pub fn shutdown(&mut self) {
        if self.epoll_fd.is_some() {
            close(self.epoll_fd.unwrap());
            self.epoll_fd = None;
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        // TODO: check if double close is not happening
        self.shutdown()
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
            epoll_ctl(self.epoll_fd.unwrap(), EpollOp::EpollCtlMod, fd, &mut epoll_event)?;
        } else {
            // delete whole fd
            epoll_ctl(self.epoll_fd.unwrap(), EpollOp::EpollCtlDel, fd, None)?;
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

    use nix::unistd::{pipe, read};

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

    #[test]
    fn callbacks() {
        let mut r = Reactor::create();
        let (p_rd, p_wr) = pipe().unwrap();

        r.add_input(p_rd, |fd| {
            let mut buf = [0; 10];
            read(fd, &mut buf);

            Ok(())
        });

        let mut loop_count = 2;
        r.run(|r| {
            loop_count -= 1;
            if loop_count <= 0 {
                r.shutdown();
            }
            100
        }).unwrap();
    }
}
