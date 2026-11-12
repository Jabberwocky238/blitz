//! Abstractions of networking so that custom networking implementations can be provided

pub use bytes::Bytes;
pub use http::{self, HeaderMap, Method};
use serde::{
    Serialize,
    ser::{SerializeSeq, SerializeTuple},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{ops::Deref, path::PathBuf};
pub use url::Url;

/// A type that fetches resources for a Document.
///
/// This may be over the network via http(s), via the filesystem, or some other method.
pub trait NetProvider: Send + Sync + 'static {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>);

    /// Whether this provider is a no-op (e.g. `DummyNetProvider`) that will never
    /// deliver resources. When true, callers must NOT register resources as
    /// "pending critical" — doing so blocks painting forever, since the
    /// completion callback never fires. Used by integrations that feed a
    /// pre-rendered DOM and perform no sub-fetches (e.g. aginxbrowser).
    fn is_noop(&self) -> bool {
        false
    }

    /// acrender: a resource was taken from the document's cache (or joined a request
    /// already in flight) instead of being fetched, so a provider can still report it
    fn reused(&self, _url: &Url, _destination: &'static str) {}
}

/// A type that parses raw bytes from a network request into a Data and then calls
/// the NetCallack with the result.
pub trait NetHandler: Send + Sync + 'static {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes);

    /// acrender: the request's destination as Fetch names it ("image", "style", "font",
    /// "iframe"; "" when unknown), so a provider can apply that destination's rules
    fn destination(&self) -> &'static str {
        ""
    }

    /// acrender: the CORS settings the element asks for (its `crossorigin` attribute):
    /// "anonymous" or "use-credentials"; "" for a request that is not a CORS one
    fn cors(&self) -> &'static str {
        ""
    }

    /// acrender: the response about to be given to `bytes` is opaque to the page: it came from
    /// another origin and no CORS check was made, so what is built from it may be used but not
    /// read (a stylesheet's rules)
    fn opaque(&mut self) {}

    /// acrender: the request has no usable response (not found, refused, a status or type its
    /// destination does not take). A handler for which no bytes and no response are the same
    /// thing leaves this as it is; a stylesheet overrides it, an empty sheet being a loaded one
    fn failed(self: Box<Self>, resolved_url: String) {
        self.bytes(resolved_url, Bytes::new())
    }
}

/// A callback which gets called every time a network request completes
// Q: Should we use std::task::Waker for this?
pub trait NetWaker: Send + Sync + 'static {
    fn wake(&self, client_id: usize);
}

impl<F: Fn(usize) + Send + Sync + 'static> NetWaker for F {
    fn wake(&self, doc_id: usize) {
        self(doc_id)
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
/// A request type loosely representing <https://fetch.spec.whatwg.org/#requests>
pub struct Request {
    pub url: Url,
    pub method: Method,
    pub content_type: Option<String>,
    pub headers: HeaderMap,
    pub body: Body,
    pub signal: Option<AbortSignal>,
}
impl Request {
    /// A get request to the specified Url and an empty body
    pub fn get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            content_type: None,
            headers: HeaderMap::new(),
            body: Body::Empty,
            signal: None,
        }
    }

    pub fn signal(mut self, signal: AbortSignal) -> Self {
        self.signal = Some(signal);
        self
    }
}

#[derive(Debug, Clone)]
pub enum Body {
    Bytes(Bytes),
    Form(FormData),
    Empty,
}

/// A list of form entries used for form submission
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormData(pub Vec<Entry>);
impl FormData {
    /// Creates a new empty FormData
    pub fn new() -> Self {
        FormData(Vec::new())
    }
}
impl Serialize for FormData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq_serializer = serializer.serialize_seq(Some(self.len()))?;
        for entry in &self.0 {
            seq_serializer.serialize_element(entry)?;
        }
        seq_serializer.end()
    }
}
impl Deref for FormData {
    type Target = Vec<Entry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A single form entry consisting of a name and value
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub value: EntryValue,
}
impl Serialize for Entry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut serializer = serializer.serialize_tuple(2)?;
        serializer.serialize_element(&self.name)?;
        match &self.value {
            EntryValue::String(s) => serializer.serialize_element(s)?,
            EntryValue::File(p) => serializer.serialize_element(p.to_str().unwrap_or_default())?,
            EntryValue::EmptyFile => serializer.serialize_element("")?,
        }
        serializer.end()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryValue {
    String(String),
    File(PathBuf),
    EmptyFile,
}
impl AsRef<str> for EntryValue {
    fn as_ref(&self) -> &str {
        match self {
            EntryValue::String(s) => s,
            EntryValue::File(p) => p.to_str().unwrap_or_default(),
            EntryValue::EmptyFile => "",
        }
    }
}

impl From<&str> for EntryValue {
    fn from(value: &str) -> Self {
        EntryValue::String(value.to_string())
    }
}
impl From<PathBuf> for EntryValue {
    fn from(value: PathBuf) -> Self {
        EntryValue::File(value)
    }
}

/// A default noop NetProvider
#[derive(Default)]
pub struct DummyNetProvider;
impl NetProvider for DummyNetProvider {
    fn fetch(&self, _doc_id: usize, _request: Request, _handler: Box<dyn NetHandler>) {}
    fn is_noop(&self) -> bool {
        true
    }
}

/// The AbortController interface represents a controller object that
/// allows you to abort one or more Web requests as and when desired.
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/AbortController>
#[derive(Debug, Default)]
pub struct AbortController {
    pub signal: AbortSignal,
}

impl AbortController {
    /// The abort() method of the AbortController interface aborts
    /// an asynchronous operation before it has completed.
    /// This is able to abort fetch requests.
    ///
    /// <https://developer.mozilla.org/en-US/docs/Web/API/AbortController/abort>
    pub fn abort(self) {
        self.signal.0.store(true, Ordering::SeqCst);
    }
}

/// The AbortSignal interface represents a signal object that allows you to
/// communicate with an asynchronous operation (such as a fetch request) and
/// abort it if required via an AbortController object.
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/AbortSignal>
#[derive(Debug, Default, Clone)]
pub struct AbortSignal(Arc<AtomicBool>);

impl AbortSignal {
    /// The aborted read-only property returns a value that indicates whether
    /// the asynchronous operations the signal is communicating with are
    /// aborted (true) or not (false).
    ///
    /// <https://developer.mozilla.org/en-US/docs/Web/API/AbortSignal/aborted>
    pub fn aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
