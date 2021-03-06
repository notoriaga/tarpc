// Copyright 2019 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

//! A generic Serde-based `Transport` that can serialize anything supported by `tokio-serde` via any medium that implements `AsyncRead` and `AsyncWrite`.

#![deny(missing_docs)]

use futures::{prelude::*, task::*};
use pin_project::pin_project;
use serde::{Deserialize, Serialize};
use std::{error::Error, io, pin::Pin};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_serde::{Framed as SerdeFramed, *};
use tokio_util::codec::{length_delimited::LengthDelimitedCodec, Framed};

/// A transport that serializes to, and deserializes from, a byte stream.
#[pin_project]
pub struct Transport<S, Item, SinkItem, Codec> {
    #[pin]
    inner: SerdeFramed<Framed<S, LengthDelimitedCodec>, Item, SinkItem, Codec>,
}

impl<S, Item, SinkItem, Codec> Transport<S, Item, SinkItem, Codec> {
    /// Returns the inner transport over which messages are sent and received.
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref().get_ref()
    }
}

impl<S, Item, SinkItem, Codec, CodecError> Stream for Transport<S, Item, SinkItem, Codec>
where
    S: AsyncWrite + AsyncRead,
    Item: for<'a> Deserialize<'a>,
    Codec: Deserializer<Item>,
    CodecError: Into<Box<dyn std::error::Error + Send + Sync>>,
    SerdeFramed<Framed<S, LengthDelimitedCodec>, Item, SinkItem, Codec>:
        Stream<Item = Result<Item, CodecError>>,
{
    type Item = io::Result<Item>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<io::Result<Item>>> {
        match self.project().inner.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok::<_, CodecError>(next))) => Poll::Ready(Some(Ok(next))),
            Poll::Ready(Some(Err::<_, CodecError>(e))) => {
                Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::Other, e))))
            }
        }
    }
}

