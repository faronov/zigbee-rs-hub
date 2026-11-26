# catalog-2026.11.24 (excerpt)

Files of the published catalog release
[`catalog-2026.11.24`](https://github.com/faronov/zigbee-rs-definitions/releases/tag/catalog-2026.11.24)
of faronov/zigbee-rs-definitions, unchanged: the signed `manifest.cose`
(key ID 66f64ac1cbfc9e27, the firmware's slot 0) and the five shards a
Tuya TS011F plug with manufacturer `_TZ3000_gjnozsaz` asks for
(`catalog/status` `missing`: 0x08, 0x7d, 0xb0, 0xc6, 0xd9). Hub 0.3.0 kept
that plug generic and said "the catalog has no definition": the regression
tests `hub-devices/tests/catalog_lookup.rs` and `hub-app`
`catalog::tests_e2e` replay its Find definition against these bytes.

The definitions are generated from zigbee-herdsman-converters 26.130.0
(MIT, Copyright (c) 2018 Koen Kanters); zigbee-rs-definitions is MIT
licensed (its `LICENSE` and `NOTICE`).
