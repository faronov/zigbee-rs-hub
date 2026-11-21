//! Web tools in `hubdata` (device-support 14.4, D15): a `.zwb` bundle
//! (`zdef::webtools`) with the definition compiler for the web UI (zdefc
//! WebAssembly and its JavaScript glue), served gzip-compressed straight
//! from the mapped area at `/zdefc/<file>`.
//!
//! Installed from the catalog release (manifest key 64, `zdefc-web.zwb`,
//! SHA-256 from the signed manifest) or uploaded (`catalog/upload_chunk`
//! with `target: "web"`). Everything after the 40-byte header is written
//! first and the header last; serving stops (write lock) while the area is
//! rewritten.

use std::io;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use zdef::webtools::{Bundle, HEADER_LEN};

use super::data::{AreaWriter, DataFlash, SECTOR};

/// Name of the bundle asset in a catalog release.
pub const ASSET: &str = "zdefc-web.zwb";

/// The installed bundle.
#[derive(Clone)]
pub struct Installed {
    bytes: &'static [u8],
    /// SHA-256 of the whole file (compared with the manifest).
    pub sha256: [u8; 32],
    /// Strong ETag (quoted).
    pub etag: String,
}

impl Installed {
    /// File size.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// File names.
    pub fn files(&self) -> Vec<String> {
        Bundle::parse(self.bytes).map(|b| b.iter().flatten().map(|e| e.name.to_string()).collect()).unwrap_or_default()
    }
}

/// One file to serve.
pub struct WebFile<'a> {
    /// Content type.
    pub content_type: &'a str,
    /// Gzip bytes.
    pub data: &'a [u8],
    /// Strong ETag of the bundle (quoted).
    pub etag: &'a str,
}

/// The web tools, shared with the HTTP servers.
#[derive(Default)]
pub struct WebTools {
    inner: RwLock<Option<Installed>>,
}

/// Shared handle.
pub type SharedWeb = Arc<WebTools>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl WebTools {
    /// Serves `name` while the area cannot change: `f` gets the file, or
    /// `None` when nothing (or no such file) is installed.
    #[inline(never)]
    pub fn serve<R>(&self, name: &str, f: impl FnOnce(Option<WebFile<'_>>) -> R) -> R {
        let g = self.inner.read().unwrap();
        let file = g.as_ref().and_then(|i| {
            let e = Bundle::parse(i.bytes).ok()?.find(name)?;
            Some(WebFile { content_type: e.content_type, data: e.data, etag: &i.etag })
        });
        f(file)
    }

    /// The installed bundle.
    pub fn installed(&self) -> Option<Installed> {
        self.inner.read().unwrap().clone()
    }

    fn set(&self, i: Option<Installed>) {
        *self.inner.write().unwrap() = i;
    }

    /// Checks the bundle in the area (`offset`, mapped) and serves it.
    #[inline(never)]
    pub fn load(&self, f: &dyn DataFlash, offset: u32, size: u32) -> Result<(), String> {
        let area = f.map(offset, size);
        let b = Bundle::parse(area).map_err(|e| format!("web tools: {e:?}"))?;
        let bytes = &area[..b.len()];
        if <[u8; 32]>::from(Sha256::digest(&bytes[HEADER_LEN..])) != b.digest() {
            return Err("web tools: SHA-256 of the bundle differs".into());
        }
        let sha256: [u8; 32] = Sha256::digest(bytes).into();
        let etag = format!("\"{}\"", &hex(&sha256)[..16]);
        self.set(Some(Installed { bytes, sha256, etag }));
        Ok(())
    }
}

/// A bundle being written into the web area.
pub struct WebInstall {
    offset: u32,
    size: u32,
    head: Vec<u8>,
    writer: AreaWriter,
    hash: Sha256,
    written: u32,
    /// Expected file size, if known.
    pub expect_size: Option<u32>,
    /// Expected SHA-256 of the file, if known (manifest).
    pub expect_sha: Option<[u8; 32]>,
}

impl WebInstall {
    /// Starts an install: stops serving and erases the header sector.
    #[inline(never)]
    pub fn begin(
        web: &WebTools,
        f: &mut dyn DataFlash,
        offset: u32,
        size: u32,
        expect_size: Option<u32>,
        expect_sha: Option<[u8; 32]>,
    ) -> io::Result<WebInstall> {
        if expect_size.is_some_and(|n| n > size) {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "web tools larger than their area"));
        }
        web.set(None);
        f.erase(offset, SECTOR)?;
        // The writer covers the area from the first sector on; the header
        // bytes stay erased until `finish`.
        let mut writer = AreaWriter::new(offset, size);
        writer.write(f, &[0xFF; HEADER_LEN])?;
        Ok(WebInstall {
            offset,
            size,
            head: Vec::with_capacity(HEADER_LEN),
            writer,
            hash: Sha256::new(),
            written: 0,
            expect_size,
            expect_sha,
        })
    }

    /// Bytes received.
    pub fn written(&self) -> u32 {
        self.written
    }

    /// Appends a chunk.
    #[inline(never)]
    pub fn write(&mut self, f: &mut dyn DataFlash, mut data: &[u8]) -> io::Result<()> {
        if self.expect_size.is_some_and(|n| self.written as u64 + data.len() as u64 > n as u64)
            || self.written as u64 + data.len() as u64 > self.size as u64
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "more bytes than expected"));
        }
        self.hash.update(data);
        self.written += data.len() as u32;
        let take = (HEADER_LEN - self.head.len()).min(data.len());
        self.head.extend_from_slice(&data[..take]);
        data = &data[take..];
        if !data.is_empty() {
            self.writer.write(f, data)?;
        }
        Ok(())
    }

    /// Writes the header, checks the bundle and serves it.
    #[inline(never)]
    pub fn finish(self, web: &WebTools, f: &mut dyn DataFlash) -> Result<(), String> {
        let sha: [u8; 32] = self.hash.finalize().into();
        if self.expect_size.is_some_and(|n| n != self.written) {
            return Err(format!("{} bytes, expected {}", self.written, self.expect_size.unwrap()));
        }
        if self.expect_sha.is_some_and(|h| h != sha) {
            return Err("SHA-256 differs from the manifest".into());
        }
        if self.head.len() < HEADER_LEN || &self.head[..4] != zdef::webtools::MAGIC {
            return Err("not a web tools bundle".into());
        }
        f.write(self.offset, &self.head).map_err(|e| e.to_string())?;
        if let Err(e) = web.load(f, self.offset, self.size) {
            let _ = f.erase(self.offset, SECTOR);
            return Err(e);
        }
        Ok(())
    }
}

