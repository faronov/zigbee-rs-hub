//! A simulated OTA Upgrade client (ZCL r8 11.13, client side), for host
//! tests and `hubctl --sim`: it answers the server's frames the way a
//! device does (the zigbee-rs `OtaManager` flow): Image Notify → Query
//! Next Image → Image Block (or Page) Requests → Upgrade End → "reboot"
//! into the new version and query again. It also answers Read Attributes
//! of CurrentFileVersion.

use crate::file;
use crate::server::{Millis, attr, cmd, status};

/// Client state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// Nothing running.
    Idle,
    /// Query sent.
    Querying,
    /// Downloading.
    Downloading,
    /// Told to wait until (ms).
    Waiting(Millis),
    /// Upgrade End sent.
    Ending,
}

/// The simulated client.
#[derive(Debug, Clone)]
pub struct SimClient {
    /// Manufacturer code.
    pub manufacturer: u16,
    /// Image type.
    pub image_type: u16,
    /// Running file version.
    pub version: u32,
    /// Hardware version sent in queries.
    pub hw: Option<u16>,
    /// MaxDataSize of block requests.
    pub block_size: u8,
    /// Use Image Page Requests of this size (0 = block requests).
    pub page_size: u16,
    /// State.
    pub state: ClientState,
    /// Bytes received of the current download.
    pub received: Vec<u8>,
    /// `(file version, size)` offered.
    pub offer: Option<(u32, u32)>,
    /// MinimumBlockPeriod told by the server.
    pub min_block_period: u16,
    /// Answer the completed download with INVALID_IMAGE.
    pub reject: bool,
    /// Upgrades completed.
    pub upgrades: u32,
    /// Largest block received.
    pub largest_block: usize,
    /// Block responses received.
    pub blocks: u32,
    /// WAIT_FOR_DATA received.
    pub waits: u32,
    /// The server answered ABORT.
    pub aborted: bool,
    tsn: u8,
}

/// A ZCL frame `(tsn, cmd, payload)` from the client.
pub type Frame = (u8, u8, Vec<u8>);

impl SimClient {
    /// A client running `version`.
    pub fn new(manufacturer: u16, image_type: u16, version: u32) -> Self {
        Self {
            manufacturer,
            image_type,
            version,
            hw: None,
            block_size: 48,
            page_size: 0,
            state: ClientState::Idle,
            received: Vec::new(),
            offer: None,
            min_block_period: 0,
            reject: false,
            upgrades: 0,
            largest_block: 0,
            blocks: 0,
            waits: 0,
            aborted: false,
            tsn: 0x80,
        }
    }

    fn next_tsn(&mut self) -> u8 {
        self.tsn = self.tsn.wrapping_add(1);
        self.tsn
    }

    /// Query Next Image Request (a device's own periodic query).
    pub fn query(&mut self) -> Frame {
        self.state = ClientState::Querying;
        let mut p = vec![u8::from(self.hw.is_some())];
        p.extend(self.manufacturer.to_le_bytes());
        p.extend(self.image_type.to_le_bytes());
        p.extend(self.version.to_le_bytes());
        if let Some(hw) = self.hw {
            p.extend(hw.to_le_bytes());
        }
        (self.next_tsn(), cmd::QUERY_NEXT_IMAGE_REQ, p)
    }

    fn request(&mut self) -> Frame {
        let (v, _) = self.offer.unwrap();
        let mut p = vec![0u8];
        p.extend(self.manufacturer.to_le_bytes());
        p.extend(self.image_type.to_le_bytes());
        p.extend(v.to_le_bytes());
        p.extend((self.received.len() as u32).to_le_bytes());
        p.push(self.block_size);
        let tsn = self.next_tsn();
        if self.page_size > 0 {
            p.extend(self.page_size.to_le_bytes());
            p.extend(0u16.to_le_bytes());
            (tsn, cmd::IMAGE_PAGE_REQ, p)
        } else {
            if self.min_block_period > 0 {
                p[0] |= 2;
                p.extend(self.min_block_period.to_le_bytes());
            }
            (tsn, cmd::IMAGE_BLOCK_REQ, p)
        }
    }

    fn end(&mut self, st: u8) -> Frame {
        let (v, _) = self.offer.unwrap_or((0, 0));
        let mut p = vec![st];
        p.extend(self.manufacturer.to_le_bytes());
        p.extend(self.image_type.to_le_bytes());
        p.extend(v.to_le_bytes());
        self.state = if st == status::SUCCESS {
            ClientState::Ending
        } else {
            ClientState::Idle
        };
        (self.next_tsn(), cmd::UPGRADE_END_REQ, p)
    }

