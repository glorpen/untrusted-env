use std::borrow::{Borrow, BorrowMut};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::ptr;

use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use nix::unistd::{close, read, write};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

const BUFFER_SIZE: usize = 1024;

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

type EventCallback<S> = fn(fd: RawFd, state: &mut S) -> Result<(), Error>;

type EventMap<S> = HashMap<ReactorEvent, EventCallback<S>>;

struct ScopedReactor<S> {
    fds: HashMap<RawFd, EventMap<S>>,
}

struct EventHandler<S> {
    callback: EventCallback<S>,
    scope: Option<usize>,
}

type EventHandlerMap<S> = HashMap<ReactorEvent, EventHandler<S>>;

struct Reactor<S> {
    epoll_fd: Option<RawFd>,
    fds: HashMap<RawFd, EventHandlerMap<S>>,
}

#[derive(EnumIter, Debug, PartialEq, Eq, Hash, Copy, Clone)]
enum ReactorEvent {
    ReadyRead,
    ReadyWrite,
}

impl<S> ScopedReactor<S> {
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

trait MutableReactor<S> {
    fn add_input(&mut self, fd: RawFd, handler: EventCallback<S>) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyRead, handler)
    }

    fn add_output(&mut self, fd: RawFd, handler: EventCallback<S>) -> Result<(), Error> {
        self.add(fd, ReactorEvent::ReadyWrite, handler)
    }

    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback<S>) -> Result<(), Error>;

    fn remove(&mut self, fd: RawFd, event: ReactorEvent) -> Result<(), Error>;
}

pub fn reactor_noop_config<T>(arg: T) -> isize {
    -1
}

impl<S> Reactor<S> {
    pub fn create() -> Reactor<S> {
        Reactor {
            epoll_fd: Some(epoll_create().unwrap()),
            fds: Default::default(),
        }
    }

