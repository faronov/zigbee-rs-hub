//! The fingerprint: canonical, hashable record of an interview (spec 3.2)
//! or of a GP commissioning (13.2).

use sha2::{Digest, Sha256};

use crate::gp::types::GpdId;
use crate::model::PowerKind;
use crate::zdo::LogicalType;

/// One endpoint of the fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EndpointFp {
    /// Endpoint.
    pub ep: u8,
    /// Profile.
    pub profile: u16,
    /// Device ID.
    pub device_id: u16,
    /// Server clusters.
    pub in_clusters: Vec<u16>,
    /// Client clusters.
    pub out_clusters: Vec<u16>,
}

/// Green Power part of the fingerprint (13.2).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GpFingerprint {
    /// GPD ID.
    pub gpd_id: GpdId,
    /// DeviceID.
    pub device_id: u8,
    /// Manufacturer ID (application information).
    pub manufacturer_id: Option<u16>,
    /// Model ID.
    pub model_id: Option<u16>,
    /// GPD command list.
    pub commands: Vec<u8>,
    /// Server clusters.
    pub server_clusters: Vec<u16>,
    /// Client clusters.
    pub client_clusters: Vec<u16>,
    /// Switch information: contacts, switch type.
    pub switch_info: Option<(u8, u8)>,
    /// Security level the sink commissioned it with.
    pub security_level: u8,
    /// RxOnCapability (bidirectional).
    pub rx_on_capability: bool,
}

/// The fingerprint (3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    /// Basic 0x0004.
    pub manufacturer_name: Option<String>,
    /// Basic 0x0005.
    pub model_id: Option<String>,
    /// Node descriptor manufacturer code.
    pub mfr_code: Option<u16>,
    /// Node descriptor logical type.
    pub logical_type: Option<LogicalType>,
    /// Basic 0x0007.
    pub power_source: Option<u8>,
    /// Node descriptor RxOnWhenIdle.
    pub rx_on_when_idle: Option<bool>,
    /// Node descriptor MAC "mains powered" bit.
    pub mac_mains: Option<bool>,
    /// Basic 0x0001.
    pub app_version: Option<u32>,
    /// Basic 0x0002.
    pub stack_version: Option<u32>,
    /// Basic 0x0003.
    pub hw_version: Option<u32>,
    /// Basic 0x0000.
    pub zcl_version: Option<u32>,
    /// Basic 0x0006.
    pub date_code: Option<String>,
    /// Basic 0x4000.
    pub sw_build_id: Option<String>,
    /// Simple descriptors.
    pub endpoints: Vec<EndpointFp>,
    /// Self-description schema hash (8 bytes, hex).
    pub self_description: Option<String>,
    /// Green Power identity.
    pub gp: Option<GpFingerprint>,
}

impl Fingerprint {
    /// Endpoint by number.
    pub fn endpoint(&self, ep: u8) -> Option<&EndpointFp> {
        self.endpoints.iter().find(|e| e.ep == ep)
    }

    /// Power source: Basic PowerSource, else the Node descriptor.
    pub fn power(&self) -> PowerKind {
        if self.gp.is_some() {
            return PowerKind::Battery;
        }
        match self.power_source.map(|p| p & 0x7F) {
            Some(0x01 | 0x02 | 0x04 | 0x05 | 0x06) => PowerKind::Mains,
            Some(0x03) => PowerKind::Battery,
            _ => match (self.mac_mains, self.rx_on_when_idle) {
                (Some(true), _) | (None, Some(true)) => PowerKind::Mains,
                (Some(false), _) | (None, Some(false)) => PowerKind::Battery,
                (None, None) => PowerKind::Mains,
            },
        }
    }

    /// Sleepy end device (RxOnWhenIdle = 0).
    pub fn sleepy(&self) -> bool {
        self.gp.is_none() && self.rx_on_when_idle == Some(false)
    }

