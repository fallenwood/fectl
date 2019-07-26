use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

use futures::{Async, Poll};
use mio;
use mio::unix::EventedFd;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use tokio::io::AsyncRead;
use tokio::prelude::*;
use tokio::reactor::PollEvented2;

pub struct PipeFile {
    read_poll: PollEvented2<Io>,
    write: Io,
    write_poll: PollEvented2<Io>,
}

impl PipeFile {
    pub fn new(read: RawFd, write: RawFd) -> PipeFile {
        PipeFile {
            read_poll: PollEvented2::new(unsafe { Io::from_raw_fd(read) }),
            write: unsafe { Io::from_raw_fd(write) },
            write_poll: PollEvented2::new(unsafe { Io::from_raw_fd(write) }),
        }
    }
}

impl Read for PipeFile {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        match self.read_poll.poll_read(dst) {
            Ok(r) => match r {
                Async::Ready(size) => Ok(size),
                Async::NotReady => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            },
            Err(_) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
        }
    }
}

impl Write for PipeFile {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        match self.write_poll.poll_write(src) {
            Ok(r) => match r {
                Async::Ready(size) => Ok(size),
                Async::NotReady => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
            },
            Err(_) => Err(io::Error::new(io::ErrorKind::WouldBlock, "")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.write).flush()
    }
}

impl AsyncRead for PipeFile {}

impl AsyncWrite for PipeFile {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

/// Manages a FD
#[derive(Debug)]
pub struct Io {
    fd: File,
}

impl Io {
    /// Try to clone the FD
    pub fn try_clone(&self) -> io::Result<Io> {
        Ok(Io {
            fd: self.fd.try_clone()?,
        })
    }
}

impl FromRawFd for Io {
    unsafe fn from_raw_fd(fd: RawFd) -> Io {
        let flags = fcntl(fd, FcntlArg::F_GETFL).unwrap();
        let _ = fcntl(
            fd,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        );

        Io {
            fd: File::from_raw_fd(fd),
        }
    }
}

impl IntoRawFd for Io {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl AsRawFd for Io {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl mio::Evented for Io {
    fn register(
        &self, poll: &mio::Poll, token: mio::Token, interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).register(poll, token, interest, opts)
    }

    fn reregister(
        &self, poll: &mio::Poll, token: mio::Token, interest: mio::Ready,
        opts: mio::PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).deregister(poll)
    }
}

impl Read for Io {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        (&self.fd).read(dst)
    }
}

impl<'a> Read for &'a Io {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        (&self.fd).read(dst)
    }
}

impl Write for Io {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        (&self.fd).write(src)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.fd).flush()
    }
}

impl<'a> Write for &'a Io {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        (&self.fd).write(src)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.fd).flush()
    }
}

impl AsyncRead for Io {}

impl<'a> AsyncRead for &'a Io {}

impl AsyncWrite for Io {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

impl<'a> AsyncWrite for &'a Io {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}