    /// A cluster command from the server; returns the client's answers.
    pub fn on_command(&mut self, command: u8, p: &[u8], now: Millis) -> Vec<Frame> {
        let u16_ = |i: usize| p.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let u32_ = |i: usize| {
            p.get(i..i + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        match command {
            cmd::IMAGE_NOTIFY => {
                if !matches!(self.state, ClientState::Idle) {
                    return Vec::new();
                }
                let ty = p.first().copied().unwrap_or(0);
                if ty >= 1 && u16_(2).is_some_and(|m| m != 0xFFFF && m != self.manufacturer) {
                    return Vec::new();
                }
                if ty >= 2 && u16_(4).is_some_and(|t| t != 0xFFFF && t != self.image_type) {
                    return Vec::new();
                }
                if ty >= 3 && u32_(6) == Some(self.version) {
                    return Vec::new();
                }
                vec![self.query()]
            }
            cmd::QUERY_NEXT_IMAGE_RSP => {
                if self.state != ClientState::Querying {
                    return Vec::new();
                }
                if p.first() != Some(&status::SUCCESS) {
                    self.state = ClientState::Idle;
                    return Vec::new();
                }
                let (Some(m), Some(t), Some(v), Some(size)) = (u16_(1), u16_(3), u32_(5), u32_(9))
                else {
                    self.state = ClientState::Idle;
                    return Vec::new();
                };
                if (m, t) != (self.manufacturer, self.image_type) {
                    self.state = ClientState::Idle;
                    return Vec::new();
                }
                if self.offer != Some((v, size)) {
                    self.received.clear();
                }
                self.offer = Some((v, size));
                self.state = ClientState::Downloading;
                vec![self.request()]
            }
            cmd::IMAGE_BLOCK_RSP => {
                if !matches!(
                    self.state,
                    ClientState::Downloading | ClientState::Waiting(_)
                ) {
                    return Vec::new();
                }
                match p.first().copied() {
                    Some(status::SUCCESS) => {
                        let (Some(off), Some(&n)) = (u32_(9), p.get(13)) else {
                            return Vec::new();
                        };
                        let data = &p[14..];
                        if data.len() != usize::from(n) || off as usize != self.received.len() {
                            // A stale or out-of-order block (a page burst
                            // after a retry): ignore.
                            return Vec::new();
                        }
                        assert!(
                            n <= self.block_size,
                            "block {n} > MaxDataSize {}",
                            self.block_size
                        );
                        self.blocks += 1;
                        self.largest_block = self.largest_block.max(data.len());
                        self.received.extend_from_slice(data);
                        let size = self.offer.unwrap().1 as usize;
                        if self.received.len() >= size {
                            let ok = !self.reject
                                && file::check(&self.received).is_ok_and(|c| {
                                    (
                                        c.header.manufacturer,
                                        c.header.image_type,
                                        c.header.file_version,
                                    ) == (self.manufacturer, self.image_type, self.offer.unwrap().0)
                                });
                            return vec![self.end(if ok {
                                status::SUCCESS
                            } else {
                                status::INVALID_IMAGE
                            })];
                        }
                        // Pages: the next request once the page is complete.
                        if self.page_size > 0
                            && !self
                                .received
                                .len()
                                .is_multiple_of(usize::from(self.page_size))
                        {
                            return Vec::new();
                        }
                        self.state = ClientState::Downloading;
                        vec![self.request()]
                    }
                    Some(status::WAIT_FOR_DATA) => {
                        self.waits += 1;
                        let (Some(cur), Some(at), Some(period)) = (u32_(1), u32_(5), u16_(9))
                        else {
                            return Vec::new();
                        };
                        self.min_block_period = period;
                        let delay = u64::from(at.saturating_sub(cur)) * 1000;
                        if delay == 0 {
                            self.state = ClientState::Downloading;
                            vec![self.request()]
                        } else {
                            self.state = ClientState::Waiting(now + delay);
                            Vec::new()
                        }
                    }
                    _ => {
                        self.aborted = true;
                        self.state = ClientState::Idle;
                        Vec::new()
                    }
                }
            }
            cmd::UPGRADE_END_RSP => {
                if self.state != ClientState::Ending {
                    return Vec::new();
                }
                // "Reboot" into the new image, then query as devices do.
                self.version = self.offer.take().map_or(self.version, |o| o.0);
                self.received.clear();
                self.upgrades += 1;
                self.state = ClientState::Idle;
                vec![self.query()]
            }
            _ => Vec::new(),
        }
    }

    /// Time passes: a WAIT_FOR_DATA delay may end.
    pub fn tick(&mut self, now: Millis) -> Vec<Frame> {
        match self.state {
            ClientState::Waiting(t) if now >= t => {
                self.state = ClientState::Downloading;
                vec![self.request()]
            }
            _ => Vec::new(),
        }
    }

    /// A whole ZCL frame from the server; returns whole ZCL frames back
    /// (cluster commands, and Read Attributes of CurrentFileVersion).
    pub fn on_zcl(&mut self, asdu: &[u8], now: Millis) -> Vec<Vec<u8>> {
        let Some(&fc) = asdu.first() else {
            return Vec::new();
        };
        let hdr = if fc & 0x04 != 0 { 5 } else { 3 };
        if asdu.len() < hdr {
            return Vec::new();
        }
        let (tsn, command, payload) = (asdu[hdr - 2], asdu[hdr - 1], &asdu[hdr..]);
        if fc & 0x03 == 0x00 {
            // Read Attributes of the client cluster.
            if command != 0x00 {
                return Vec::new();
            }
            let mut rsp = vec![
                0x18, /* global, client → server, no default rsp */
                tsn, 0x01,
            ];
            for a in payload
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
            {
                rsp.extend(a.to_le_bytes());
                if a == attr::CURRENT_FILE_VERSION {
                    rsp.push(0x00);
                    rsp.push(0x23);
                    rsp.extend(self.version.to_le_bytes());
                } else {
                    rsp.push(0x86);
                }
            }
            return vec![rsp];
        }
        self.on_command(command, payload, now)
            .into_iter()
            .map(frame)
            .collect()
    }

    /// [`tick`](Self::tick) as whole ZCL frames.
    pub fn tick_zcl(&mut self, now: Millis) -> Vec<Vec<u8>> {
        self.tick(now).into_iter().map(frame).collect()
    }
}

/// A client → server cluster command frame.
pub fn frame((tsn, command, payload): Frame) -> Vec<u8> {
    let mut f = vec![0x01, tsn, command];
    f.extend(payload);
    f
}
