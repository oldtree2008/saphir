use std::path::PathBuf;

use futures_util::{
    io::SeekFrom,
    stream::Stream,
    task::{Context, Poll},
};
use hyper::body::Bytes;
use tokio::{
    fs::File as TokioFile,
    io::{AsyncRead as TokioAsyncRead, AsyncSeek as TokioAsyncSeek},
    macros::support::Pin,
    prelude::*,
};

use crate::{file::middleware::PathExt, http_context::HttpContext, responder::Responder, response::Builder};
use futures::io::{AsyncRead, AsyncSeek, AsyncSeekExt};

mod conditional_request;
mod content_range;
mod etag;
pub mod middleware;
mod range;
mod range_requests;

pub const MAX_BUFFER: usize = 65534;

pub trait SaphirFile: AsyncRead + AsyncSeek + FileInfo + Sync + Send {}

impl<T: AsyncRead + AsyncSeek + FileInfo + Sync + Send> SaphirFile for T {}

pub trait FileInfo {
    fn get_path(&self) -> &PathBuf;
    fn get_mime(&self) -> Option<&mime::Mime>;
    fn get_size(&self) -> u64;
}

pub struct File {
    inner: Pin<Box<TokioFile>>,
    path: PathBuf,
    mime: Option<mime::Mime>,
    seek_has_started: bool,
}

impl FileInfo for File {
    fn get_path(&self) -> &PathBuf {
        &self.path
    }

    fn get_mime(&self) -> Option<&mime::Mime> {
        self.mime.as_ref()
    }

    fn get_size(&self) -> u64 {
        self.path.size()
    }
}

impl File {
    pub async fn open(path_str: &str) -> tokio::io::Result<File> {
        let path = path_str.to_string();
        match TokioFile::open(path_str).await {
            Ok(file) => Ok(File {
                inner: Box::pin(file),
                path: PathBuf::from(path),
                mime: None,
                seek_has_started: false,
            }),

            Err(e) => Err(e),
        }
    }
}

impl AsyncRead for File {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncSeek for File {
    fn poll_seek(mut self: Pin<&mut Self>, cx: &mut Context<'_>, pos: SeekFrom) -> Poll<io::Result<u64>> {
        if !self.seek_has_started {
            match self.inner.as_mut().start_seek(cx, pos) {
                Poll::Ready(Ok(())) => {
                    self.seek_has_started = true;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        match self.inner.as_mut().poll_complete(cx) {
            Poll::Ready(Ok(res)) => {
                self.seek_has_started = false;
                Poll::Ready(Ok(res))
            }
            Poll::Ready(Err(e)) => {
                self.seek_has_started = false;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Responder for File {
    fn respond_with_builder(self, builder: Builder, _ctx: &HttpContext) -> Builder {
        let mime = if let Some(mime) = &self.get_mime() {
            mime.as_ref().to_string()
        } else {
            self.get_path()
                .mime()
                .unwrap_or_else(|| {
                    if self.get_path().is_dir() {
                        mime::TEXT_HTML_UTF_8
                    } else {
                        mime::TEXT_PLAIN_UTF_8
                    }
                })
                .as_ref()
                .to_string()
        };

        let len = self.get_size();

        let b = match builder.file(self.into()) {
            Ok(b) => b,
            Err((b, _e)) => b.status(500).body("Unable to read file"),
        };

        b.header(http::header::ACCEPT_RANGES, "bytes")
            .header(http::header::CONTENT_TYPE, mime)
            .header(http::header::CONTENT_LENGTH, len)
    }
}

pub struct FileStream {
    inner: Pin<Box<dyn SaphirFile>>,
    buffer: Vec<u8>,
    end_of_file: bool,
    range_len: Option<u64>,
    amount_read: usize,
}

impl FileStream {
    pub fn new<T: SaphirFile + 'static>(inner: T) -> Self {
        FileStream {
            inner: Box::pin(inner),
            buffer: Vec::with_capacity(0),
            end_of_file: false,
            range_len: None,
            amount_read: 0,
        }
    }

    pub async fn set_range(&mut self, range: (u64, u64)) -> io::Result<()> {
        let (start, end) = range;
        self.inner.seek(SeekFrom::Start(start)).await?;
        self.range_len = Some(end - start);
        Ok(())
    }

    pub fn get_size(&self) -> u64 {
        self.inner.get_size()
    }
}

impl From<File> for FileStream {
    fn from(other: File) -> Self {
        FileStream::new(other)
    }
}

impl Stream for FileStream {
    type Item = Result<Bytes, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.end_of_file {
            return Poll::Ready(None);
        }

        if let Some(range_len) = self.range_len {
            let usize_range = range_len as usize;
            let mut buffer = vec![0; usize_range];
            while self.amount_read < usize_range && !self.end_of_file {
                match self.inner.as_mut().poll_read(cx, &mut buffer) {
                    Poll::Ready(Ok(s)) => {
                        if s + self.amount_read <= usize_range {
                            self.buffer.extend_from_slice(&buffer[0..s]);
                            self.amount_read += s;
                            self.end_of_file = s == 0 || self.amount_read == usize_range;
                        } else {
                            let amount_to_read = usize_range - self.amount_read;
                            self.buffer.extend_from_slice(&buffer[0..amount_to_read]);
                            self.end_of_file = true;
                        }
                    }

                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(Box::new(e)))),

                    Poll::Pending => return Poll::Pending,
                }
            }
        } else {
            let mut buffer = vec![0; MAX_BUFFER];
            while self.buffer.len() < MAX_BUFFER && !self.end_of_file {
                match self.inner.as_mut().poll_read(cx, &mut buffer) {
                    Poll::Ready(Ok(s)) => {
                        if s > 0 {
                            self.buffer.extend_from_slice(&buffer[0..s]);
                        }
                        self.end_of_file = s == 0;
                    }

                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(Box::new(e)))),

                    Poll::Pending => return Poll::Pending,
                }
            }
        }

        Poll::Ready(Some(Ok(Bytes::from(std::mem::take(&mut self.buffer)))))
    }
}

impl Responder for FileStream {
    fn respond_with_builder(self, builder: Builder, _ctx: &HttpContext) -> Builder {
        let mime = if let Some(mime) = &self.inner.get_mime() {
            mime.as_ref().to_string()
        } else {
            self.inner
                .get_path()
                .mime()
                .unwrap_or_else(|| {
                    if self.inner.get_path().is_dir() {
                        mime::TEXT_HTML_UTF_8
                    } else {
                        mime::TEXT_PLAIN_UTF_8
                    }
                })
                .as_ref()
                .to_string()
        };

        let len = self.inner.get_size();

        let b = match builder.file(self) {
            Ok(b) => b,
            Err((b, _e)) => b.status(500).body("Unable to read file"),
        };

        b.header(http::header::ACCEPT_RANGES, "bytes")
            .header(http::header::CONTENT_TYPE, mime)
            .header(http::header::CONTENT_LENGTH, len)
    }
}