    /// Canonical CBOR (deterministic: integer keys in order, definite
    /// lengths, shortest integers).
    pub fn canonical_cbor(&self) -> Vec<u8> {
        let mut c = Cbor::default();
        let mut fields: Vec<(u64, Box<dyn Fn(&mut Cbor) + '_>)> = Vec::new();
        let opt_s = |v: &Option<String>| v.clone();
        if let Some(s) = opt_s(&self.manufacturer_name) {
            fields.push((0, Box::new(move |c| c.text(&s))));
        }
        if let Some(s) = opt_s(&self.model_id) {
            fields.push((1, Box::new(move |c| c.text(&s))));
        }
        if let Some(v) = self.mfr_code {
            fields.push((2, Box::new(move |c| c.uint(u64::from(v)))));
        }
        if let Some(v) = self.logical_type {
            let n = match v {
                LogicalType::Coordinator => 0,
                LogicalType::Router => 1,
                LogicalType::EndDevice => 2,
                LogicalType::Unknown => 7,
            };
            fields.push((3, Box::new(move |c| c.uint(n))));
        }
        if let Some(v) = self.power_source {
            fields.push((4, Box::new(move |c| c.uint(u64::from(v)))));
        }
        if let Some(v) = self.rx_on_when_idle {
            fields.push((5, Box::new(move |c| c.bool(v))));
        }
        for (k, v) in [
            (6, self.app_version),
            (7, self.stack_version),
            (8, self.hw_version),
            (9, self.zcl_version),
        ] {
            if let Some(v) = v {
                fields.push((k, Box::new(move |c| c.uint(u64::from(v)))));
            }
        }
        if let Some(s) = opt_s(&self.date_code) {
            fields.push((10, Box::new(move |c| c.text(&s))));
        }
        if let Some(s) = opt_s(&self.sw_build_id) {
            fields.push((11, Box::new(move |c| c.text(&s))));
        }
        let mut eps = self.endpoints.clone();
        eps.sort_by_key(|e| e.ep);
        fields.push((
            12,
            Box::new(move |c| {
                c.array(eps.len());
                for e in &eps {
                    c.array(5);
                    c.uint(u64::from(e.ep));
                    c.uint(u64::from(e.profile));
                    c.uint(u64::from(e.device_id));
                    c.array(e.in_clusters.len());
                    e.in_clusters.iter().for_each(|x| c.uint(u64::from(*x)));
                    c.array(e.out_clusters.len());
                    e.out_clusters.iter().for_each(|x| c.uint(u64::from(*x)));
                }
            }),
        ));
        if let Some(s) = opt_s(&self.self_description) {
            fields.push((13, Box::new(move |c| c.text(&s))));
        }
        if let Some(gp) = &self.gp {
            fields.push((
                14,
                Box::new(move |c| {
                    c.array(8);
                    match gp.gpd_id {
                        GpdId::SrcId(s) => {
                            c.array(1);
                            c.uint(u64::from(s));
                        }
                        GpdId::Ieee { ieee, endpoint } => {
                            c.array(2);
                            c.uint(ieee);
                            c.uint(u64::from(endpoint));
                        }
                    }
                    c.uint(u64::from(gp.device_id));
                    c.opt_uint(gp.manufacturer_id.map(u64::from));
                    c.opt_uint(gp.model_id.map(u64::from));
                    c.bytes(&gp.commands);
                    c.array(gp.server_clusters.len());
                    gp.server_clusters.iter().for_each(|x| c.uint(u64::from(*x)));
                    c.array(gp.client_clusters.len());
                    gp.client_clusters.iter().for_each(|x| c.uint(u64::from(*x)));
                    c.opt_uint(gp.switch_info.map(|(n, t)| (u64::from(n) << 8) | u64::from(t)));
                }),
            ));
        }
        c.map(fields.len());
        for (k, f) in &fields {
            c.uint(*k);
            f(&mut c);
        }
        c.out
    }

    /// Fingerprint hash: SHA-256 over the canonical CBOR, first 16 bytes,
    /// hex.
    pub fn hash(&self) -> String {
        let d = Sha256::digest(self.canonical_cbor());
        crate::expr::hex(&d[..16])
    }
}

/// Minimal deterministic CBOR writer.
#[derive(Default)]
pub struct Cbor {
    /// Output.
    pub out: Vec<u8>,
}

#[allow(missing_docs)]
impl Cbor {
    fn head(&mut self, major: u8, v: u64) {
        let m = major << 5;
        match v {
            0..=23 => self.out.push(m | v as u8),
            24..=0xFF => self.out.extend_from_slice(&[m | 24, v as u8]),
            0x100..=0xFFFF => {
                self.out.push(m | 25);
                self.out.extend_from_slice(&(v as u16).to_be_bytes());
            }
            0x1_0000..=0xFFFF_FFFF => {
                self.out.push(m | 26);
                self.out.extend_from_slice(&(v as u32).to_be_bytes());
            }
            _ => {
                self.out.push(m | 27);
                self.out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
    pub fn uint(&mut self, v: u64) {
        self.head(0, v);
    }
    pub fn int(&mut self, v: i64) {
        if v >= 0 {
            self.head(0, v as u64);
        } else {
            self.head(1, (-1 - v) as u64);
        }
    }
    pub fn opt_uint(&mut self, v: Option<u64>) {
        match v {
            Some(v) => self.uint(v),
            None => self.out.push(0xF6),
        }
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.head(2, b.len() as u64);
        self.out.extend_from_slice(b);
    }
    pub fn text(&mut self, s: &str) {
        self.head(3, s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
    }
    pub fn array(&mut self, n: usize) {
        self.head(4, n as u64);
    }
    pub fn map(&mut self, n: usize) {
        self.head(5, n as u64);
    }
    pub fn bool(&mut self, b: bool) {
        self.out.push(if b { 0xF5 } else { 0xF4 });
    }
    pub fn null(&mut self) {
        self.out.push(0xF6);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_order_independent() {
        let mut a = Fingerprint {
            manufacturer_name: Some("LUMI".into()),
            model_id: Some("lumi.motion.ac02".into()),
            endpoints: vec![
                EndpointFp { ep: 2, profile: 0x0104, device_id: 0x0107, in_clusters: vec![0x0406], out_clusters: vec![] },
                EndpointFp { ep: 1, profile: 0x0104, device_id: 0x0107, in_clusters: vec![0x0000], out_clusters: vec![] },
            ],
            ..Default::default()
        };
        let h = a.hash();
        assert_eq!(h.len(), 32);
        a.endpoints.reverse();
        assert_eq!(a.hash(), h);
        a.sw_build_id = Some("1.2".into());
        assert_ne!(a.hash(), h);
        let mut c = Cbor::default();
        c.uint(500);
        c.int(-1);
        assert_eq!(c.out, vec![0x19, 0x01, 0xF4, 0x20]);
    }

    #[test]
    fn power_classification() {
        let mut f = Fingerprint { power_source: Some(0x03), ..Default::default() };
        assert_eq!(f.power(), PowerKind::Battery);
        f.power_source = Some(0x81);
        assert_eq!(f.power(), PowerKind::Mains);
        f.power_source = None;
        f.rx_on_when_idle = Some(false);
        assert_eq!(f.power(), PowerKind::Battery);
        assert!(f.sleepy());
    }
}
