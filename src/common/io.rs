use crate::common::utils::*;

use std::io::Error;
use std::io::ErrorKind;
use tokio::prelude::*;
use tokio_io::AsyncRead;

use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;

use std::io;
use std::mem;

use futures::{Future, Poll};

use std::net::SocketAddr;

pub fn other_error(desc: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::Other, desc)
}

pub enum HostPort {
    DomainPort(String, u16),
    IPPort(SocketAddr),
}

#[derive(Debug)]
enum State<A> {
    Reading {
        separator: String,
        buf: BytesMut,
        reader: A,
    },
    Empty,
}

pub struct ReadUntilSeparator<A> {
    state: State<A>,
}

impl<A> Future for ReadUntilSeparator<A>
where
    A: AsyncRead,
{
    type Item = (A, Bytes, Bytes);
    type Error = std::io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut r = [0u8; 1024];
        loop {
            match self.state {
                State::Reading {
                    ref mut separator,
                    ref mut buf,
                    ref mut reader,
                } => match reader.poll_read(&mut r) {
                    Ok(Async::NotReady) => {
                        return Ok(Async::NotReady);
                    }
                    Ok(Async::Ready(n)) => {
                        if 0 == n {
                            return Err(Error::from(ErrorKind::ConnectionReset));
                        }
                        buf.reserve(n);
                        buf.put_slice(&r[0..n]);
                        if let Some(pos) = twoway::find_bytes(&buf, separator.as_bytes()) {
                            let wsize = separator.len();
                            let body = buf.split_off(pos + wsize);
                            match mem::replace(&mut self.state, State::Empty) {
                                State::Reading {
                                    separator: _,
                                    buf,
                                    reader,
                                } => return Ok((reader, buf.freeze(), body.freeze()).into()),
                                State::Empty => unreachable!(),
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        return Err(e);
                    }
                },
                State::Empty => panic!("poll ReadUntil after it's done"),
            }
        }
    }
}

pub fn read_until_separator<A>(a: A, separator: &str) -> ReadUntilSeparator<A>
where
    A: AsyncRead,
{
    ReadUntilSeparator {
        state: State::Reading {
            separator: String::from(separator),
            buf: BytesMut::new(),
            reader: a,
        },
    }
}

pub struct PeekExact<R: AsyncRead, T> {
    state: PeekState<R, T>,
}

enum PeekState<R: AsyncRead, T> {
    Reading {
        a: PeekableReader<R>,
        buf: T,
        pos: usize,
    },
    Empty,
}

/// Creates a future which will read exactly enough bytes to fill `buf`,
/// returning an error if EOF is hit sooner.
///
/// The returned future will resolve to both the I/O stream as well as the
/// buffer once the read operation is completed.
///
/// In the case of an error the buffer and the object will be discarded, with
/// the error yielded. In the case of success the object will be destroyed and
/// the buffer will be returned, with all data read from the stream appended to
/// the buffer.
pub fn peek_exact<R, T>(a: R, buf: T) -> PeekExact<R, T>
where
    R: AsyncRead,
    T: AsMut<[u8]>,
{
    PeekExact {
        state: PeekState::Reading {
            a: PeekableReader::new(a),
            buf: buf,
            pos: 0,
        },
    }
}

pub fn peek_exact2<R, T>(r: PeekableReader<R>, buf: T) -> PeekExact<R, T>
where
    R: AsyncRead,
    T: AsMut<[u8]>,
{
    PeekExact {
        state: PeekState::Reading {
            a: r,
            buf: buf,
            pos: 0,
        },
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "early eof")
}

impl<R, T> Future for PeekExact<R, T>
where
    R: AsyncRead,
    T: AsMut<[u8]>,
{
    type Item = (PeekableReader<R>, T);
    type Error = (PeekableReader<R>, io::Error);

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut err: Option<std::io::Error> = None;
        match self.state {
            PeekState::Reading {
                ref mut a,
                ref mut buf,
                ref mut pos,
            } => {
                let buf = buf.as_mut();
                while *pos < buf.len() {
                    match a.poll_peek(&mut buf[*pos..]) {
                        Ok(Async::NotReady) => {
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(n)) => {
                            *pos += n;
                            if n == 0 {
                                err = Some(eof());
                                break;
                            }
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
            }
            PeekState::Empty => panic!("poll a ReadExact after it's done"),
        }
        match err {
            None => match mem::replace(&mut self.state, PeekState::Empty) {
                PeekState::Reading { a, buf, .. } => Ok((a, buf).into()),
                PeekState::Empty => panic!(),
            },
            Some(e) => match mem::replace(&mut self.state, PeekState::Empty) {
                PeekState::Reading { a, buf, .. } => Err((a, e).into()),
                PeekState::Empty => panic!(),
            },
        }
    }
}

pub struct PeekableReader<T> {
    inner: T,
    peek_buf: BytesMut,
}

impl<T> PeekableReader<T>
where
    T: AsyncRead,
{
    pub fn new(c: T) -> Self {
        Self {
            inner: c,
            peek_buf: BytesMut::new(),
        }
    }
    pub fn poll_peek(&mut self, buf: &mut [u8]) -> Poll<usize, std::io::Error> {
        loop {
            let cur_n = self.peek_buf.len();
            if cur_n < buf.len() {
                self.peek_buf.reserve(buf.len() - cur_n);
                unsafe {
                    self.peek_buf.set_len(buf.len());
                }
                match self.inner.poll_read(&mut self.peek_buf[cur_n..]) {
                    Err(e) => {
                        unsafe {
                            self.peek_buf.set_len(cur_n);
                        }
                        return Err(e);
                    }
                    Ok(Async::NotReady) => {
                        unsafe {
                            self.peek_buf.set_len(cur_n);
                        }
                        return Ok(Async::NotReady);
                    }
                    Ok(Async::Ready(n)) => unsafe {
                        self.peek_buf.set_len(cur_n + n);
                    },
                }
            } else {
                buf.copy_from_slice(&self.peek_buf[0..buf.len()]);
                return Ok(Async::Ready(buf.len()));
            }
        }
    }
}

impl<T> AsyncRead for PeekableReader<T> where T: AsyncRead {}

impl<T> Read for PeekableReader<T>
where
    T: AsyncRead,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        debug!("enter peek read {}!", self.peek_buf.len());
        if self.peek_buf.len() == 0 {
            let ret = self.inner.read(buf);
            match &ret {
                Ok(v) => {
                    debug!("enter peek read return {}!", v);
                }
                Err(e) => {
                    debug!("enter peek read return {}!", e);
                }
            };
            return ret;
        }
        let mut n = self.peek_buf.len();
        if n > buf.len() {
            n = buf.len();
        }
        buf[0..n].copy_from_slice(&self.peek_buf[0..n]);
        self.peek_buf.advance(n);
        debug!("enter peek read return ok {}!", n);
        Ok(n)
    }
}

pub struct AsyncReadWriter<R, W> {
    r: R,
    w: W,
}

impl<R, W> AsyncReadWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    pub fn new(a: R, b: W) -> Self {
        Self { r: a, w: b }
    }
}

impl<R, W> Read for AsyncReadWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.r.read(buf)
    }
}

impl<R, W> Write for AsyncReadWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.w.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

impl<R, W> AsyncRead for AsyncReadWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.r.prepare_uninitialized_buffer(buf)
    }

    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.r.read_buf(buf)
    }
}

impl<R, W> AsyncWrite for AsyncReadWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    fn shutdown(&mut self) -> Poll<(), std::io::Error> {
        self.w.shutdown()
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, std::io::Error> {
        self.w.write_buf(buf)
    }
}