    fn add_fd(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback<S>, source: Option<&ScopedReactor<S>>) -> Result<(), Error> {
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
            let mut event = Some(EpollEvent::new(ReactorEvent::epoll_flags(infos.keys()), fd as u64));

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
    fn run<T>(&mut self, mut config: T, state: &mut S) -> Result<(), Error>
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
                            (handler.callback)(fd, state)?;
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
    pub fn update_scope(&mut self, scope: &ScopedReactor<S>) -> Result<(), Error> {
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

impl<S> Drop for Reactor<S> {
    fn drop(&mut self) {
        // TODO: check if double close is not happening
        self.shutdown()
    }
}

impl<S> MutableReactor<S> for Reactor<S> {
    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback<S>) -> Result<(), Error> {
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

impl<S> MutableReactor<S> for ScopedReactor<S> {
    fn add(&mut self, fd: RawFd, revent: ReactorEvent, handler: EventCallback<S>) -> Result<(), Error> {
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

#[must_use]
fn forward_buffer(send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<usize, Error> {
    let mut written: usize = 0;

    while send_buffer.len() > 0 {
        let size = write(f_out, send_buffer)?;
        send_buffer.drain(0..size);
        written += size;
    }

    Ok(written)
}

#[must_use]
fn forward_iter<'x>(f_in: impl Iterator<Item=&'x [u8]>, send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<usize, Error> {
    let mut written = forward_buffer(send_buffer, f_out)?;
    for data in f_in {
        send_buffer.extend_from_slice(data);
        if send_buffer.len() >= BUFFER_SIZE {
            written += forward_buffer(send_buffer, f_out)?;
        }
    }
    written += forward_buffer(send_buffer, f_out)?;

    Ok(written)
}

#[must_use]
fn read_fd_until_full(f_in: RawFd, buffer: &mut Vec<u8>) -> Result<IOAction, Error> {
    let mut bytes_read = 0;

    while buffer.len() <= BUFFER_SIZE {
        let size = buffer.len();
        match read(f_in, &mut buffer[size..]) {
            Ok(size) => {
                bytes_read += size;
            }
            Err(nix::errno::Errno::EAGAIN) => {
                return Ok(IOAction::again(bytes_read));
            }
            Err(e) => {
                return Err(Error::from(e))
            }
        };
        if buffer.len() >= BUFFER_SIZE {
            break;
        }
    }

    Ok(IOAction::done(bytes_read))
}

struct IOAction {
    bytes_transferred: usize,
    try_again: bool
}

impl IOAction {
    fn again(bytes_transferred: usize) -> Self {
        Self {
            bytes_transferred,
            try_again: true
        }
    }
    fn done(bytes_transferred: usize) -> Self {
        Self {
            bytes_transferred,
            try_again: false
        }
    }
}

#[must_use]
fn forward_fd(f_in: RawFd, send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<IOAction, Error> {
    // add f_out to watchable if WouldBlock
    // remove f_out from watchable if read_op would block and empty buffer

    let mut reads_ended = false;
    let mut written_bytes: usize = 0;

    loop {
        if ! reads_ended {
            let read_info = read_fd_until_full(f_in, send_buffer)?;
            if read_info.try_again {
                // read stream stopped
                reads_ended = true;
            }
        }

        match forward_buffer(send_buffer, f_out) {
            Ok(bytes) => {
                written_bytes += bytes;
            }
            Err(Error::Errno(nix::errno::Errno::EAGAIN)) => {
                return Ok(IOAction::again(written_bytes));
            }
            Err(e) => return Err(e)
        };

        if send_buffer.is_empty() {
            break;
        }
    }

    return Ok(IOAction::done(written_bytes));
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};
    use std::os::unix::io::RawFd;
    use std::thread;
    use std::time::Duration;

    use nix::fcntl::OFlag;
    use nix::unistd::{pipe, pipe2, read, write};

    use crate::events::{Error, EventCallback, MutableReactor, Reactor, ReactorEvent, ScopedReactor};

    struct State {
        counter: usize,
    }

    impl State {
        pub fn new() -> Self {
            Self {
                counter: 0
            }
        }
    }


    fn assert_fd_listeners_count<S>(reactor: &Reactor<S>, fd: RawFd, count: usize) {
        assert!(reactor.fds.contains_key(&fd));
        assert_eq!(reactor.fds.get(&fd).unwrap().len(), count);
    }

    #[test]
    fn reactor_crud() {
        let mut r = Reactor::<State>::create();
        r.add(1, ReactorEvent::ReadyRead, |fd, state| { Ok(()) });
        assert_fd_listeners_count(&r, 1, 1);
        r.add(1, ReactorEvent::ReadyWrite, |fd, state| { Ok(()) });
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

        let mut r = Reactor::<State>::create();
        let cb: EventCallback<State> = |fd, state| { Ok(()) };
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
        let ns = ScopedReactor::<State>::new();
        s.add_input(1, cb).unwrap();
        assert!(matches!(r.update_scope(&s), Err(Error::DuplicatedFdWithDifferentSource)));
    }

    fn run_reactor_once<T>(reactor: &mut Reactor<T>, state: &mut T) {
        let mut iterations_count = 1;
        reactor.run(|r: &mut Reactor<T>| {
            if iterations_count <= 0 {
                r.shutdown();
            }
            iterations_count -= 1;
            100
        }, state).unwrap();
    }

    #[test]
    fn callbacks() {
        let mut r = Reactor::create();
        let (p_rd, p_wr) = pipe2(OFlag::O_NONBLOCK).unwrap();
        let mut state = State::new();
        let cb: EventCallback<State> = |fd, state| {
            let mut buf = [0; 10];
            state.counter += read(fd, &mut buf).unwrap();
            Ok(())
        };

        r.add_input(p_rd, cb);

        let written = write(p_wr, CString::new("test").unwrap().to_bytes()).unwrap();
        run_reactor_once(&mut r, &mut state);
        assert_eq!(written, state.counter);
    }
}
