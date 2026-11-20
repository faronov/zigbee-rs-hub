# Catalog signing keys

`zdef-catalog-ed25519.pub.pem` is the public half of the Ed25519 key that signs
the device-definition catalog released by
[faronov/zigbee-rs-definitions](https://github.com/faronov/zigbee-rs-definitions)
(GitHub Actions secret `ZDEF_SIGNING_KEY`, created 2026-11-20). The hub firmware
embeds it and rejects catalogs, banks and shards whose manifest signature does
not verify (docs/device-support.md §9).

Raw 32-byte public key (hex):

```
ff931767df347a7bccd14b69602d4eb7841ef8503f771248b717f4554f63c6a2
```

The private key is kept offline by the maintainer and never committed. A
second key slot in the firmware is reserved for rotation.
