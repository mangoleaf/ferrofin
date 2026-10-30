//! Embedded-attachment extraction.
//!
//! Port of `MediaBrowser.MediaEncoding.Attachments.AttachmentExtractor`. The
//! pure orchestration — resolve the source, reject `mjpeg`/missing attachments,
//! build the per-attachment output path, and serialize extraction per output
//! folder with a keyed lock (C# `AsyncKeyedLocker<string>`) — is ported here.
//! The ffmpeg `-dump_attachment` spawn, the on-disk cache reads, and the
//! item→media-source lookup sit behind the [`AttachmentIo`] and
//! [`MediaSourceResolver`] seams so unit tests inject fakes.

pub mod extractor;

pub use extractor::{AttachmentExtractorImpl, AttachmentIo, MediaSourceResolver, NoopAttachmentIo};
