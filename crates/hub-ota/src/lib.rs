//! Zigbee OTA Upgrade server of zigbee-rs-hub (cluster 0x0019, ZCL r8
//! chapter 11) for the devices of the network.
//!
//! | Module | Contents |
//! |---|---|
//! | [`file`] | the OTA upgrade file: header, sub-elements, streaming check |
//! | [`json`] | JSON text writer for the API records |
//! | [`images`] | the image sources of the server (store, streams) |
//! | [`stream`] | images streamed on demand from an API client (no storage on the hub) |
//! | [`store`] (feature `store`) | images on a raw flash region (the inactive app slot of the 8 MB layout): chunked, resumable uploads, quota |
//! | [`server`] | the server state machine: queries, block and page requests, upgrade end, Image Notify, policy and schedule window, pacing, jobs |
//! | [`sim`] | a simulated OTA client for host tests and `hubctl --sim` |
//!
//! The hub never downloads images itself (no TLS on the hub): the web UI,
//! the Home Assistant integration or `hubctl` upload them into the store,
//! or serve them block by block while a device downloads (streaming).

pub mod file;
pub mod images;
pub mod json;
pub mod server;
pub mod sim;
#[cfg(feature = "store")]
pub mod store;
pub mod stream;

pub use images::{Fetch, Images, Offer, Sources, View};
pub use server::{Action, Device, Policy, Server, Settings};
#[cfg(feature = "store")]
pub use store::{ImageStore, StoreError, UploadMeta};
pub use stream::Streams;

#[cfg(all(test, feature = "store"))]
mod tests;
#[cfg(test)]
mod tests_stream;
