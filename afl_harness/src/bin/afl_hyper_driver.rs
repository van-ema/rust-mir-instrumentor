use http_body_util::Full;
use hyper::body::Bytes;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream};
use std::pin::Pin;
use std::task::{Context, Poll};

fn main() {
    let data = afl_harness::read_input();
    if data.is_empty() {
        return;
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let service = service_fn(|_req: Request<hyper::body::Incoming>| async {
            Ok::<_, hyper::Error>(Response::new(Full::<Bytes>::from(
                Bytes::from_static(b"ok"),
            )))
        });

        let io = TokioIo::new(server);
        let conn =
            hyper::server::conn::http1::Builder::new().serve_connection(io, service);

        let writer = write_request_bytes(client, data);

        let _ = tokio::join!(conn, writer);
    });
}

async fn write_request_bytes(mut client: DuplexStream, data: Vec<u8>) {
    let _ = client.write_all(&data).await;
    let _ = client.shutdown().await;
}

struct TokioIo<T>(T);

impl<T> TokioIo<T> {
    fn new(io: T) -> Self {
        TokioIo(io)
    }

    fn pinned(self: Pin<&mut Self>) -> Pin<&mut T> {
        // SAFETY: we do not move the inner value.
        unsafe { self.map_unchecked_mut(|me| &mut me.0) }
    }
}

impl<T> Read for TokioIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let n = unsafe {
            let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
            match AsyncRead::poll_read(self.pinned(), cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };

        unsafe {
            buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> Write for TokioIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        AsyncWrite::poll_write(self.pinned(), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        AsyncWrite::poll_flush(self.pinned(), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        AsyncWrite::poll_shutdown(self.pinned(), cx)
    }

    fn is_write_vectored(&self) -> bool {
        AsyncWrite::is_write_vectored(&self.0)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        AsyncWrite::poll_write_vectored(self.pinned(), cx, bufs)
    }
}
