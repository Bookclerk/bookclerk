//! In-memory sink a `covers` plugin streams downloaded images into.
//!
//! `CoverFetcher.fetchCovers` hands the guest a host-served `Destination`. The
//! guest puts the body for `urls[i]` under the key `i`. This sink is that
//! destination. It is deliberately narrow:
//!
//! - it accepts `put` only, and only for the keys `0..slots`, each once;
//! - it refuses a body over the call's byte cap while reading it, so a guest
//!   can never make the host buffer more than `slots * max_bytes`;
//! - every other `Destination` method is `unsupported`.
//!
//! The bytes stay untrusted: callers sniff, decode and re-encode them before
//! anything stores or serves them.

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;

use bookclerk_integrations::FetchedCover;
use bookclerk_plugin_abi::cover_sink_slot;
use bookclerk_plugin_sdk::{
    ByteRange, CopyResult, CoverImage, Destination, ListOptions, ListPage, ObjectMetadata,
    PluginError, PutResult, ReadResult, WriteOptions,
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Shared state of one `fetchCovers` call's sink.
pub(crate) struct CoverSink {
    /// Largest body accepted for one key, in bytes.
    max_bytes: u32,
    /// Body stored under each index; `None` until the guest puts it.
    slots: RefCell<Vec<Option<Vec<u8>>>>,
}

impl CoverSink {
    /// A sink for `slots` URLs, each body at most `max_bytes`.
    ///
    /// # Arguments
    ///
    /// * `slots` - Number of requested URLs; valid keys are `0..slots`.
    /// * `max_bytes` - Largest body accepted for one key.
    pub(crate) fn new(slots: usize, max_bytes: u32) -> Rc<Self> {
        Rc::new(Self {
            max_bytes,
            slots: RefCell::new(vec![None; slots]),
        })
    }

    /// A boxed `Destination` handle onto this sink for the guest call.
    pub(crate) fn destination(self: &Rc<Self>) -> Box<dyn Destination> {
        Box::new(SinkHandle(Rc::clone(self)))
    }

    /// Pairs the guest's per-URL answers with the bodies it stored.
    ///
    /// A URL gets a body only when the guest said `stored` and a body arrived
    /// under its index. A `stored` answer without a body becomes an error, so
    /// a guest cannot claim success it did not deliver.
    ///
    /// # Arguments
    ///
    /// * `images` - The guest's answers, one per URL in request order (the
    ///   typed client has already checked the count and URLs).
    ///
    /// # Returns
    ///
    /// One [`FetchedCover`] per answer, in order.
    pub(crate) fn take_fetched(&self, images: Vec<CoverImage>) -> Vec<FetchedCover> {
        let mut slots = self.slots.borrow_mut();
        images
            .into_iter()
            .enumerate()
            .map(|(index, image)| {
                let body = slots.get_mut(index).and_then(Option::take);
                let (body, error) = match (image.stored, body) {
                    (true, Some(body)) => (Some(body), None),
                    (true, None) => (None, Some("plugin reported a body it never sent".into())),
                    (false, _) => (
                        None,
                        Some(image.error.unwrap_or_else(|| "not fetched".into())),
                    ),
                };
                FetchedCover {
                    url: image.url,
                    body,
                    content_type: image.content_type,
                    error,
                }
            })
            .collect()
    }

    /// Parses `key` as a slot index that has not been filled yet.
    ///
    /// # Errors
    ///
    /// `invalid_params` for a key that is not a decimal index in range, and
    /// `already_exists`-style `invalid_params` for a slot already filled.
    fn open_slot(&self, key: &str) -> Result<usize, PluginError> {
        let index = cover_sink_slot(key, self.slots.borrow().len())
            .ok_or_else(|| PluginError::invalid_params(format!("cover sink has no key {key:?}")))?;
        if self.slots.borrow()[index].is_some() {
            return Err(PluginError::invalid_params(format!(
                "cover sink key {key:?} was already written"
            )));
        }
        Ok(index)
    }
}

/// `Destination` handle the guest call writes through.
struct SinkHandle(Rc<CoverSink>);

#[async_trait::async_trait(?Send)]
impl Destination for SinkHandle {
    async fn head(&self, _key: &str) -> Result<Option<ObjectMetadata>, PluginError> {
        Err(PluginError::unsupported("head"))
    }

    async fn list(&self, _options: ListOptions) -> Result<ListPage, PluginError> {
        Err(PluginError::unsupported("list"))
    }

    async fn get(&self, _key: &str, _range: Option<ByteRange>) -> Result<ReadResult, PluginError> {
        Err(PluginError::unsupported("get"))
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult, PluginError> {
        if options.stage_only {
            return Err(PluginError::unsupported("staged cover writes"));
        }
        let index = self.0.open_slot(key)?;
        let max = u64::from(self.0.max_bytes);
        if options.content_length.is_some_and(|len| len > max) {
            return Err(too_large(max));
        }
        let mut bytes = Vec::new();
        // One byte past the cap tells an oversized body from one that fits.
        body.take(max.saturating_add(1))
            .read_to_end(&mut bytes)
            .await
            .map_err(|err| PluginError::internal(format!("reading cover body: {err}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max {
            return Err(too_large(max));
        }
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        // A concurrent put for the same key may have landed while this one
        // was reading; the first write wins.
        let mut slots = self.0.slots.borrow_mut();
        if slots[index].is_some() {
            return Err(PluginError::invalid_params(format!(
                "cover sink key {key:?} was already written"
            )));
        }
        slots[index] = Some(bytes);
        Ok(PutResult {
            key: key.to_string(),
            bytes_written: size,
            ..PutResult::default()
        })
    }

    async fn copy(&self, _from: &str, _to: &str) -> Result<CopyResult, PluginError> {
        Err(PluginError::unsupported("copy"))
    }

    async fn delete(&self, _key: &str) -> Result<(), PluginError> {
        Err(PluginError::unsupported("delete"))
    }
}

/// The error for a body over the call's byte cap.
fn too_large(max: u64) -> PluginError {
    PluginError::payload_too_large(format!("cover body exceeds {max} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(bytes: &'static [u8]) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(bytes)
    }

    fn image(url: &str, stored: bool, error: Option<&str>) -> CoverImage {
        CoverImage {
            url: url.into(),
            stored,
            content_type: stored.then(|| "image/png".into()),
            error: error.map(Into::into),
        }
    }

    #[tokio::test]
    async fn stores_each_index_once_within_the_cap() {
        let sink = CoverSink::new(3, 4);
        let dest = sink.destination();
        let put = dest
            .put("0", body(b"abcd"), WriteOptions::default())
            .await
            .expect("fits");
        assert_eq!(put.bytes_written, 4);
        let again = dest
            .put("0", body(b"x"), WriteOptions::default())
            .await
            .expect_err("second write");
        assert!(again.message.contains("already written"));
        for key in ["3", "01", "-1", "+1", " 1", "a", ""] {
            dest.put(key, body(b"x"), WriteOptions::default())
                .await
                .expect_err("not a slot");
        }
        let big = dest
            .put("1", body(b"abcde"), WriteOptions::default())
            .await
            .expect_err("one byte over");
        assert_eq!(
            big.code,
            bookclerk_plugin_sdk::PluginErrorCode::PayloadTooLarge
        );
        let declared = dest
            .put(
                "1",
                body(b"a"),
                WriteOptions {
                    content_length: Some(5),
                    ..WriteOptions::default()
                },
            )
            .await
            .expect_err("declared length over the cap");
        assert_eq!(
            declared.code,
            bookclerk_plugin_sdk::PluginErrorCode::PayloadTooLarge
        );
        let staged = dest
            .put(
                "1",
                body(b"a"),
                WriteOptions {
                    stage_only: true,
                    ..WriteOptions::default()
                },
            )
            .await
            .expect_err("staging");
        assert_eq!(
            staged.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unsupported
        );
        dest.put("2", body(b""), WriteOptions::default())
            .await
            .expect("empty body fits");

        let fetched = sink.take_fetched(vec![
            image("https://a", true, None),
            image("https://b", false, Some("HTTP 404")),
            image("https://c", true, None),
        ]);
        assert_eq!(fetched[0].body.as_deref(), Some(&b"abcd"[..]));
        assert_eq!(fetched[0].content_type.as_deref(), Some("image/png"));
        assert_eq!(fetched[1].body, None);
        assert_eq!(fetched[1].error.as_deref(), Some("HTTP 404"));
        assert_eq!(fetched[2].body.as_deref(), Some(&b""[..]));
    }

    #[tokio::test]
    async fn answers_without_bodies_become_errors() {
        let sink = CoverSink::new(2, 8);
        sink.destination()
            .put("1", body(b"late"), WriteOptions::default())
            .await
            .expect("stored");
        let fetched = sink.take_fetched(vec![
            image("https://a", true, None),
            image("https://b", false, None),
        ]);
        assert_eq!(fetched[0].body, None);
        assert!(fetched[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("never sent")));
        // A body the guest did not claim is dropped.
        assert_eq!(fetched[1].body, None);
        assert_eq!(fetched[1].error.as_deref(), Some("not fetched"));
    }

    #[tokio::test]
    async fn other_methods_are_unsupported() {
        let dest = CoverSink::new(1, 8).destination();
        assert!(dest.head("0").await.is_err());
        assert!(dest.list(ListOptions::default()).await.is_err());
        assert!(dest.get("0", None).await.is_err());
        assert!(dest.copy("0", "1").await.is_err());
        assert!(dest.delete("0").await.is_err());
    }
}
