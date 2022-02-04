// Copyright 2021-2022 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

#[macro_use]
extern crate derive_new;
#[macro_use]
extern crate derive_setters;
#[macro_use]
extern crate log;
#[macro_use]
extern crate thiserror;

pub mod checksum;
mod checksum_system;
mod concatenator;
mod range;

pub use self::checksum_system::*;
pub use self::concatenator::*;

use filetime::FileTime;
use futures::{
    prelude::*,
    stream::{self, StreamExt},
};
use http::StatusCode;
use httpdate::HttpDate;
use isahc::config::Configurable;
use isahc::{AsyncBody, HttpClient as Client, Request, Response};
use numtoa::NumToA;
use std::{
    fmt::Debug,
    future::Future,
    io,
    num::{NonZeroU16, NonZeroU32, NonZeroU64},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub type EventSender<Data> = mpsc::UnboundedSender<(Arc<Path>, Data, FetchEvent)>;
pub type Output<T> = (Arc<Path>, Result<T, Error>);

/// An error from the asynchronous file fetcher.
#[derive(Debug, Error)]
pub enum Error {
    #[error("task was cancelled")]
    Cancelled,
    #[error("http client error")]
    Client(isahc::Error),
    #[error("unable to concatenate fetched parts")]
    Concatenate(#[source] io::Error),
    #[error("unable to create file")]
    FileCreate(#[source] io::Error),
    #[error("unable to set timestamp on {:?}", _0)]
    FileTime(Arc<Path>, #[source] io::Error),
    #[error("content length is an invalid range")]
    InvalidRange(#[source] io::Error),
    #[error("unable to remove file with bad metadata")]
    MetadataRemove(#[source] io::Error),
    #[error("destination has no file name")]
    Nameless,
    #[error("unable to open fetched part")]
    OpenPart(Arc<Path>, #[source] io::Error),
    #[error("destination lacks parent")]
    Parentless,
    #[error("connection timed out")]
    TimedOut,
    #[error("error writing to file")]
    Write(#[source] io::Error),
    #[error("failed to rename partial to destination")]
    Rename(#[source] io::Error),
    #[error("server responded with an error: {}", _0)]
    Status(StatusCode),
}

impl From<isahc::Error> for Error {
    fn from(e: isahc::Error) -> Self {
        Self::Client(e)
    }
}

/// Information about a source being fetched.
#[derive(Debug, Setters)]
pub struct Source {
    /// URLs whereby the file can be found.
    #[setters(skip)]
    pub urls: Arc<[Box<str>]>,

    /// Where the file shall ultimately be fetched to.
    #[setters(skip)]
    pub dest: Arc<Path>,

    /// Optional location to store the partial file
    #[setters(strip_option)]
    #[setters(into)]
    pub part: Option<Arc<Path>>,
}

impl Source {
    pub fn new(urls: impl Into<Arc<[Box<str>]>>, dest: impl Into<Arc<Path>>) -> Self {
        Self {
            urls: urls.into(),
            dest: dest.into(),
            part: None,
        }
    }
}

/// Events which are submitted by the fetcher.
#[derive(Debug)]
pub enum FetchEvent {
    /// Signals that this file was already fetched.
    AlreadyFetched,
    /// States that we know the length of the file being fetched.
    ContentLength(u64),
    /// Notifies that the file has been fetched.
    Fetched,
    /// Notifies that a file is being fetched.
    Fetching,
    /// Reports the amount of bytes that have been read for a file.
    Progress(u64),
    /// Reports that a part of a file is being fetched.
    PartFetching(u64),
    /// Reports that a part has been fetched.
    PartFetched(u64),
}

/// An asynchronous file fetcher for clients fetching files.
///
/// The futures generated by the fetcher are compatible with single and multi-threaded
/// runtimes, allowing you to choose between the runtime that works best for your
/// application. A single-threaded runtime is generally recommended for fetching files,
/// as your network connection is unlikely to be faster than a single CPU core.
#[derive(new, Setters)]
pub struct Fetcher<Data> {
    #[setters(skip)]
    client: Client,

    /// When set, cancels any active operations.
    #[new(default)]
    #[setters(strip_option)]
    cancel: Option<Arc<AtomicBool>>,

    /// The number of concurrent connections to sustain per file being fetched.
    #[new(default)]
    connections_per_file: Option<NonZeroU16>,

    /// The number of attempts to make when a request fails.
    #[new(value = "unsafe { NonZeroU16::new_unchecked(3) } ")]
    retries: NonZeroU16,

    /// The maximum size of a part file when downloading in parts.
    #[new(value = "unsafe { NonZeroU32::new_unchecked(2 * 1024 * 1024) }")]
    max_part_size: NonZeroU32,

    /// The time to wait between chunks before giving up.
    #[new(default)]
    #[setters(strip_option)]
    timeout: Option<Duration>,

    /// Holds a sender for submitting events to.
    #[new(default)]
    #[setters(into)]
    #[setters(strip_option)]
    events: Option<Arc<EventSender<Arc<Data>>>>,
}

impl<Data> Default for Fetcher<Data> {
    fn default() -> Self {
        Self::new(
            Client::builder()
                .low_speed_timeout(1, std::time::Duration::from_secs(15))
                .redirect_policy(isahc::config::RedirectPolicy::Follow)
                .build()
                .expect("failed to build HTTP client"),
        )
    }
}

impl<Data: Send + Sync + 'static> Fetcher<Data> {
    /// Finalizes the fetcher to prepare it for fetch tasks.
    pub fn build(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Build a stream that will perform fetches when polled.
    pub fn requests_stream(
        self: Arc<Self>,
        inputs: impl Stream<Item = (Source, Arc<Data>)> + Unpin + Send + 'static,
    ) -> impl Stream<
        Item = impl Future<Output = (Arc<Path>, Arc<Data>, Result<(), Error>)> + Send + 'static,
    > + Send
           + Unpin
           + 'static {
        inputs.map(move |(source, extra)| {
            let fetcher = self.clone();

            async move {
                let Source {
                    dest, urls, part, ..
                } = source;

                fetcher.send(|| (dest.clone(), extra.clone(), FetchEvent::Fetching));

                let result = match part {
                    Some(part) => match fetcher
                        .clone()
                        .request(urls, part.clone(), extra.clone())
                        .await
                    {
                        Ok(()) => fs::rename(&*part, &*dest).await.map_err(Error::Rename),
                        Err(why) => Err(why),
                    },
                    None => {
                        fetcher
                            .clone()
                            .request(urls, dest.clone(), extra.clone())
                            .await
                    }
                };

                fetcher.send(|| (dest.clone(), extra.clone(), FetchEvent::Fetched));

                (dest, extra, result)
            }
        })
    }

    /// Request a file from one or more URIs.
    ///
    /// At least one URI must be provided as a source for the file. Each additional URI
    /// serves as a mirror for failover and load-balancing purposes.
    pub async fn request(
        self: Arc<Self>,
        uris: Arc<[Box<str>]>,
        to: Arc<Path>,
        extra: Arc<Data>,
    ) -> Result<(), Error> {
        remove_parts(&to).await;

        let result = match self
            .clone()
            .inner_request(uris.clone(), to.clone(), extra.clone())
            .await
        {
            Ok(()) => Ok(()),
            Err(mut why) => {
                for _ in 1..self.retries.get() {
                    match self
                        .clone()
                        .inner_request(uris.clone(), to.clone(), extra.clone())
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(cause) => why = cause,
                    }
                }

                Err(why)
            }
        };

        remove_parts(&to).await;

        result
    }

    async fn inner_request(
        self: Arc<Self>,
        uris: Arc<[Box<str>]>,
        to: Arc<Path>,
        extra: Arc<Data>,
    ) -> Result<(), Error> {
        let mut modified = None;
        let mut length = None;
        let mut resume = None;

        // If the file already exists, validate that it is the same.
        if to.exists() {
            if let Some(response) = head(&self.client, &*uris[0]).await? {
                length = response.content_length();
                modified = response.last_modified();

                if let (Some(length), Some(last_modified)) = (length, modified) {
                    match fs::metadata(to.as_ref()).await {
                        Ok(metadata) => {
                            let modified = metadata.modified().map_err(Error::Write)?;
                            let ts = modified
                                .duration_since(UNIX_EPOCH)
                                .expect("time went backwards");

                            if metadata.len() == length {
                                if ts.as_secs() == date_as_timestamp(last_modified) {
                                    self.send(|| (to, extra.clone(), FetchEvent::AlreadyFetched));
                                    return Ok(());
                                } else {
                                    let _ = fs::remove_file(to.as_ref())
                                        .await
                                        .map_err(Error::MetadataRemove)?;
                                }
                            } else {
                                resume = Some(metadata.len());
                            }
                        }
                        Err(why) => {
                            error!("failed to fetch metadata of {:?}: {}", to, why);
                            fs::remove_file(to.as_ref())
                                .await
                                .map_err(Error::MetadataRemove)?;
                        }
                    }
                }
            }
        }

        // If set, this will use multiple connections to download a file in parts.
        if let Some(connections) = self.connections_per_file {
            if let Some(response) = head(&self.client, &*uris[0]).await? {
                modified = response.last_modified();
                length = match length {
                    Some(length) => Some(length),
                    None => response.content_length(),
                };

                let resume = resume.unwrap_or(0);

                if let Some(length) = length {
                    if supports_range(&self.client, &*uris[0], resume, Some(length)).await? {
                        self.send(|| {
                            (to.clone(), extra.clone(), FetchEvent::ContentLength(length))
                        });

                        if resume != 0 {
                            self.send(|| (to.clone(), extra.clone(), FetchEvent::Progress(resume)));
                        }

                        self.get_many(
                            length,
                            connections.get(),
                            uris,
                            to.clone(),
                            modified,
                            resume,
                            extra,
                        )
                        .await?;

                        if let Some(modified) = modified {
                            let filetime =
                                FileTime::from_unix_time(date_as_timestamp(modified) as i64, 0);
                            filetime::set_file_times(&to, filetime, filetime)
                                .map_err(move |why| Error::FileTime(to, why))?;
                        }

                        return Ok(());
                    }
                }
            }
        }

        if let Some(length) = length {
            self.send(|| (to.clone(), extra.clone(), FetchEvent::ContentLength(length)));
        }

        if let Some(r) = resume {
            if let Some(length) = length {
                if r > length {
                    resume = None;
                }
            }
        }

        let mut request = Request::get(&*uris[0]);

        if let Some(r) = resume {
            match supports_range(&self.client, &*uris[0], r, length).await {
                Ok(true) => {
                    request = request.header("Range", range::to_string(r, length));
                }
                _ => resume = None,
            }
        }

        let resume = resume.unwrap_or(0);

        let path = match self
            .get(
                &mut modified,
                request,
                to.clone(),
                length,
                resume,
                extra.clone(),
            )
            .await
        {
            Ok(path) => path,
            Err(Error::Status(StatusCode::NOT_MODIFIED)) => to,
            // Server does not support if-modified-since
            Err(Error::Status(StatusCode::NOT_IMPLEMENTED)) => {
                let request = Request::get(&*uris[0]);
                self.get(&mut modified, request, to, length, resume, extra)
                    .await?
            }
            Err(why) => return Err(why),
        };

        if let Some(modified) = modified {
            let filetime = FileTime::from_unix_time(date_as_timestamp(modified) as i64, 0);
            filetime::set_file_times(&path, filetime, filetime)
                .map_err(move |why| Error::FileTime(path, why))?;
        }

        Ok(())
    }

    async fn get(
        &self,
        modified: &mut Option<HttpDate>,
        request: http::request::Builder,
        to: Arc<Path>,
        length: Option<u64>,
        offset: u64,
        extra: Arc<Data>,
    ) -> Result<Arc<Path>, Error> {
        let request = request.body(()).expect("failed to build request");

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(offset != 0)
            .truncate(offset == 0)
            .open(to.as_ref())
            .await
            .map_err(Error::FileCreate)?;

        if let Some(length) = length {
            file.set_len(length).await.map_err(Error::Write)?;
        }

        let initial_response = if let Some(duration) = self.timeout {
            timed(
                duration,
                Box::pin(async { self.client.send_async(request).await.map_err(Error::from) }),
            )
            .await??
        } else {
            self.client.send_async(request).await?
        };

        if initial_response.status() == StatusCode::NOT_MODIFIED {
            return Ok(to);
        }

        let response = &mut validate(initial_response)?;

        if modified.is_none() {
            *modified = response.last_modified();
        }

        let mut buffer = vec![0u8; 8 * 1024];
        let mut read;

        loop {
            if self.cancelled() {
                return Err(Error::Cancelled);
            }

            read = {
                let reader = async {
                    response
                        .body_mut()
                        .read(&mut buffer)
                        .await
                        .map_err(Error::Write)
                };

                futures::pin_mut!(reader);

                match self.timeout {
                    Some(duration) => timed(duration, reader).await??,
                    None => reader.await?,
                }
            };

            if read == 0 {
                break;
            } else {
                self.send(|| (to.clone(), extra.clone(), FetchEvent::Progress(read as u64)));

                file.write_all(&buffer[..read])
                    .await
                    .map_err(Error::Write)?;
            }
        }

        let _ = file.flush().await;

        Ok(to)
    }

    async fn get_many(
        self: Arc<Self>,
        length: u64,
        concurrent: u16,
        uris: Arc<[Box<str>]>,
        to: Arc<Path>,
        mut modified: Option<HttpDate>,
        offset: u64,
        extra: Arc<Data>,
    ) -> Result<(), Error> {
        let parent = to.parent().ok_or(Error::Parentless)?;
        let filename = to.file_name().ok_or(Error::Nameless)?;

        let mut buf = [0u8; 20];

        // The destination which parts will be concatenated to.
        let concatenated_file = &mut tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(offset != 0)
            .truncate(offset == 0)
            .open(to.as_ref())
            .await
            .map_err(Error::FileCreate)?;

        let max_part_size =
            NonZeroU64::new(self.max_part_size.get() as u64).expect("max part size is 0");

        let to_ = to.clone();
        let parts = stream::iter(range::generate(length, max_part_size, offset).enumerate())
            // Generate a future for fetching each part that a range describes.
            .map(move |(partn, (range_start, range_end))| {
                let uri = uris[partn % uris.len()].clone();

                let part_path = {
                    let mut new_filename = filename.to_os_string();
                    new_filename.push(&[".part", partn.numtoa_str(10, &mut buf)].concat());
                    parent.join(new_filename)
                };

                let fetcher = self.clone();
                let to = to_.clone();
                let extra = extra.clone();

                async move {
                    let range = range::to_string(range_start, Some(range_end));

                    fetcher.send(|| {
                        (
                            to.clone(),
                            extra.clone(),
                            FetchEvent::PartFetching(partn as u64),
                        )
                    });

                    let request = Request::get(&*uri).header("range", range.as_str());

                    let result = fetcher
                        .get(
                            &mut modified,
                            request,
                            part_path.into(),
                            Some(range_end - range_start),
                            0,
                            extra.clone(),
                        )
                        .await;

                    fetcher.send(|| (to, extra.clone(), FetchEvent::PartFetched(partn as u64)));

                    result
                }
            })
            // Ensure that only this many connections are happenning concurrently at a
            // time
            .buffered(concurrent as usize)
            // This type exploded the stack, and therefore needs to be boxed
            .boxed();

        concatenator(concatenated_file, parts).await?;

        if let Some(modified) = modified {
            let filetime = FileTime::from_unix_time(date_as_timestamp(modified) as i64, 0);
            filetime::set_file_times(&to, filetime, filetime)
                .map_err(|why| Error::FileTime(to, why))?;
        }

        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map_or(false, |cancel| cancel.load(Ordering::SeqCst))
    }

    fn send(&self, event: impl FnOnce() -> (Arc<Path>, Arc<Data>, FetchEvent)) {
        if let Some(sender) = self.events.as_ref() {
            let _ = sender.send(event());
        }
    }
}

pub async fn head(client: &Client, uri: &str) -> Result<Option<Response<AsyncBody>>, Error> {
    let request = Request::head(uri).body(()).unwrap();

    match validate(client.send_async(request).await?).map(Some) {
        result @ Ok(_) => result,
        Err(Error::Status(StatusCode::NOT_MODIFIED))
        | Err(Error::Status(StatusCode::NOT_IMPLEMENTED)) => Ok(None),
        Err(other) => Err(other),
    }
}

pub async fn supports_range(
    client: &Client,
    uri: &str,
    resume: u64,
    length: Option<u64>,
) -> Result<bool, Error> {
    let request = Request::head(uri)
        .header("Range", range::to_string(resume, length).as_str())
        .body(())
        .unwrap();

    let response = client.send_async(request).await?;

    if response.status() == StatusCode::PARTIAL_CONTENT {
        if let Some(header) = response.headers().get("Content-Range") {
            if let Ok(header) = header.to_str() {
                if header.starts_with(&format!("bytes {}-", resume)) {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    } else {
        validate(response).map(|_| false)
    }
}

pub async fn timed<F, T>(duration: Duration, future: F) -> Result<T, Error>
where
    F: Future<Output = T> + Unpin,
{
    let timeout = async move {
        tokio::time::sleep(duration).await;
        Err(Error::TimedOut)
    };

    let result = async move { Ok(future.await) };

    futures::pin_mut!(timeout);
    futures::pin_mut!(result);

    futures::future::select(timeout, result)
        .await
        .factor_first()
        .0
}

fn validate(response: Response<AsyncBody>) -> Result<Response<AsyncBody>, Error> {
    let status = response.status();

    if status.is_informational() || status.is_success() {
        Ok(response)
    } else {
        Err(Error::Status(status))
    }
}

trait ResponseExt {
    fn content_length(&self) -> Option<u64>;
    fn last_modified(&self) -> Option<HttpDate>;
}

impl ResponseExt for Response<AsyncBody> {
    fn content_length(&self) -> Option<u64> {
        let header = self.headers().get("content-length")?;
        header.to_str().ok()?.parse::<u64>().ok()
    }

    fn last_modified(&self) -> Option<HttpDate> {
        let header = self.headers().get("last-modified")?;
        httpdate::parse_http_date(header.to_str().ok()?)
            .ok()
            .map(HttpDate::from)
    }
}

pub fn date_as_timestamp(date: HttpDate) -> u64 {
    SystemTime::from(date)
        .duration_since(UNIX_EPOCH)
        .expect("time backwards")
        .as_secs()
}

/// Cleans up after a process that may have been aborted.
async fn remove_parts(to: &Path) {
    let original_filename = match to.file_name().and_then(|x| x.to_str()) {
        Some(name) => name,
        None => return,
    };

    if let Some(parent) = to.parent() {
        if let Ok(mut dir) = tokio::fs::read_dir(parent).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                if let Some(entry_name) = entry.file_name().to_str() {
                    if let Some(potential_part) = entry_name.strip_prefix(original_filename) {
                        if potential_part.starts_with(".part") {
                            let path = entry.path();
                            let _ = tokio::fs::remove_file(path).await;
                        }
                    }
                }
            }
        }
    }
}
