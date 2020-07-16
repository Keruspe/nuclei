use std::{io, task};
use std::{task::Poll, fs::File, pin::Pin, task::Context};
use super::handle::Handle;
use futures::io::{AsyncRead, AsyncWrite, SeekFrom};
use super::submission_handler::SubmissionHandler;
use std::io::Read;

use std::net::{TcpStream};

#[cfg(unix)]
use std::{mem::ManuallyDrop, os::unix::io::{AsRawFd, RawFd, FromRawFd}, os::unix::prelude::*};
#[cfg(unix)]
use std::os::unix::net::{UnixStream};

use std::future::Future;

use crate::syscore::Processor;
use crate::syscore::*;
use std::sync::Arc;
use futures::{AsyncBufRead, AsyncSeek};
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};

//
// Proxy operations for Future registration via AsyncRead, AsyncWrite and others.
// Linux, windows etc. specific

macro_rules! impl_async_read {
    ($name:ident) => {
        impl AsyncRead for Handle<$name> {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_read(cx, buf)
            }
        }
    }
}

macro_rules! impl_async_write {
    ($name:ident) => {
        impl AsyncWrite for Handle<$name> {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_write(cx, buf)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
                Pin::new(&mut &*Pin::get_mut(self)).poll_close(cx)
            }
        }
    }
}

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl_async_read!(File);
#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl_async_write!(File);

impl_async_read!(TcpStream);
impl_async_write!(TcpStream);

#[cfg(unix)]
impl_async_read!(UnixStream);
#[cfg(unix)]
impl_async_write!(UnixStream);


///////////////////////////////////
///// Non proactive File
///////////////////////////////////

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl AsyncRead for &Handle<File> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_read_file(&file, buf).await?;

            let _ = ManuallyDrop::new(file);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(not(all(feature = "iouring", target_os = "linux")))]
impl AsyncWrite for &Handle<File> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let file = unsafe { File::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_write_file(&file, buf).await?;

            let _ = ManuallyDrop::new(file);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}


///////////////////////////////////
///// IO URING / Proactive / Linux
///////////////////////////////////

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncRead for Handle<File> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut inner = futures::ready!(self.as_mut().poll_fill_buf(cx))?;
        dbg!("BEFORE INNER");
        let len = io::Read::read(&mut inner, buf)?;
        dbg!("AFTER INNER");
        self.consume(len);
        dbg!(len);
        Poll::Ready(Ok(len))
    }
}

