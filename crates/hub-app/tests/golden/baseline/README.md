# Coordinator-boundary baselines

Golden files of `docs/coordinator-boundary.md` §4 ("Proof of no wire or
persistence change") and §7 PR A. They pin what the hub does today, so the
boundary refactoring can show that it changes none of it. Helpers:
`crates/hub-app/src/baseline.rs`; tests: `crates/hub-app/src/tests_baseline.rs`
and `catalog_areas_golden` in `crates/hub-app/src/catalog/tests_e2e.rs`.

```sh
cargo test -p hub-app --lib -- tests_baseline catalog_areas   # compare
UPDATE_GOLDENS=1 cargo test -p hub-app --lib -- tests_baseline catalog_areas   # rewrite
```

A difference fails with the changed lines of every golden at once. A
rewritten golden belongs in the same PR as the change that explains it.

| File | Pins |
|---|---|
| `wire_trace.txt` | Every request the hub sent to the simulated NCP (`SimState.trace`, off by default and turned on by the test: msg_id, payload hex) per scenario step: boot, app start (endpoint registration, NCP device import), form, NCP restart and resume, permit join, join and interview of a device, group add and delete, GP sink config and commissioning, GP remote pairing, app restart, network map, device removal, read-only API, backup, reset, restore. Compared exactly; `baseline::relaxed` is the comparison for an asynchronous transport (same multiset, same order per destination, same barriers with the same requests between them). |
| `store_writes.txt` | The write path of the scenario: every record the host store committed, per step (logged from the journal's record writes; sorted within a step, as threads interleave): typed records, `kv_put` with key, part/parts and length, `kv_del`. The app writes its registry and last values at the end of every step (a `hub/reboot` request, which the test platform ignores), so their timers do not decide which step a write lands in. Extra, duplicated or differently split writes change it. |
| `store_mid.*`, `store_end.*` | The content of the host store after the app stop in the middle and at the end of the scenario (normalized), written again through `Store` (`commit`, `kv_put`) into an empty 32 × 4 KiB journal: `.txt` lists the records that wrote, `.hex` is the image. They pin the record encodings and the key/value split of the stored values, not the order or number of the scenario's writes (that is `store_writes.txt`); the scenario's own journal is checked to reopen to the same state. |
| `journal_format.hex` | Raw journal bytes of a fixed sequence through `Store`: every record type (`commit`), unchanged and changed `commit_if_changed`, `kv_put` (an identical put, a value of three parts, an empty value), `kv_delete` (also of a missing key), three compactions, the wrap of the ring (6 × 4 KiB). The on-flash format and the store's skip and split rules, byte for byte. |
| `catalog_areas.hex` | The `hubdata` catalog areas (active-bank record, manifest, bank slots, local definitions, scratch, web tools) after installing the frozen release in `catalog/` and fetching one definition from its shards. `catalog/` is built from `definitions/examples` only when it is absent. |
| `restore_stream.hex` | The record stream the NCP received in `BKP_RESTORE_PUT`. |
| `api/*.json` | `network/info`, `hub/info`, `hub/system`, `network/diagnostics`, `network/topology`, `network/backup` (OCB), `network/reset` and `network/restore` results, keys sorted. |

Normalized (they follow the clock or thread timing, not the code):

- API JSON (`VOLATILE` in `tests_baseline.rs`): `time`, `time_utc`,
  `duration_ms`, `seq`, OCB `metadata.internal.creation_time`,
  `host_link` (link frame counters), `app.registry_writes`,
  `app.last_values[2]` (bytes written: the times vary in length),
  `store.journal_live_sectors`; `hub/info` `advanced` and `ota` are build
  features (checked, then replaced; `ota` is present only with feature
  `ota`, the 8 MB images);
- host store (`normalize_kv`): every number of at least 10^9 in `st/`
  values (Unix times in s or ms: `c`, `r`, `seen`, the `last_seen` entity),
  `creation_time` in `bkp/last`, and `last_seen` of the NCP device records.

Build-independent inputs: the hub endpoint is the one with OTA Upgrade
(0x0019) in every build (`hub_endpoint(true)`, the default with feature
`ota`), so `wire_trace.txt` (`NWK_ADD_ENDPOINT`), `store_writes.txt` and
`store_mid/end` hold the same endpoint record without `ota` too.

Deterministic inputs: the hub's RNG is the tests' counter, the simulated
devices (hub-devices' `SimDevice`) answer at once, the GP answers are
injected by the test, and every step waits for its effects and 300 ms
without a request, then has the app write its registry and last values,
before the next one starts.
