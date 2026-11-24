//! Where the server's images come from: the image store (feature
//! `store`), and images streamed on demand from an API client
//! ([`crate::stream::Streams`]).

use crate::file::Header;
use crate::server::{Ident, Millis};

/// An image the server can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// ID: a store image (< 0x1_0000) or a stream session (≥ 0x1_0000).
    pub id: u32,
    /// The OTA header.
    pub header: Header,
    /// File size.
    pub size: u32,
    /// Offered only to devices running at least this version.
    pub min_file_version: Option<u32>,
    /// Offered only to devices running at most this version.
    pub max_file_version: Option<u32>,
    /// File name.
    pub name: String,
    /// Streamed from a client.
    pub streamed: bool,
}

impl Offer {
    /// `(manufacturer, image type, file version)`.
    pub fn ident(&self) -> Ident {
        (
            self.header.manufacturer,
            self.header.image_type,
            self.header.file_version,
        )
    }
}

/// The result of reading image bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// The buffer is filled.
    Ready,
    /// Not here yet (asked for from the client): WAIT_FOR_DATA.
    Wait,
    /// The image is gone: ABORT.
    Gone,
}

/// Images and their bytes.
pub trait Images {
    /// Every image that can be offered now.
    fn offers(&self) -> Vec<Offer>;
    /// Reads `buf.len()` bytes at `offset` of image `id`.
    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8], now: Millis) -> Fetch;

    /// The offer with this identity (store images first).
    fn find(&self, ident: Ident) -> Option<Offer> {
        self.offers().into_iter().find(|o| o.ident() == ident)
    }

    /// The offer with this ID.
    fn get(&self, id: u32) -> Option<Offer> {
        self.offers().into_iter().find(|o| o.id == id)
    }
}

/// No images.
pub struct NoImages;

impl Images for NoImages {
    fn offers(&self) -> Vec<Offer> {
        Vec::new()
    }
    fn read(&mut self, _: u32, _: u32, _: &mut [u8], _: Millis) -> Fetch {
        Fetch::Gone
    }
}

/// The store (if any) and the streams together.
pub struct Sources<'a> {
    /// Stored images.
    #[cfg(feature = "store")]
    pub store: Option<&'a mut crate::store::ImageStore>,
    /// Streamed images.
    pub streams: &'a mut crate::stream::Streams,
}

impl<'a> Sources<'a> {
    /// The streams (and no store).
    pub fn new(streams: &'a mut crate::stream::Streams) -> Self {
        Self {
            #[cfg(feature = "store")]
            store: None,
            streams,
        }
    }

    /// With the store.
    #[cfg(feature = "store")]
    pub fn with_store(mut self, store: Option<&'a mut crate::store::ImageStore>) -> Self {
        self.store = store;
        self
    }
}

/// [`Sources`] for reading the offers only.
pub struct View<'a> {
    #[cfg(feature = "store")]
    store: Option<&'a crate::store::ImageStore>,
    streams: &'a crate::stream::Streams,
}

impl<'a> View<'a> {
    /// The streams (and no store).
    pub fn new(streams: &'a crate::stream::Streams) -> Self {
        Self {
            #[cfg(feature = "store")]
            store: None,
            streams,
        }
    }

    /// With the store.
    #[cfg(feature = "store")]
    pub fn with_store(mut self, store: Option<&'a crate::store::ImageStore>) -> Self {
        self.store = store;
        self
    }
}

impl Images for View<'_> {
    // Per block request: no list of every offer.
    fn find(&self, ident: Ident) -> Option<Offer> {
        #[cfg(feature = "store")]
        if let Some(s) = self.store
            && let Some(o) = s.offers().into_iter().find(|o| o.ident() == ident)
        {
            return Some(o);
        }
        self.streams.find(ident)
    }
    fn offers(&self) -> Vec<Offer> {
        #[allow(unused_mut)]
        let mut v = Vec::new();
        #[cfg(feature = "store")]
        if let Some(s) = self.store {
            v.extend(s.offers());
        }
        v.extend(self.streams.offers());
        v
    }
    fn read(&mut self, _: u32, _: u32, _: &mut [u8], _: Millis) -> Fetch {
        Fetch::Gone
    }
}

impl Images for Sources<'_> {
    // Per block request: no list of every offer.
    fn find(&self, ident: Ident) -> Option<Offer> {
        #[cfg(feature = "store")]
        if let Some(s) = self.store.as_deref()
            && let Some(o) = s.offers().into_iter().find(|o| o.ident() == ident)
        {
            return Some(o);
        }
        self.streams.find(ident)
    }
    fn offers(&self) -> Vec<Offer> {
        #[allow(unused_mut)]
        let mut v = Vec::new();
        #[cfg(feature = "store")]
        if let Some(s) = self.store.as_deref() {
            v.extend(s.offers());
        }
        v.extend(self.streams.offers());
        v
    }

    fn read(&mut self, id: u32, offset: u32, buf: &mut [u8], now: Millis) -> Fetch {
        if id >= crate::stream::FIRST_ID {
            return self.streams.read(id, offset, buf, now);
        }
        #[cfg(feature = "store")]
        if let Some(s) = self.store.as_deref_mut() {
            return s.read_offer(id, offset, buf);
        }
        Fetch::Gone
    }
}
