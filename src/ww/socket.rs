//! [ww_device::ws::Socket] on one already accepted futures-io stream.
//!
//! `ww_device::ws` serves one client per `WsConnection` and accepts it through the socket. Here
//! the listener accepts on the loop and each connection gets its own `WsConnection` over this
//! socket: `accept` hands out the stream once, and after that (the link went down) waits forever,
//! as the connection's task ends then. Moves into wire_weaver with TR-25 (ws-listener).

use std::io;

use futures_util::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub struct StreamSocket<S> {
    stream: Option<S>,
    handed_out: bool,
}

impl<S> StreamSocket<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: Some(stream),
            handed_out: false,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> ww_device::ws::Socket for StreamSocket<S> {
    type Error = io::Error;

    async fn accept(&mut self) {
        if !self.handed_out && self.stream.is_some() {
            self.handed_out = true;
            return;
        }
        // The one client is gone: close it and never accept another.
        self.stream = None;
        std::future::pending::<()>().await;
    }

    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.stream {
            Some(stream) => stream.read(buf).await,
            None => Ok(0),
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.stream {
            Some(stream) => {
                let n = stream.write(bytes).await?;
                stream.flush().await?;
                Ok(n)
            }
            None => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}