impl<S, Item, SinkItem, Codec, CodecError> Sink<SinkItem> for Transport<S, Item, SinkItem, Codec>
where
    S: AsyncWrite,
    SinkItem: Serialize,
    Codec: Serializer<SinkItem>,
    CodecError: Into<Box<dyn Error + Send + Sync>>,
    SerdeFramed<Framed<S, LengthDelimitedCodec>, Item, SinkItem, Codec>:
        Sink<SinkItem, Error = CodecError>,
{
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        convert(self.project().inner.poll_ready(cx))
    }

    fn start_send(self: Pin<&mut Self>, item: SinkItem) -> io::Result<()> {
        self.project()
            .inner
            .start_send(item)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        convert(self.project().inner.poll_flush(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        convert(self.project().inner.poll_close(cx))
    }
}

fn convert<E: Into<Box<dyn Error + Send + Sync>>>(
    poll: Poll<Result<(), E>>,
) -> Poll<io::Result<()>> {
    poll.map(|ready| ready.map_err(|e| io::Error::new(io::ErrorKind::Other, e)))
}

impl<S, Item, SinkItem, Codec> From<(S, Codec)> for Transport<S, Item, SinkItem, Codec>
where
    S: AsyncWrite + AsyncRead,
    Item: for<'de> Deserialize<'de>,
    SinkItem: Serialize,
    Codec: Serializer<SinkItem> + Deserializer<Item>,
{
    fn from((inner, codec): (S, Codec)) -> Self {
        Transport {
            inner: SerdeFramed::new(Framed::new(inner, LengthDelimitedCodec::new()), codec),
        }
    }
}

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
/// TCP support for generic transport using Tokio.
pub mod tcp {
    use {
        super::*,
        futures::ready,
        std::{marker::PhantomData, net::SocketAddr},
        tokio::net::{TcpListener, TcpStream, ToSocketAddrs},
    };

    mod private {
        use super::*;

        pub trait Sealed {}

        impl<Item, SinkItem, Codec> Sealed for Transport<TcpStream, Item, SinkItem, Codec> {}
    }

    impl<Item, SinkItem, Codec> Transport<TcpStream, Item, SinkItem, Codec> {
        /// Returns the peer address of the underlying TcpStream.
        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            self.inner.get_ref().get_ref().peer_addr()
        }
        /// Returns the local address of the underlying TcpStream.
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.get_ref().get_ref().local_addr()
        }
    }

    /// Returns a new JSON transport that reads from and writes to `io`.
    pub fn new<Item, SinkItem, Codec>(
        io: TcpStream,
        codec: Codec,
    ) -> Transport<TcpStream, Item, SinkItem, Codec>
    where
        Item: for<'de> Deserialize<'de>,
        SinkItem: Serialize,
        Codec: Serializer<SinkItem> + Deserializer<Item>,
    {
        Transport::from((io, codec))
    }

    /// Connects to `addr`, wrapping the connection in a JSON transport.
    pub async fn connect<A, Item, SinkItem, Codec>(
        addr: A,
        codec: Codec,
    ) -> io::Result<Transport<TcpStream, Item, SinkItem, Codec>>
    where
        A: ToSocketAddrs,
        Item: for<'de> Deserialize<'de>,
        SinkItem: Serialize,
        Codec: Serializer<SinkItem> + Deserializer<Item>,
    {
        Ok(new(TcpStream::connect(addr).await?, codec))
    }

    /// Listens on `addr`, wrapping accepted connections in JSON transports.
    pub async fn listen<A, Item, SinkItem, Codec, CodecFn>(
        addr: A,
        codec_fn: CodecFn,
    ) -> io::Result<Incoming<Item, SinkItem, Codec, CodecFn>>
    where
        A: ToSocketAddrs,
        Item: for<'de> Deserialize<'de>,
        Codec: Serializer<SinkItem> + Deserializer<Item>,
        CodecFn: Fn() -> Codec,
    {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        Ok(Incoming {
            listener,
            codec_fn,
            local_addr,
            ghost: PhantomData,
        })
    }

    /// A [`TcpListener`] that wraps connections in [transports](Transport).
    #[pin_project]
    #[derive(Debug)]
    pub struct Incoming<Item, SinkItem, Codec, CodecFn> {
        listener: TcpListener,
        local_addr: SocketAddr,
        codec_fn: CodecFn,
        ghost: PhantomData<(Item, SinkItem, Codec)>,
    }

    impl<Item, SinkItem, Codec, CodecFn> Incoming<Item, SinkItem, Codec, CodecFn> {
        /// Returns the address being listened on.
        pub fn local_addr(&self) -> SocketAddr {
            self.local_addr
        }
    }

    impl<Item, SinkItem, Codec, CodecFn> Stream for Incoming<Item, SinkItem, Codec, CodecFn>
    where
        Item: for<'de> Deserialize<'de>,
        SinkItem: Serialize,
        Codec: Serializer<SinkItem> + Deserializer<Item>,
        CodecFn: Fn() -> Codec,
    {
        type Item = io::Result<Transport<TcpStream, Item, SinkItem, Codec>>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let next =
                ready!(Pin::new(&mut self.as_mut().project().listener.incoming()).poll_next(cx)?);
            Poll::Ready(next.map(|conn| Ok(new(conn, (self.codec_fn)()))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Transport;
    use assert_matches::assert_matches;
    use futures::{task::*, Sink, Stream};
    use pin_utils::pin_mut;
    use std::{
        io::{self, Cursor},
        pin::Pin,
    };
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio_serde::formats::SymmetricalJson;

    fn ctx() -> Context<'static> {
        Context::from_waker(&noop_waker_ref())
    }

    #[test]
    fn test_stream() {
        struct TestIo(Cursor<&'static [u8]>);

        impl AsyncRead for TestIo {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                AsyncRead::poll_read(Pin::new(self.0.get_mut()), cx, buf)
            }
        }

        impl AsyncWrite for TestIo {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                unreachable!()
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                unreachable!()
            }

            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                unreachable!()
            }
        }

        let data = b"\x00\x00\x00\x18\"Test one, check check.\"";
        let transport = Transport::from((
            TestIo(Cursor::new(data)),
            SymmetricalJson::<String>::default(),
        ));
        pin_mut!(transport);

        assert_matches!(
            transport.poll_next(&mut ctx()),
            Poll::Ready(Some(Ok(ref s))) if s == "Test one, check check.");
    }

    #[test]
    fn test_sink() {
        struct TestIo<'a>(&'a mut Vec<u8>);

        impl<'a> AsyncRead for TestIo<'a> {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                unreachable!()
            }
        }

        impl<'a> AsyncWrite for TestIo<'a> {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                AsyncWrite::poll_write(Pin::new(&mut *self.0), cx, buf)
            }

            fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                AsyncWrite::poll_flush(Pin::new(&mut *self.0), cx)
            }

            fn poll_shutdown(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<io::Result<()>> {
                AsyncWrite::poll_shutdown(Pin::new(&mut *self.0), cx)
            }
        }

        let mut writer = vec![];
        let transport =
            Transport::from((TestIo(&mut writer), SymmetricalJson::<String>::default()));
        pin_mut!(transport);

        assert_matches!(
            transport.as_mut().poll_ready(&mut ctx()),
            Poll::Ready(Ok(()))
        );
        assert_matches!(
            transport
                .as_mut()
                .start_send("Test one, check check.".into()),
            Ok(())
        );
        assert_matches!(transport.poll_flush(&mut ctx()), Poll::Ready(Ok(())));
        assert_eq!(writer, b"\x00\x00\x00\x18\"Test one, check check.\"");
    }
}
