// Node smoke test of the wasm-bindgen build (CI): compile an example, compare
// with the CLI golden CBOR, decompile and recompile, check a broken input.
// usage: node smoke.js <wasm-bindgen --target nodejs out dir> <repo root>
const fs = require('fs');
const path = require('path');
const z = require(path.resolve(process.argv[2], 'zdefc_wasm.js'));
const root = process.argv[3];
const src = fs.readFileSync(path.join(root, 'definitions/examples/tuya-ts0601-trv-me167.yaml'), 'utf8');
const cbor = z.compile(src, 'yaml');
const golden = fs.readFileSync(path.join(root, 'crates/zdefc/tests/golden/tuya-ts0601-trv-me167.cbor'));
if (Buffer.compare(Buffer.from(cbor), golden) !== 0) throw new Error('wasm output differs from the golden CBOR');
const yaml = z.decompile(cbor, 'yaml');
if (Buffer.compare(Buffer.from(z.compile(yaml, 'yaml')), golden) !== 0) throw new Error('round trip differs');
const bad = JSON.parse(z.check('zdef: 1\nid: x\n', 'yaml'));
if (bad.ok || bad.errors.length === 0) throw new Error('check accepted a broken definition');
console.log('wasm ok:', cbor.length, 'B', z.content_hash(cbor).slice(0, 16), '|', bad.errors[0].message);