#[cfg(all(feature = "iouring", target_os = "linux"))]
const NON_READ: &[u8] = &[];

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncBufRead for Handle<File> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let mut store = &mut self.get_mut().store_file;
        let state = store.as_ref().unwrap();

        state.op_state().replace_with(|x| {
            if *x != Op::Pending {
                Op::Read
            } else {
                Op::Pending
            }
        });

        if *state.op_state().get() == Op::Read {

        }
        store
            .as_mut()
            .map_or_else(|| {
                dbg!("NOTHING");
                Poll::Ready(Ok(NON_READ))
            }, |store_file| {
                let fd = store_file.get_fd();
                let op_state = store_file.op_state();
                // if *op_state.get() == Op::Pending {
                //     store_file.cancel();
                // }

                dbg!("CALLED");

                let (bufp, pos) = store_file.bufpair();

                bufp.fill_buf(|buf| {
                    let fut = Processor::processor_read_file(&fd, buf, *pos);
                    futures_util::pin_mut!(fut);
                    loop {
                        let n = futures::ready!(fut.as_mut().poll(cx)?);
                        *pos += n;
                        break Poll::Ready(Ok(n))
                    }

                    // loop {
                    //     let mut fut = Processor::processor_read_file(&fd, buf, *pos);
                    //     futures_util::pin_mut!(fut);
                    //
                    //     if let Poll::Ready(n) = fut.as_mut().poll(cx)? {
                    //         *pos += n;
                    //         dbg!(*pos);
                    //         op_state.replace_with(|_| Op::Pending);
                    //         // fut = Processor::processor_read_file(&fd, buf, *pos);
                    //         // futures_util::pin_mut!(fut);
                    //         break Poll::Ready(Ok(n))
                    //     } else {
                    //         match *op_state.get() {
                    //             Op::Pending => {
                    //                 op_state.replace_with(|_| Op::Read);
                    //                 break Poll::Pending
                    //             },
                    //             // Op::Pending => break Poll::Ready(Ok(8191)),
                    //             _ => {
                    //                 dbg!("UNEXPECTED");
                    //             }
                    //         }
                    //         // match *op_state.get() {
                    //         //     // Op::Pending => break Poll::Ready(Ok(0)),
                    //         //     Op::Pending => break Poll::Pending,
                    //         //     // Op::Pending => {
                    //         //         // op_state.replace_with(|_| Op::Nothing);
                    //         //         // dbg!("MORE PENDING");
                    //         //         // break Poll::Pending
                    //         //     // },
                    //         //     _ => {}
                    //         // }
                    //     }
                    // }
                    // fut.as_mut().poll(cx)

                    // let fut = Processor::processor_read_file(&fd, buf, *pos);
                    //
                    // futures_util::pin_mut!(fut);
                    //
                    // // dbg!("BEFORE READY");
                    // let n = futures::ready!(fut.as_mut().poll(cx)?);
                    // dbg!(n);
                    // *pos += n;
                    // Poll::Ready(Ok(n))
                })
            })
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        dbg!(amt);
        let mut store = self.get_mut().store_file.as_mut().unwrap();
        store.buf().consume(amt);
    }
}

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncWrite for &Handle<File> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(all(feature = "iouring", target_os = "linux"))]
impl AsyncSeek for Handle<File> {
    fn poll_seek(self: Pin<&mut Self>, cx: &mut Context<'_>, pos: SeekFrom) -> Poll<io::Result<u64>> {
        let mut store = &mut self.get_mut().store_file.as_mut().unwrap();

        let (whence, offset) = match pos {
            io::SeekFrom::Start(n) => {
                *store.pos() = n as usize;
                return Poll::Ready(Ok(*store.pos() as u64));
            }
            io::SeekFrom::Current(n) => (*store.pos(), n),
            io::SeekFrom::End(n)     => {
                let fut = store.poll_file_size();
                futures::pin_mut!(fut);
                (futures::ready!(fut.as_mut().poll(cx))?, n)
            }
        };
        let valid_seek = if offset.is_negative() {
            match whence.checked_sub(offset.abs() as usize) {
                Some(valid_seek) => valid_seek,
                None => {
                    let invalid = io::Error::from(io::ErrorKind::InvalidInput);
                    return Poll::Ready(Err(invalid));
                }
            }
        } else {
            match whence.checked_add(offset as usize) {
                Some(valid_seek) => valid_seek,
                None => {
                    let overflow = io::Error::from_raw_os_error(libc::EOVERFLOW);
                    return Poll::Ready(Err(overflow));
                }
            }
        };
        *store.pos() = valid_seek;
        Poll::Ready(Ok(*store.pos() as u64))
    }
}

///////////////////////////////////
///// TcpStream
///////////////////////////////////

#[cfg(unix)]
impl AsyncRead for &Handle<TcpStream> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_recv(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(unix)]
impl AsyncWrite for &Handle<TcpStream> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { TcpStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_send(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}


#[cfg(unix)]
impl AsyncRead for &Handle<UnixStream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_mut_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { UnixStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };
            let size = Processor::processor_recv(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_read(self, cx, completion_dispatcher)
    }
}

#[cfg(unix)]
impl AsyncWrite for &Handle<UnixStream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let raw_fd = self.as_raw_fd();
        let buf_len = buf.len();
        let buf = buf.as_ptr();

        let completion_dispatcher = async move {
            let sock = unsafe { UnixStream::from_raw_fd(raw_fd) };

            let buf = unsafe { std::slice::from_raw_parts(buf, buf_len) };
            let size = Processor::processor_send(&sock, buf).await?;

            let _ = ManuallyDrop::new(sock);
            Ok(size)
        };

        SubmissionHandler::<Self>::handle_write(self, cx, completion_dispatcher)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}