/// Installs a whole bundle (hubctl `--web-tools`, tests).
pub fn install_bytes(
    web: &WebTools,
    f: &mut dyn DataFlash,
    offset: u32,
    size: u32,
    bytes: &[u8],
) -> Result<(), String> {
    let mut i = WebInstall::begin(web, f, offset, size, Some(bytes.len() as u32), None).map_err(|e| e.to_string())?;
    for c in bytes.chunks(4096) {
        i.write(f, c).map_err(|e| e.to_string())?;
    }
    i.finish(web, f)
}

#[cfg(test)]
mod tests {
    use super::super::data::MemData;
    use super::*;

    fn bundle() -> Vec<u8> {
        let files = vec![
            ("zdefc_wasm.js".to_string(), "text/javascript".to_string(), vec![0x1f, 0x8b, 1, 2, 3]),
            ("zdefc_wasm_bg.wasm".to_string(), "application/wasm".to_string(), [0x1f, 0x8b].repeat(2500)),
        ];
        zdef::webtools::build(&files).unwrap()
    }

    #[test]
    fn install_serve_reinstall_and_torn_writes() {
        let (off, size) = (8192, 64 * 1024);
        let mut f = MemData::new(off + size);
        let web = WebTools::default();
        assert!(web.load(&f, off, size).is_err(), "erased area");
        let b = bundle();
        install_bytes(&web, &mut f, off, size, &b).unwrap();
        assert!(web.serve("zdefc_wasm.js", |x| x.is_some_and(|x| x.data == [0x1f, 0x8b, 1, 2, 3])));
        assert!(web.serve("nope", |x| x.is_none()));
        assert_eq!(web.installed().unwrap().size(), b.len());
        // A restart finds it again.
        let again = WebTools::default();
        again.load(&f, off, size).unwrap();
        assert_eq!(again.installed().unwrap().sha256, web.installed().unwrap().sha256);

        // Power lost in the middle of a new install: the header is erased,
        // nothing is served after the restart.
        let mut i = WebInstall::begin(&web, &mut f, off, size, None, None).unwrap();
        assert!(web.serve("zdefc_wasm.js", |x| x.is_none()), "not served while written");
        i.write(&mut f, &b[..3000]).unwrap();
        drop(i);
        assert!(WebTools::default().load(&f, off, size).is_err());

        // A corrupted body is refused and the header erased again.
        let mut bad = b.clone();
        let n = bad.len();
        bad[n - 10] ^= 0xFF;
        assert!(install_bytes(&web, &mut f, off, size, &bad).is_err());
        assert!(WebTools::default().load(&f, off, size).is_err());
        // A wrong manifest hash is refused before the header is written.
        let mut i = WebInstall::begin(&web, &mut f, off, size, None, Some([0; 32])).unwrap();
        i.write(&mut f, &b).unwrap();
        assert!(i.finish(&web, &mut f).unwrap_err().contains("manifest"));
    }
}
