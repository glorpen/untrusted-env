use std::borrow::BorrowMut;
use std::fs::File;
use std::io::{Error, ErrorKind, Read, stderr, stdout, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use libc::{STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::epoll::{epoll_create, epoll_ctl, epoll_wait, EpollEvent, EpollFlags, EpollOp};
use nix::unistd::{close, dup2};

#[derive(Debug)]
pub enum IpcError {
    UnhandledFd,
    IoError(Error),
    Errno(Errno),
}

struct Messages {
    endpoint_w: File,
    endpoint_r: File,
}

struct Stdio {
    stdout: File,
    stderr: File,
    stdin: File,
}

pub struct ChildSide {
    parent_io: Stdio,
    messages: Messages,
}

pub struct ParentSide {
    child_io: Stdio,
    messages: Messages,
    stdin: File,
    pending_stdin: Vec<u8>,
}

pub struct Api {
    pub parent: ParentSide,
    pub child: ChildSide,
}

pub struct Reactor {
    epoll_fd: RawFd,
}

pub trait ReactorHandler: Sized {
    fn get_watchable_read_files(&self) -> Vec<&File>;

    fn get_total_watchable_files(&self) -> usize;

    fn on_fd_event(&mut self, reactor: &Reactor, fd: RawFd) -> Result<(), IpcError>;

    fn close(&self) -> Result<(), Error>;
}

impl Reactor {
    pub fn create() -> Reactor {
        Reactor {
            epoll_fd: epoll_create().unwrap()
        }
    }

    pub fn add_watchable_write_files(&self, f_write: &File) -> Result<(), Error> {
        let mut event = Some(EpollEvent::new(EpollFlags::EPOLLOUT, 0));
        let ctl_op = epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, f_write.as_raw_fd(), event.borrow_mut());

        if ctl_op.is_err() && ctl_op.err().unwrap() == Errno::EEXIST {
            return Ok(());
        }

        ctl_op?;

        return Ok(());
    }

    pub fn remove_watchable_file(&self, f: &File) -> Result<(), Error> {
        let ctl_op = epoll_ctl(self.epoll_fd, EpollOp::EpollCtlDel, f.as_raw_fd(), None);

        if ctl_op.is_err() && ctl_op.err().unwrap() == Errno::ENOENT {
            return Ok(());
        }

        return Ok(());
    }

    pub fn run(&mut self, handler: &mut impl ReactorHandler) -> Result<(), IpcError> {
        let mut event_in = Some(EpollEvent::new(EpollFlags::EPOLLIN, 0));
        let read_files = handler.get_watchable_read_files();
        let mut events: Vec<EpollEvent> = vec![EpollEvent::empty(); handler.get_total_watchable_files()];

        for read_file in read_files {
            epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, read_file.as_raw_fd(), event_in.borrow_mut())
                .expect("epoll ctl");
        }

        // when writing: write nonblocking until EAGAIN, add EPOLLOUT to watchlist, repeat until done sending, remove EPOLLOUT

        loop {
            let events_count = epoll_wait(self.epoll_fd, &mut events, 1000).expect("epoll wait");
            for i in 0..events_count {
                let fd = events[i].data() as RawFd;

                // TODO: gracefully handle errors
                handler.on_fd_event(self, fd).expect("fd event");
            }
        }
    }

    fn forward_data_sync(&self, f_in: &mut File, mut f_out: impl Write) -> Result<(), Error> {
        let mut buffer = Vec::new();

        loop {
            let read_op = f_in.read_to_end(&mut buffer);

            if read_op.is_err() {
                if read_op.as_ref().err().unwrap().kind() == ErrorKind::WouldBlock {
                    break;
                }
                read_op?;
            }

            f_out.write_all(buffer.as_slice())?;
            buffer.clear();
        }

        return Ok(());
    }

    fn forward_data(&self, f_in: &mut File, send_buffer: &mut Vec<u8>, f_out: &mut File) -> Result<(), Error> {
        loop {
            let read_op = f_in.read_to_end(send_buffer);

            if read_op.is_err() && read_op.as_ref().err().unwrap().kind() != ErrorKind::WouldBlock {
                read_op?;
            }

            let write_op = f_out.write(send_buffer.as_slice());

            if write_op.is_ok() {
                send_buffer.splice(0..write_op.unwrap(), []);

                if send_buffer.is_empty() {
                    break;
                }
            } else {
                if write_op.as_ref().err().unwrap().kind() == ErrorKind::WouldBlock {
                    self.add_watchable_write_files(f_out)?;
                    return Ok(());
                } else {
                    write_op?;
                }
            }
        }

        self.remove_watchable_file(f_out)?;

        return Ok(());
    }
}

fn fd_as_file(fd: RawFd) -> File {
    unsafe { File::from_raw_fd(fd) }
}

