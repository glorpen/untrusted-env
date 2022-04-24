use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::os::unix::io::RawFd;
use std::ptr;

use libc::{c_void, size_t};
use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use nix::unistd::{close, write};
use strum_macros::EnumIter;

#[derive(Debug)]
pub enum Error {
    DuplicatedFdWithDifferentSource,
    EmptyRead,
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

type EventCallback<S> = fn(reactor: &mut Reactor<S>, state: &mut S);

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

#[macro_export]
macro_rules! reactor_run {
    ($reactor:expr, $config:expr, $state:expr) => {
        $reactor.run($config, $state)
    };
    ($reactor:expr, $state:expr) => {
        $reactor.run(|_| { -1 }, $state)
    };
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
    /// Use `pre_configurator` param to change timeout and/or manage fd on each loop
    /// To handle closing one should call remove() in callback and close fd.
    #[must_use]
    pub fn run<T>(&mut self, mut pre_configurator: T, state: &mut S) -> Result<(), Error>
        where T: FnMut(&mut Self) -> isize
    {
        let mut events: Vec<EpollEvent> = Vec::new();

        loop {
            let timeout = pre_configurator(self);
            // configurator can stop reactor
            if self.epoll_fd.is_none() { break; }

            events.resize(self.fds.len(), EpollEvent::empty());

            let events_count = match epoll_wait(self.epoll_fd.unwrap(), &mut events, timeout) {
                Ok(res) => {
                    res
                }
                Err(nix::errno::Errno::EINTR) => {
                    if self.epoll_fd.is_none() { break; }
                    0
                }
                Err(err) => {
                    return Err(Error::Errno(err));
                }
            };

            for i in 0..events_count {
                let fd = events[i].data() as RawFd;
                let flags = events[i].events();

                let mut callbacks = Vec::new();

                // TODO: gracefully handle errors
                if self.fds.contains_key(&fd) {
                    let items = self.fds.get(&fd).unwrap();
                    for (event, handler) in items.iter() {
                        if flags.contains(event.to_epoll_flag()) {
                            callbacks.push(handler.callback);
                        }
                    }
                }

                for callback in callbacks {
                    callback(self, state);
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
            close(self.epoll_fd.unwrap()).unwrap();
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
pub fn forward_buffer(send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<usize, Error> {
    let mut written: usize = 0;

    while send_buffer.len() > 0 {
        let size = write(f_out, send_buffer)?;
        send_buffer.drain(0..size);
        written += size;
    }

    Ok(written)
}

#[must_use]
pub fn forward_iter<'x>(f_in: impl Iterator<Item=&'x [u8]>, send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<usize, Error> {
    let mut written = forward_buffer(send_buffer, f_out)?;
    for data in f_in {
        send_buffer.extend_from_slice(data);
        if send_buffer.len() >= send_buffer.capacity() {
            written += forward_buffer(send_buffer, f_out)?;
        }
    }
    written += forward_buffer(send_buffer, f_out)?;

    Ok(written)
}

pub fn read_append(fd: RawFd, buffer: &mut Vec<u8>) -> Result<usize, Error> {
    let offset = buffer.len();
    let read_size = buffer.capacity() - offset;
    if read_size <= 0 {
        Err(Error::EmptyRead)
    } else {
        let res = unsafe { libc::read(fd, buffer[offset..].as_mut_ptr() as *mut c_void, read_size as size_t) };
        let ret = nix::errno::Errno::result(res).map(|r| r as usize).map_err(|e| {
            Error::Errno(e)
        });

        if ret.is_ok() {
            unsafe { buffer.set_len(res as usize + offset) };
        }

        ret
    }
}

#[must_use]
fn read_fd_until_full(f_in: RawFd, buffer: &mut Vec<u8>) -> Result<PipeState, Error> {
    let mut bytes_read = 0;

    while buffer.len() < buffer.capacity() {
        match read_append(f_in, buffer) {
            Ok(size) => {
                bytes_read += size;
            }
            Err(Error::Errno(nix::errno::Errno::EAGAIN)) => {
                return Ok(PipeState::again(bytes_read));
            }
            Err(e) => {
                return Err(Error::from(e));
            }
        };
    }

    Ok(PipeState::done(bytes_read))
}

pub struct PipeState {
    bytes_transferred: usize,
    try_again: bool,
}

impl PipeState {
    fn again(bytes_transferred: usize) -> Self {
        Self {
            bytes_transferred,
            try_again: true,
        }
    }
    fn done(bytes_transferred: usize) -> Self {
        Self {
            bytes_transferred,
            try_again: false,
        }
    }
}

#[must_use]
pub fn forward_fd(f_in: RawFd, send_buffer: &mut Vec<u8>, f_out: RawFd) -> Result<PipeState, Error> {
    let mut reads_ended = false;
    let mut written_bytes: usize = 0;

    loop {
        if !reads_ended {
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
                return Ok(PipeState::again(written_bytes));
            }
            Err(e) => return Err(e)
        };

        if send_buffer.is_empty() {
            break;
        }
    }

    return Ok(PipeState::done(written_bytes));
}

#[cfg(test)]
mod tests {
    use std::os::unix::io::RawFd;

    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::unistd::{pipe2, read, write};

    use crate::events::{Error, EventCallback, forward_fd, MutableReactor, Reactor, ReactorEvent, ScopedReactor};

    struct NoopState {}

    fn assert_fd_listeners_count<S>(reactor: &Reactor<S>, fd: RawFd, count: usize) {
        assert!(reactor.fds.contains_key(&fd));
        assert_eq!(reactor.fds.get(&fd).unwrap().len(), count);
    }

    #[test]
    fn reactor_crud() {
        let mut r = Reactor::<NoopState>::create();
        r.add(1, ReactorEvent::ReadyRead, |_reactor, _state| {}).unwrap();
        assert_fd_listeners_count(&r, 1, 1);
        r.add(1, ReactorEvent::ReadyWrite, |_reactor, _state| {}).unwrap();
        assert_fd_listeners_count(&r, 1, 2);
        r.remove(1, ReactorEvent::ReadyRead).unwrap();
        assert_fd_listeners_count(&r, 1, 1);
        r.remove(1, ReactorEvent::ReadyWrite).unwrap();
        assert_fd_listeners_count(&r, 1, 0);
    }

    #[test]
    fn scopes() {
        let global_fd = 1;
        let scope_fd = 2;

        let mut r = Reactor::<NoopState>::create();
        let cb: EventCallback<NoopState> = |_reactor, _state| {};
        r.add(global_fd, ReactorEvent::ReadyRead, cb).unwrap();

        let mut s = ScopedReactor::new();

        s.add_output(global_fd, cb).unwrap();
        s.add_output(2, cb).unwrap();

        r.update_scope(&s).unwrap();
        assert_fd_listeners_count(&r, global_fd, 2);
        assert_fd_listeners_count(&r, scope_fd, 1);

        // remove existing fd and add a new one
        s.remove(2, ReactorEvent::ReadyWrite).unwrap();
        r.update_scope(&s).unwrap();

        assert_fd_listeners_count(&r, scope_fd, 0);

        // add duplicated global fd
        s.add_input(1, cb).unwrap();
        assert!(matches!(r.update_scope(&s), Err(Error::DuplicatedFdWithDifferentSource)));

        // add duplicated fd in another scope
        let mut ns = ScopedReactor::<NoopState>::new();
        ns.add_input(1, cb).unwrap();
        assert!(matches!(r.update_scope(&ns), Err(Error::DuplicatedFdWithDifferentSource)));
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

    fn write_until_full(fd: RawFd) -> usize {
        let data: [u8; 1000] = [20; 1000];
        let mut written = 0;
        loop {
            match write(fd, &data) {
                Ok(size) => {
                    written += size
                }
                Err(nix::errno::Errno::EAGAIN) => {
                    break;
                }
                Err(e) => {
                    panic!("{}", e);
                }
            }
        }
        return written;
    }

    #[test]
    fn callbacks() {
        struct State {
            p1_wr: RawFd,
            p1_rd: RawFd,
            p2_wr: RawFd,
            p2_rd: RawFd,
            buffer: Vec<u8>,
            writen: usize,
            read: usize,
            write_overflow: bool,
            pipe_size: usize,
        }

        let mut state = {
            let (p1_rd, p1_wr) = pipe2(OFlag::O_NONBLOCK).unwrap();
            let (p2_rd, p2_wr) = pipe2(OFlag::O_NONBLOCK).unwrap();

            State {
                p1_wr,
                p1_rd,
                p2_wr,
                p2_rd,
                buffer: Vec::with_capacity(200),
                writen: 0,
                read: 0,
                write_overflow: false,
                pipe_size: fcntl(p1_rd, FcntlArg::F_GETPIPE_SZ).unwrap() as usize,
            }
        };

        let mut r = Reactor::<State>::create();

        // add some more when pipe is available
        r.add_output(state.p1_wr, |r, state| {
            // fill p1_wr
            state.writen += write_until_full(state.p1_wr);

            if state.writen > 2 * state.pipe_size {
                r.remove(state.p1_wr, ReactorEvent::ReadyWrite).unwrap();
            }
        }).unwrap();

        // forward all data
        r.add_input(state.p1_rd, |reactor, state| {
            let ret = forward_fd(state.p1_rd, &mut state.buffer, state.p2_wr).unwrap();

            if ret.try_again {
                // assert
                state.write_overflow = true;
                // write all data
                reactor.add_output(state.p2_wr, |r, s| {
                    let ret = forward_fd(s.p1_rd, &mut s.buffer, s.p2_wr).unwrap();
                    if !ret.try_again {
                        // remove wr fd when done
                        r.remove(s.p2_wr, ReactorEvent::ReadyWrite).unwrap();
                    }
                }).unwrap();

                reactor.add_input(state.p2_rd, |r, s| {
                    let mut buf: [u8; 100] = [0; 100];
                    loop {
                        match read(s.p2_rd, &mut buf) {
                            Err(nix::errno::Errno::EAGAIN) => {
                                if s.read == s.writen {
                                    r.shutdown();
                                }
                                break;
                            }
                            Ok(size) => {
                                s.read += size;
                            }
                            Err(e) => {
                                panic!("{}", e);
                            }
                        }
                    }
                }).unwrap();
            }
        }).unwrap();

        reactor_run!(r, &mut state).unwrap();

        assert_eq!(state.read, state.writen);
        assert!(state.write_overflow);
    }
}