impl Api {
    //noinspection DuplicatedCode
    pub fn init() -> Api {
        let (parent_api_rd, child_api_wr) = nix::unistd::pipe2(OFlag::O_NONBLOCK).unwrap();
        let (parent_api_wr, child_api_rd) = nix::unistd::pipe2(OFlag::O_NONBLOCK).unwrap();

        let (parent_stdout_rd, child_stdout_wr) = nix::unistd::pipe().unwrap();
        fcntl(parent_stdout_rd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let (parent_stdin_wr, child_stdin_rd) = nix::unistd::pipe().unwrap();
        fcntl(parent_stdin_wr, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();
        let (parent_stderr_rd, child_stderr_wr) = nix::unistd::pipe().unwrap();
        fcntl(parent_stderr_rd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).unwrap();


        Api {
            parent: ParentSide {
                messages: Messages {
                    endpoint_w: fd_as_file(parent_api_wr),
                    endpoint_r: fd_as_file(parent_api_rd),
                },
                child_io: Stdio {
                    stdout: fd_as_file(parent_stdout_rd),
                    stderr: fd_as_file(parent_stderr_rd),
                    stdin: fd_as_file(parent_stdin_wr),
                },
                stdin: fd_as_file(STDIN_FILENO),

                pending_stdin: Vec::new(),
            },
            child: ChildSide {
                messages: Messages {
                    endpoint_w: fd_as_file(child_api_wr),
                    endpoint_r: fd_as_file(child_api_rd),
                },
                parent_io: Stdio {
                    stdout: fd_as_file(child_stdout_wr),
                    stderr: fd_as_file(child_stderr_wr),
                    stdin: fd_as_file(child_stdin_rd),
                },
            },
        }
    }
}

impl From<Error> for IpcError {
    fn from(err: Error) -> Self {
        IpcError::IoError(err)
    }
}

impl From<Errno> for IpcError {
    fn from(err: Errno) -> Self {
        IpcError::Errno(err)
    }
}

impl ReactorHandler for ParentSide {
    fn get_watchable_read_files(&self) -> Vec<&File> {
        vec![
            &self.stdin,
            &self.messages.endpoint_r,
            &self.child_io.stdout,
            &self.child_io.stderr,
        ]
    }

    fn get_total_watchable_files(&self) -> usize {
        6
    }

    fn on_fd_event(&mut self, reactor: &Reactor, fd: RawFd) -> Result<(), IpcError> {
        if fd == self.stdin.as_raw_fd() {
            reactor.forward_data(
                &mut self.stdin, &mut self.pending_stdin, &mut self.child_io.stdin,
            )?;
        } else if fd == self.child_io.stderr.as_raw_fd() {
            reactor.forward_data_sync(&mut self.child_io.stderr, stderr())?;
        } else if fd == self.child_io.stdout.as_raw_fd() {
            reactor.forward_data_sync(&mut self.child_io.stdout, stdout())?;
        }

        return Err(IpcError::UnhandledFd);
    }

    fn close(&self) -> Result<(), Error> {
        close(self.child_io.stdin.as_raw_fd())?;
        close(self.child_io.stdout.as_raw_fd())?;
        close(self.child_io.stderr.as_raw_fd())?;
        close(self.stdin.as_raw_fd())?;
        close(self.messages.endpoint_r.as_raw_fd())?;
        close(self.messages.endpoint_w.as_raw_fd())?;
        return Ok(());
    }
}

impl ReactorHandler for ChildSide {
    fn get_watchable_read_files(&self) -> Vec<&File> {
        vec![
            &self.messages.endpoint_r
        ]
    }

    fn get_total_watchable_files(&self) -> usize {
        2
    }

    fn on_fd_event(&mut self, reactor: &Reactor, fd: RawFd) -> Result<(), IpcError> {
        // messages api
        Err(IpcError::UnhandledFd)
    }

    fn close(&self) -> Result<(), Error> {
        close(self.parent_io.stdin.as_raw_fd())?;
        close(self.parent_io.stdout.as_raw_fd())?;
        close(self.parent_io.stderr.as_raw_fd())?;
        close(self.messages.endpoint_r.as_raw_fd())?;
        close(self.messages.endpoint_w.as_raw_fd())?;
        return Ok(());
    }
}

impl ChildSide {
    pub fn setup_stdio(&mut self) -> Result<(), Error> {
        fn adjust_fd(src: &mut File, current_fd: RawFd) -> Result<(), Error> {
            dup2(src.as_raw_fd(), current_fd)?;
            close(src.as_raw_fd())?;
            *src = fd_as_file(current_fd);

            return Ok(());
        }

        adjust_fd(&mut self.parent_io.stdin, STDIN_FILENO)?;
        adjust_fd(&mut self.parent_io.stdout, STDOUT_FILENO)?;
        adjust_fd(&mut self.parent_io.stderr, STDERR_FILENO)?;

        return Ok(());
    }
}

// pub fn run(reactor: &mut Reactor, handler: &impl ReactorHandler) {
//     reactor.run(handler);
// }