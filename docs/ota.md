# Zigbee OTA Upgrade on the hub

The hub is the **OTA Upgrade server** (cluster 0x0019, ZCL r8 chapter 11) of
its network: it serves Zigbee OTA files to the devices' OTA clients. Code:
[`crates/hub-ota`](../crates/hub-ota) (file format, image sources — store
and streams —, server state machine, simulated client; sans-IO, host
tested), [`crates/hub-app/src/ota.rs`](../crates/hub-app/src/ota.rs) and
`ota_store.rs` (the server on the app thread, the `ota/*` API), the web
UI's **Updates** tab, the Home Assistant `update` platform and `hubctl
ota-upload` / `ota-stream` / `ota-make` / `--sim-ota`. The API is in
[device-support 14.11](device-support.md#1411-ota-upgrade).

**Zigbee OTA is in the 8 MB images only** (decided 2026-11-24): the
server costs about 100 KB of app image, which would fill the 4 MB dev
slot to 95 %. Cargo features (hub-app, hub-web, tbr-hub): `ota` is the
server with streamed images, `ota-store` adds the image store and implies
`ota`. `tools/build-hub-firmware.sh` builds the 8 MB images with
`ota-store` and the 4 MB images without `ota`; `tools/release-images.py`
refuses a 4 MB image with Zigbee OTA and an 8 MB one without it. Without
`ota` the hub has no `ota` in `hub/info`, answers `ota/*` with
`unsupported`, the web page has no Updates tab and a device's OTA cluster
commands take the engine's generic path as before the server existed (a
Query Next Image gets the Default Response it asks for, SUCCESS, and no
Query Next Image Response, unchanged for devices that found the server
before). The hub endpoint lists OTA Upgrade (0x0019) as a server cluster
only with `ota` (decided 2026-11-24): the endpoint is stored in the host
store and replayed on every NCP resume, so at start the app compares the
stored record with this image's and, when they differ (a 4 MB hub updated
from an image with OTA, or the other way), sends `NWK_ADD_ENDPOINT` again
and stores the new record; the zigbee-rs NCP replaces an endpoint with the
same number (`upsert_endpoint`, since ncp-v0.1.0), no removal and no
factory reset needed.

**The hub downloads nothing.** There is no TLS on the hub. Images reach
the devices in one of two ways:

| | 8 MB images (`ota-store`) | 4 MB images | build with `ota` only |
|---|---|---|---|
| **Stream** (a connected client serves the image on demand) | yes | no Zigbee OTA | yes (the only way) |
| **Store** (uploaded, kept on the hub) | yes: the inactive app slot (3 MiB) | no Zigbee OTA | no |

A build with `ota` but without `ota-store` (e.g. a 4 MB lab image,
`-- --features ota`, see the hardware test) is in no release.

## Streaming

A client that holds the file — the Home Assistant integration (always
online, the preferred client), a browser tab with the web UI, `hubctl
ota-stream` — offers it with its first bytes (`ota/stream/offer`: size,
the header in base64). The hub checks the header (identifier, versions,
header length, total size = offered size; the client checks the
sub-elements and the provider's SHA) and offers the image to devices like
a stored one; `ota/start {device, image: session}` notifies the device.
When the device asks for bytes the hub does not have, the hub answers
WAIT_FOR_DATA (1 s) and sends the client
`{"type": "ota/block_needed", "session", "offset", "len"}` (1 KiB chunks);
the client answers `ota/block_data {session, offset, data}`. The hub checks
chunk 0 against the offered header. At most 4 streams.

**Memory per stream is fixed** (the first hardware run, an S3 without
PSRAM, ran out of heap after 2–3 chunks of 4 KiB: the request's `Value`
copy, the decoded bytes and the chunk copy per chunk, with fragmentation):

- a ring of **4 × 1 KiB**, allocated when the first chunk arrives;
- `ota/block_data` is taken from the request text: the base64 stays in the
  WebSocket frame (about 1.4 KB) and decodes straight into the ring — no
  `Value`, no buffer per chunk (`ota/upload/chunk` of the store likewise
  decodes into a stack buffer);
- at most one chunk is asked for at a time (plus the one a device needs
  while a prefetch is on its way), **2 chunks ahead** of the device, so
  after the first block the device does not wait;
- **heap guard**: below 20 KiB free or without a 6 KiB block nothing is
  asked for (devices get WAIT_FOR_DATA, `ota/status` shows
  `stream_paused`) until the heap recovers;
- `ota_device` events are built as text and serialized once for all
  subscribers (the API also no longer copies a `Value` per subscriber for
  any event).

A host test with a counting allocator (`crates/hub-ota/tests/stream_heap.rs`)
downloads 120 KB through the server and the streams: after the first
chunk the largest allocation is 256 B, the peak +0.9 KB, nothing grows.
`ota/status` counts per stream `asks`, `waits` (WAIT_FOR_DATA answers) and
`memory`.

If the client goes away (session closed), its streams wait **2 minutes**
for a client to offer the same image again (same session ID, e.g. Home
Assistant reconnecting after a hub restart); devices get WAIT_FOR_DATA
meanwhile, then ABORT, and the job ends `aborted` ("the stream's client did
not come back"). A device keeps what it has (zigbee-rs: suspended
download) and continues when the image is offered again and the upgrade
is started again. `hubctl ota-stream` notices a dropped connection itself
(a ping after 10 s of silence, given up after 25 s), connects again with
backoff, offers the file again and notifies the device again
(`ota/start … force` while the job runs). Home Assistant keeps streamed files on disk
(`.storage/zigbee_rs_hub_ota/`) and, after any reconnect, offers the files
of running jobs again and restarts them (`ota/start … force`); the file is
deleted when the job is done.

## Server behaviour

| Client command | Server |
|---|---|
| Query Next Image Request (0x01) | Records manufacturer code, image type, current file version and hardware version (persisted). Answers SUCCESS with the image when the device is approved (an `ota/start`, or the `auto` policy inside the window) and a download slot is free; otherwise NO_IMAGE_AVAILABLE (0x98) — the newer image is still shown as `available`. A query with the version a job installed completes the job (`done`) |
| Image Block Request (0x03) | Answers any offset of a stored or streamed image the device's job is for (so a download continues after a hub restart, after a device reset, from any offset the device asks for); WAIT_FOR_DATA (1 s) while a streamed block is fetched from the client; ABORT (0x95) for unknown or aborted images, a stream whose client did not come back, and devices that are not approved. Block = min(the device's MaxDataSize, `block_max` (48), NCP `max_asdu` − 17) |
| Image Page Request (0x04) | Serves up to 4 KiB, at most 128 blocks, as Image Block Responses spaced by the request's ResponseSpacing (at least `min_block_period_ms`), through the same pacing. Each block is read when it falls due and takes the next shared pacing slot, so a page holds no buffers and other devices keep their turn; the same request repeated while its page runs is ignored, a new page or block request of the device replaces it. A streamed block not here within 2 s ends the page early (the device asks again). The 128-block cap is decided (2026-11-24): a client with tiny blocks gets a short page and asks again, as the spec allows |
| Upgrade End Request (0x06) | SUCCESS from a device whose job for this image runs (or under `auto` inside the window): Upgrade End Response with upgrade time 0/0 ("now"; `upgrade_delay_s` > 0 sets a time), job `installing`; an aborted job, the `off` policy or no approval: Default Response ABORT (sent even if the client disabled default responses), never the go-ahead; the hub then reads CurrentFileVersion (0x0002 of the client) every minute from 30 s on, and the device's next query also confirms; not confirmed within 30 min → `failed`. Other status (INVALID_IMAGE, REQUIRE_MORE_IMAGE, ABORT) → job `failed` with the reason, Default Response SUCCESS |
| Query Device Specific File (0x08) | NO_IMAGE_AVAILABLE |
| anything else / manufacturer-specific | Default Response UNSUP_CLUSTER_COMMAND / 0x83 |

- **Image Notify** (unicast, payload type 3, QueryJitter 100, with the
  image's manufacturer code, type and version; type 0 for "check") starts
  a download: the zigbee-rs client, like zigpy's clients, only starts a
  transfer with an unknown server on an Image Notify. Sleepy devices get it
  through the engine's sleepy queue (sent after the device's next frame)
  and the NCP's indirect queue (`tx_options` bit 5); a waiting job is
  notified again every 5 minutes, 4 times.
- **Policy** per device (`manual`, `auto`, `off`), default `manual` (like
  ZHA). `auto` offers updates inside the schedule window (local time,
  `HH:MM`–`HH:MM`, may wrap midnight) and needs a synchronised clock; an
  automatic download outside the window is told WAIT_FOR_DATA until it
  opens. `manual` jobs ignore the window. `off`: never (decided
  2026-11-24) — `ota/start` is refused (`invalid_value`), queries get
  NO_IMAGE_AVAILABLE, block, page and Upgrade End requests ABORT, and a
  job running when the policy (the device's or the default) becomes `off`
  ends `aborted` within a second.
- **Concurrency**: `max_concurrent` (2) devices download at once; a device
  approved beyond that is `queued`, told NO_IMAGE_AVAILABLE and notified
  when a slot frees. A download without a block request for `stall_s`
  (300 s) frees its slot (`stalled`, it may continue).
- **Pacing**: block responses leave at most `blocks_per_s` (20) per second
  over all devices and per device not faster than `min_block_period_ms`
  (0), told once with WAIT_FOR_DATA's MinimumBlockPeriod; a response that
  would wait over 3 s becomes WAIT_FOR_DATA. A device has at most one
  response queued (a repeated or new request replaces it) and at most one
  page being sent. Responses to a sleepy device
  go out at once: it fast-polls during the download and the request woke
  the engine's sleepy queue.
- **Abort** (`ota/abort`): the next block request gets ABORT.
- **Persistence**: per device `ota/d/<device>` in the host store journal
  (identity, policy, job; progress every 10 %), settings `ota/cfg`. The
  device states are network records (`hub_store::kv::scope`, ownership
  tag): like the registry they are loaded once readable (at the start, or
  after `network/registry/adopt`; after the devices, and on their own when
  the gate closed between the two at the start), written only while the
  records are this hub's (meanwhile they stay in memory), and dropped by
  `network/registry/clear`; `ota/start` and `ota/devices/policy` answer
  `registry_read_only` until then, and `auto` acts as `manual` (a
  device's own `off` may be among the records not loaded). A job kept
  when the records load takes the device's stored policy (`off` ends it).
  `ota/cfg` is the hub's: always written.

## Image store (8 MB layout)

The store lives in the **inactive app slot** (`ota_1` while `ota_0` runs,
or the other way round; 3 MiB, at most 16 images). No `hubdata` is used,
so the catalog store's `hubdata` map is untouched.

- **Shared with the hub's own update**, arbitrated by
  `hub_app::ota_store::APP_SLOT`: a hub firmware update must call
  `begin_hub_update()` first, which fails with a clear error while a
  device downloads from the slot or an image is being uploaded; once it
  succeeded, the store drops its images and answers `busy` until the hub
  restarts. **Device images are lost with every hub update** (re-upload,
  or let Home Assistant fetch them again). The hub's own OTA does not
  exist yet; whoever adds it must take the lock.
- The slot is used only while the running app is **confirmed**: during a
  trial boot (`PENDING_VERIFY`) the slot holds the previous app for a
  rollback and the store is off (streaming still works). Before the first
  write the slot's otadata entry is invalidated
  (`esp_ota_invalidate_inactive_ota_data_slot`), so the bootloader never
  falls back to it — the price: no fallback to the previous app if the
  running one is ever corrupted.
- **On boot** the store reads its index (two A/B sectors, checksums) and
  then every image's OTA header back from the slot; an image whose header
  does not match its entry is dropped and never served. A slot holding
  anything else (the previous app after a hub update) is taken over:
  the store starts empty.

```text
sector 0, 1   index slots A/B: "ZOTA", format, n, generation, checksum + n × 112-B entries
sector 2..    images, one contiguous run of 4 KiB sectors each
```

- Index updates alternate between the two slots (entries first, the
  header last, SHA-256-based checksum), so a power cut keeps the previous
  index; host tests cut power at 0–4 000 programmed bytes.
- Uploads (`ota/upload/begin`, `chunk`, `commit`) write each 4 KiB chunk
  straight to flash (erasing sectors as they are reached); nothing larger
  than a chunk is in RAM. The header is checked with the first chunk
  (identifier, header version and length, total size = upload size, the
  same identity stored already → `exists`). Every 64 KiB the index records
  the progress: after a hub restart, `begin` with the same size and name
  continues at the last checkpoint. `commit` reads the image back from
  flash: SHA-256 (compared with the client's, when given), the
  sub-element chain (an element past the end is refused; trailing bytes
  and a missing upgrade image are warnings).
- Quota: the slot; an image needs a contiguous run of sectors (`no_space`
  tells the largest free run).
- Applicability (stored and streamed): manufacturer code, image type,
  newer file version, hardware version range (only when the header and
  the device both have one), upgrade file destination (device-specific
  files), and optional `min_file_version` / `max_file_version` (as in the
  Koenkk index).

**Sizes of real images** (Koenkk/zigbee-OTA `index.json`, 1 034 images,
2026-11-21): median 251 KB, 75 % ≤ 304 KB, 90 % ≤ 446 KB, largest 2.1 MB;
192 KiB holds 27 %, 256 KiB 56 %, 448 KiB 91 %, 512 KiB 92 %. The zigbee-rs
ESP32-C6/H2 images are 353–368 KB, BL702 299 KB: all fit the 3 MiB slot;
streaming needs no space at all.

## Clients and the online index

All clients first find the identifier `0x0BEEF11E` in the first 4 KiB
(some vendors prefix the file) and cut what is before it. Upload (store):
`ota/upload/begin` (size, name, SHA-256 when the client can compute it),
chunks of the hub's `chunk_max` from the returned offset,
`ota/upload/commit` (`replace` at begin replaces a stored image of the
same identity). Stream: see above. The web page is built for the image
(hub-web features `ota` and `ota-store`, set with the firmware's): without
`ota` (4 MB) it has no Updates tab at all; with a store (8 MB) it offers
**Upload to the hub**; with `ota` alone **Install from this tab**, where
the tab serves the image and asks to stay open until the update is done
(it also warns before closing). It also hides the tab when `hub/info` has
no `ota`.
Both list the devices (Update, Abort, Check, their policy) and the
images. The server settings (default policy, window, concurrency,
pacing, index URL) are API only (`ota/settings/set`, e.g. `hubctl
ws-call`), and the page has no index search of its own: a file from the
index is downloaded by hand, or Home Assistant does it (the trimmed
Updates UI stays, decided 2026-11-24; the page budget is
`hub_web::PAGE_BUDGET`, 34 KiB gzip since #66 and 36 KiB since the
event entities and catalog-update fixes; the 8 MB page with the store
was 33 249 B, with streaming 33 394 B, when the Updates tab was trimmed).

The index URL is a hub setting (`index_url`, default Koenkk's) that
clients share. Home Assistant fetches it a minute
after set-up and then daily
and shows a newer image from it as the update entity's latest version;
Install downloads the file, checks its SHA-512 (Python), cuts a prefix,
then uploads it (store) or streams it (no store) and starts the upgrade
with that image.

**Koenkk/zigbee-OTA** ([index.json](https://raw.githubusercontent.com/Koenkk/zigbee-OTA/master/index.json),
643 KB): `raw.githubusercontent.com` sends `Access-Control-Allow-Origin: *`
for the index and the images, and every entry's `url` is on that host
(checked 2026-11-21), so a browser client could fetch both. Fields
used: `manufacturerCode`, `imageType`, `fileVersion`, `fileSize`, `url`,
`sha512`, `minFileVersion`/`maxFileVersion`, `hardwareVersionMin`/`Max`,
`fileName`. There is no GitHub Pages copy of that repository
(koenkk.github.io/zigbee-OTA: 404).

**Pages mirror (proposal)**: a scheduled GitHub Action in
`faronov/zigbee-rs-definitions` publishes `ota/index.json` on its Pages
site (already the catalog mirror): Koenkk's entries unchanged (images stay
on raw.githubusercontent.com) plus entries for zigbee-rs's own images
(GitHub release assets do not allow cross-origin reads, so those files are
copied to Pages). Benefits: one stable, cached, CORS-enabled URL the UI and
HA default to; our own devices covered. Not done in this change.

## NCP and ZRHP

- The zigbee-rs NCP reports `max_asdu` 82 (capacity 6) and no
  `APS_FRAGMENTATION` (capability bit 2): `APS_DATA_REQUEST` longer than 82
  B is refused (`TOO_LARGE`; with `tx_options` bit 3 `UNSUPPORTED_COMMAND`).
  The hub reads `max_asdu` at every session and cuts blocks to
  `max_asdu − 17` at most; fragmentation is not needed (the block size
  field is one byte, clients ask for 40–64 B).
- ZRHP gaps found:
  1. `max_asdu` does not depend on the destination: 82 B fits a frame with
     NWK security and no source route (127 − MAC 11 − NWK 8 − NWK
     security 18 − APS 8); a source route adds 2 + 2 × relays. ZRHP has
     no per-destination maximum (EZSP `maximumPayloadLength`/ZBOSS
     `zb_aps_get_max_trans_size` style), so the hub keeps a margin (48-B
     blocks, 17 B for up to 7 relays). Proposed: a request
     `APS_MAX_PAYLOAD {dst}` or a capacity for the source-route budget.
  2. No `APS_FRAGMENTATION` in the NCP image (not needed for OTA).
  3. The NCP answers Match_Desc_req from the host endpoint registry
     (`NWK_ADD_ENDPOINT`: with `ota` the hub's endpoint 1 lists 0x0019 as a server
     cluster); devices that look for the OTA server with Match_Desc find
     the hub — to verify on hardware.
  4. Hub side, not ZRHP (solved with the ticketed engine transport, #64):
     every frame of the server is a request with a `RequestId`
     (`Hub::send_app_zcl`: submitted, queued for a sleepy device, or
     refused); its outcome (`EVT_APS_DATA_CONFIRM`, collected in the 1 s
     tick) returns through `Hub::on_outcome` to `Hub::take_app_outcomes`,
     never to the engine's retries. The server only counts it
     (`ota/status` `tx`, `tx_failed` per device): pacing stays time-based
     and a device asks again for a block that did not arrive. An Image
     Notify that never reached a sleepy device now shows as
     `not_delivered` or `unknown` instead of only a missing query.

## zigbee-rs devices

zigbee-rs's client (`zigbee-runtime` `OtaManager`/`OtaSession`,
`docs/book/src/advanced/ota.md`) works with this server as with ZHA:

- it starts a transfer only on an Image Notify (`ota/start` sends one;
  BL702 also queries 0x0000/1 periodically);
- it asks for 48-B blocks and retries an unanswered request after 2–5 s;
  the hub answers any offset, so its resume (same offer → saved offset)
  works, also across a hub restart;
- WAIT_FOR_DATA (`request_time − current_time`, MinimumBlockPeriod) and
  Upgrade End Response 0/0 ("now") are handled;
- it sends its hardware version (ESP32 devkit: 1): images built with a
  hardware range must include it.
- the ESP32 products do not start a new download while the running image
  is still unconfirmed (application-driven rollback, at most 15 min after
  an update): it ends the download before the first block with an Upgrade
  End INVALID_IMAGE and the job fails; start it again later.

**OTA identity**: the zigbee-rs products advertise manufacturer code
**0x6D6B** in their OTA client since faronov/zigbee-rs#6 (ESP32 devkit
image types 0x0001 C6, 0x0002 H2, 0x0003 C5; H2 thermometer 0x0012; BL702
0x0702; EFR32MG22 0x0022; ZBT-1 0x2101; table and conventions in the
zigbee-rs book, OTA chapter). Images must carry the pair the device sends:
a device still running firmware from before that change sends 0x1234 and
needs one transition file with 0x1234 in its header
(`create-esp32-ota.py --legacy-manufacturer`, `bl702_ota.py release
--legacy-manufacturer`, `zrs_boot_ota.py release --header-manufacturer
0x1234`).

**Building an OTA file** for a zigbee-rs device:

```sh
# ESP32-C6/H2 (zigbee-rs): build with the version, wrap, verify, zigpy index
cd ~/zboss_opensource/zigbee-rs-fork
tools/create-esp32-ota.py 0x00020001 out/ --chip esp32c6     # --sign-key KEY.pem for signed-ota
# any other firmware image: wrap it (one upgrade-image sub-element)
hubctl ota-make app.bin app.ota --manufacturer 0x6D6B --image-type 0x0001 \
  --version 0x00020001 --label "zigbee-rs C6 0.2.1" --hw 1-1
# upload to the hub (continue with the same command after a drop)
hubctl ota-upload ws://<hub>/api --token <token> out/*.ota
# or stream it (any hub with Zigbee OTA); exits when the device runs it
hubctl ota-stream ws://<hub>/api --token <token> out/*.ota --device zb:<ieee>
```

The file version must be the one the new firmware reports (the zigbee-rs
build stamps `ESP32_OTA_VERSION` into Basic SWBuildID and the OTA client),
newer than the running one; the image inside is what the product's
`FirmwareWriter` expects (ESP app image, MCUboot envelope with
`signed-ota`, the BL702 or EFR32 loader formats).

## Tests

- `hub-ota` (host): the format (streamed in any chunk size, broken files),
  the store (uploads, quota, fragmentation, resume after a restart, power
  cuts during index writes, foreign data, an expendable region taken over,
  an image whose header changed underneath dropped), the stream window
  (on demand, prefetch, a block across chunks, wrong chunks, a file that
  does not match its header, the client's grace time), the server with
  simulated clients: manual policy and confirmation, block size against
  `max_asdu`, concurrency and queue, abort, page requests, pacing and
  MinimumBlockPeriod, a download across a hub restart, a rejected image,
  `auto` with window and clock, hardware/version limits, unknown commands,
  Upgrade End without an approving job (aborted, `off`, never started)
  answered ABORT, `off` ending a running job, a hostile page request
  (MaxDataSize 1, PageSize 4096) bounded while another device downloads,
  MinimumBlockPeriod between page blocks, one queued response per device,
  an upload begun with `replace`; streamed: a whole download with one
  WAIT_FOR_DATA, a download in pages, the client away and back, ABORT
  after the grace time, the hub's stream limit. Both with and without the
  `store` feature.
- `hub-devices` (`scenarios/app_frames.rs`): an OTA command goes to the
  application without a Default Response; the application's frames get
  `RequestId`s, their outcomes go to it and are never retried by the
  engine; a refusal, a frame queued for a sleepy device (its ID when sent)
  and an outcome that never comes (`Unknown` after 10 minutes).
- `hub-app` endpoint: OTA Upgrade (0x0019) in the hub endpoint only with
  `ota`; an endpoint stored with the other cluster list is registered
  again once (`NWK_ADD_ENDPOINT` with the new descriptor), stored, and
  replayed after an NCP restart (`a_changed_hub_endpoint_is_registered_again_and_replayed`).
  The baseline scenario pins the endpoint with 0x0019 in every build, so
  its goldens do not depend on the feature.
- `hub-app`: upload, query, start, download and confirmation through the
  simulated NCP (`SimNcp::set_aps_hook` with the simulated OTA client;
  every frame ≤ the simulated NCP's 82-B `max_asdu`), app restart, no
  store; a streamed download served by the test's API session; the
  outcomes of the server's frames in `ota/status` `tx` and `tx_failed`
  (delivered, then a device out of reach: the frames of a check not
  delivered). Without
  `ota` (`--no-default-features`, the 4 MB combination): `ota/*` answers
  `unsupported`, `hub/info` has no `ota`, a Query Next Image gets the
  generic Default Response; CI checks with `cargo tree` that this build
  links no `hub-ota`.
- `hub-web`: the page of every feature combination within
  `PAGE_BUDGET` (36 KiB gzip) and parsing; without `ota` no Updates tab
  and no `ota/*` request.
- Release tooling (`tools/test_release_images.py`,
  `tools/test_hub_build.py`): a 4 MB image marked with `ota` or an 8 MB
  one without it is refused; the board entries are the features the build
  script selects.
- Home Assistant: the update entity (versions, install, progress, a
  device added later, a hub without OTA, install from the index with
  upload, streaming to a hub without a store incl. a reconnect that
  offers the file again) against the fake hub, and end to end against
  `hubctl --sim --sim-ota serve` with `hubctl ota-make` and `ota-upload`
  (`HUBCTL=…`).
- By hand: `hubctl --sim --sim-ota serve` with `hubctl ota-stream` and the
  web UI's Updates tab.

**Hardware:** streaming works on the lab board's octal-PSRAM 4 MB build
of the first branch (`zigbee-ota`, 2026-11-23: two complete downloads to
the C6, steps 3–10 pass, internal heap flat) with one unexplained hub
panic during the first run; the first test without the memory fixes
failed (heap). Results below. Still to do on hardware for this branch
(`zigbee-ota-v2`, on main after PR #51–#53, with the page and Upgrade End
fixes): the panic output in TinyUSB mode plus a 40-minute baseline soak of
main on the same octal build (to tell whether OTA causes it), a store run
on an 8 MB board (host tests only so far), and a device OTA end to end
with this branch's 8 MB image (or the 4 MB lab build with `ota`).

## Hardware test

The lab's 4 MB BR board (since 2026-11-22 an **N4R8, octal PSRAM**; its
standard state is hub main's octal 4 MB dev image) with **a lab build of
this branch's octal 4 MB dev image with feature `ota`** (the released
4 MB images have no Zigbee OTA; this build has no image store: streaming
only; no layout change) and the zigbee-rs **ESP32-C6 sensor**
(`products/esp32-zigbee-devkit`, image type 0x0001, hardware version 1)
joined to it. The Mac runs `hubctl` and reaches the hub over Wi-Fi.
`<hub>` is the hub's IP, `<t>` its API token (hub console `token`), `<c6>`
the C6's `zb:<ieee>`.

1. **Lock and record.** Take the BR-board lock (queue protocol). Note the
   hub's current app image and the C6's firmware (build features,
   version) — both are restored at the end.
2. **Build.** In this branch: `cargo build -p hubctl --release`; the
   octal 4 MB hub image with the OTA server (features `mqtt,ota`: no
   image store; not the CI artifact, whose 4 MB images have no Zigbee
   OTA):
   ```sh
   . ~/export-esp.sh
   RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --psram octal --locked -- --features ota
   TARGET_DIR=target-opi FLASH_SIZE=4MB MAX_APP_BYTES=3014656 \
     tools/make-app-image.sh release target/hub-4mb-opi.bin tbr-hub
   ```
   (a quad-PSRAM board takes `--psram quad` and `TARGET_DIR=target`;
   without PSRAM the hub runs no app, so no OTA server). Flash **only the
   app** at
   0x10000 (`factory`); do not erase `nvs` or `hubdata` (network,
   registry and OTA records stay). The boot log must report the octal
   PSRAM (8 MB) and the Rust heap in PSRAM; `hub/system` then shows the
   internal heap, which the stream's heap guard watches.
3. **Boot.** S3 log: no panic or reset loop; the hub forms/resumes its
   network, the C6 stays joined (entities update). Then
   `hubctl ws-call ws://<hub>/api --token <t> '{"id":1,"type":"hub/info"}'`:
   `ota` has `"stream": true`, `"store": false`. Note the free heap from
   `{"id":2,"type":"hub/system"}` (baseline).
4. **The C6's identity.** `{"id":3,"type":"ota/devices/check","device":"<c6>"}`
   sends an Image Notify (query jitter; a sleepy C6 gets it at its next
   poll). Within a few minutes `{"id":4,"type":"ota/devices/list"}` shows
   the C6 with `manufacturer_code`, `image_type` 1, `current_version`,
   `hw_version` 1 and `last_query`. **Finding if not:** the C6 does not
   find the server (Match_Desc for 0x0019 answered by the NCP from its
   endpoint registry) — capture the S3/NCP logs and stop here.
5. **The OTA file.** In zigbee-rs master (with faronov/zigbee-rs#6):
   ```sh
   tools/create-esp32-ota.py <V> /tmp/c6ota --chip esp32c6 [--legacy-manufacturer] [--sign-key KEY.pem]
   ```
   `<V>` = `current_version` + 1. `--legacy-manufacturer` when step 4
   showed `manufacturer_code` 0x1234 (firmware from before 0x6D6B);
   `--sign-key` only if the C6 runs a `signed-ota` build (then a key it
   trusts). The tool prints manufacturer, image type and version: they
   must match step 4 (version newer).
6. **Stream.** `hubctl ota-stream ws://<hub>/api --token <t> /tmp/c6ota/<file>.ota --device <c6>`.
   Expected: the header line, `stream session 65536…`, `<c6>: notified`,
   then `downloading N %` lines, `installing`, `done`, exit 0. About
   370 KB in 48-B blocks: expect 30–45 min (ZHA took 43 min). Every
   5 min note `hub/system` free heap and `ota/status` (`stream_paused`,
   the stream's `asks`, `waits`, `memory`); the S3 log must show no NCP
   `TOO_LARGE`/`UNSUPPORTED` for the block responses; `ota: heap low`
   means the guard paused the stream (note how often and how long).
7. **Client away, back in time** (at ~20 %): Ctrl-C `hubctl`, wait 60 s,
   run the same command again. Expected: `running download, notified
   again`, progress continues from where it was (not from 0 %).
8. **Client away too long** (at ~40 %): Ctrl-C, wait 150 s. Expected:
   `ota/devices/list` shows the job `aborted` ("the stream's client did
   not come back"), the C6 keeps working. Run the command again: progress
   continues from the C6's saved offset.
9. **Hub restart** (at ~60 %): reset the S3 and leave `hubctl` running.
   Expected: within 25 s `connection lost …; connecting again`, then once
   the hub is back `stream session …`, `running download, notified again`
   (or `notified`), and the C6 continues from its saved offset.
10. **Done.** After `done` the C6 reboots into `<V>`, rejoins and
    reports; `ota/devices/list` shows `current_version` = `<V>` and the job
    `done`; Basic SWBuildID shows the new version.
11. **Optional, web UI:** tab **Updates** → "Install from this tab" with a
    file `<V+1>`, keep the tab open: progress in the device row, the
    keep-open badge; closing the tab mid-download ends like step 8.
12. **Restore and release.** Flash the recorded standard hub image and,
    if the standard state needs it, the C6's recorded firmware; check the
    C6 is joined; release the lock, write the done marker.

**Report:** hub image (commit), C6 versions before/after, duration of the
download, number of WAIT_FOR_DATA / reruns, minimum free heap vs step 3,
any NCP errors, the Match_Desc result of step 4, the outcome of steps
7–10.

## Hardware results

### Round 4: PSRAM-safe atomics, ticketed transport (2026-11-24)

Lab BR board, S3 N4R8 (octal PSRAM), 4 MB dev layout, H2 NCP 0.1.2. Image
`zigbee-ota-v2` 6efb7ab (main 6fc074d merged: #68 `-s32c1i`, #69), lab
build `--features ota` (`mqtt,ota`), octal, 2 880 464 B, app only at
0x10000. Console `hub atomics`: Rust `AtomicU32` on PSRAM 200 000 /
200 000, 0 lost. C6 sensor (zigbee-rs master, 0x6D6B / 0x0001 / v3, hw 1,
sleepy) on PAN 0x1C06 ch 17 with an IKEA remote; files built with
zigbee-rs master 15ee9550 (`create-esp32-ota.py 4` / `5`, 399 618 /
399 634 B). **Result: all steps pass, two full downloads, no panic.**

| Step | Result |
|---|---|
| 3. Boot | `ota` = `{"stream": true, "store": false, "max_streams": 4, "stream_chunk": 1024}`; internal heap 126.6 KB free (min 121.7 KB, largest block 51 200 B) |
| 4. Identity | `check` → `sleepy: true`; the C6 queried 3.5 min later: 0x6D6B / 1 / version 3 / hw 1 |
| 6. Stream (v3 → v4) | `notified`; download from the C6's next poll (4 min), 1 KiB per ~5 s (~200 B/s) |
| 7. Away 60 s | (at 20 %) `running download, notified again`, continued at 21 % |
| 8. Away 150 s | (at 40 %) job `aborted` ("the stream's client did not come back"), the C6 kept reporting. Rerun: `notified`, continued at 41 % after 3.8 min |
| 9. Hub restart | (at 60 %, console `s3 reset`) `connection lost` after 10 s, reconnected 1 s later, `running download, notified again`, continued at 60 % |
| 10. Done | `installing` → `done` 91 s later; `current_version` 4, job `done`; SWBuildID "4" after a re-interview |
| Second run (v4 → v5) | uninterrupted: 38 min 40 s of download (172 B/s), `installing` → `done` 61 s later, `current_version` 5 |
| Heap | 24 five-minute samples over both runs: internal free 124.8–126.2 KB, `min_free` 118.3 KB (first boot) / 121.6 KB (after step 9), largest block 51 200 B throughout; `stream_paused` never true, stream `memory` 4 156 B, `waits` 1 (run 1: 58, while the client was away in step 7) |
| Transport | `ota/status.tx` from the step-9 restart to the end of the second run: 11 627 submitted + 4 queued for the sleepy C6 = 11 631 delivered, 0 not delivered / refused / unknown; per-device `tx_failed` 0; NCP `last_error` null |
| Panics | none: the RTC panic record kept only the pre-#68 one from main 95bbd85; the step-9 restart was the requested software reset |

The first attempt of the second run, started 1.5 min after the C6 had
installed v4, ended at 0 % with `failed: the device rejected the image
(INVALID_IMAGE)`: the zigbee-rs ESP32 firmware does not erase its other
slot while the running image is still unconfirmed (application-driven
rollback, confirmation within 15 min). The hub reported it as it should;
the retry 16 min later ran through. The round-3 panic did not recur in
these ~2 h of streaming with #68's fix (its cause was probably the
cross-core PSRAM atomics #68 fixed).

### Round 3: memory fixes, octal-PSRAM build (2026-11-23)

Lab BR board, S3 N4R8 (8 MB octal PSRAM), 4 MB dev layout. Image
`zigbee-ota` 6d09647 (rebased on main 0.1.8), octal variant
(`sdkconfig.octal`, `-opi`), 2 662 592 B, app only at 0x10000 (`nvs`,
`hubdata` kept). H2 NCP 0.1.0 (caps 0xD683B8, `asdu_max` 82). C6 sensor
child 0x93B2 (PAN 0x1C06, ch 17), Home Assistant connected.
**Result: all steps pass; one unexplained hub panic (below).**

| Step | Result |
|---|---|
| 3. Boot | `ota` = `{"store": false, "stream": true, "stream_chunk": 1024}`; internal heap 118.0 KB free (min 111.9 KB, largest block 45 KB), PSRAM 8 MB |
| 4. Identity | `check` → `sleepy: true`; the C6 queried within 1 min: 0x1234 / 1 / version 1 / hw 1 (pre-0x6D6B firmware). Match_Desc: still not exercised (the zigbee-rs client answers the Image Notify) |
| 5. File | the round-2 file (zigbee-rs master bd91d015, `--legacy-manufacturer`, 406 978 B, 0x1234 / 0x0001 / v2), so the C6's suspended download stayed valid |
| 6. Stream | `notified`, the download started at the C6's next poll (2.7 min) and continued from its saved offset (27 %); 1 KiB per 4–5 s (~200 B/s) |
| 7. Away 60 s | (at 33 %) `running download, notified again`, continued at 33 % |
| 8. Away 150 s | (at 41 %) job `aborted`, error "the stream's client did not come back", `available` gone; the C6 kept reporting. Rerun: `notified`, continued at 41 % |
| 9. Hub restart | (at 59 %, console `s3 reset`, `hubctl` left running) `connection lost …; connecting again` after 2 s, `stream session 65536`, `running download, notified again`, continued at 59 % |
| 10. Done | `installing` → `done` 61 s later, `hubctl` exited; `ota/devices/list`: `current_version` 2, job `done`; Basic SWBuildID "2" after a re-interview; the C6 rejoined and reports |
| Heap | 5-min samples: internal free 116.7–118.1 KB, `min_free` 110.6 KB at the lowest (step 3: 111.9 KB); PSRAM free ≥ 8 264 752 of 8 388 608 B (console); `stream_paused` never true, stream `memory` 4 156 B, `waits` 1–5, no `ota: heap low` |
| NCP | `last_error` null, APS 0 failed / 0 not delivered (8 488 delivered in the v3 run), no TOO_LARGE/UNSUPPORTED |

A second complete download, v2 → v3 (zigbee-rs master a8fb590c, 0x6D6B /
0x0001 / v3, 407 058 B), ran in one go with a debug variant of the same
commit (USB-Serial-JTAG secondary console, S3 in `s3 usj` mode so the
console could be captured): 39 min 20 s from the first block (172 B/s),
no reconnect, `installing` → `done` after 62 s, the C6 rejoined with
`current_version` 3 / 0x6D6B. Heap as above (internal min 110.6 KB). The
captured console showed no warning beyond one 404 for `/favicon.ico`.

**Open: an unexplained hub panic.** During the first run, at t = 592 s of
the boot that started with the step-9 reset (84 %), the hub restarted with
reset reason 4 (panic): crash streak 1, no Rust panic message, no failed
allocation, internal heap 117.5 KB / PSRAM 8.27 MB free in the last stack
watch, and an empty `coredump` partition. `hubctl` reconnected after 2 s and
the C6 continued (84 %), so the download still finished. One more
unexplained `connection lost` came at 58 % (23:48, before the step-9
reset; whether the hub restarted then is unknown). The 40-minute repro in
USJ mode (no TinyUSB) did not panic. Needed: the panic output (UART0, or
the USJ secondary console in TinyUSB mode) and a comparable soak of main
0.1.8 octal, to tell whether this branch causes it.

### Round 2: before the memory fixes (2026-11-22)

2026-11-22, lab BR board (S3 without PSRAM, 4 MB dev layout), H2 NCP 0.1.0
(zigbee-rs master 67fcb086, caps 0xD683B8, `max_asdu` 82), C6 sensor
(`esp32c6-sensor` 0.1.0, light-sleep SED, firmware from before 0x6D6B) as
child 0x93B2 of PAN 0x1C06 on channel 17. Home Assistant's integration stayed
connected (one more API session). **Result: FAIL**, the hub runs out of heap.

| Step | Result |
|---|---|
| 2. Image | `zigbee-ota` 6b98dad, `tbr-hub` 0.1.2 + OTA, 2 638 896 B, app only at 0x10000 (`nvs`, `hubdata` kept) |
| 3. Boot | no reset loop, network resumed, C6 kept reporting; `ota` = `{"store": false, "stream": true, "stream_chunk": 4096}`; heap 48.4 KB free (largest block 31.7 KB) |
| 4. Identity | `check` → `sleepy: true`; the C6's next query 1.5 min later: `manufacturer_code` 0x1234, `image_type` 1, `current_version` 1, `hw_version` 1 |
| 4. Match_Desc | not exercised: the zigbee-rs client sends no Match_Desc for 0x0019 (it answers the Image Notify), so the NCP's Match_Desc answer is still unverified |
| 5. File | zigbee-rs master bd91d015, `create-esp32-ota.py 2 … --chip esp32c6 --features light-sleep --legacy-manufacturer`: 406 978 B, 0x1234 / 0x0001 / version 2, hardware 1..1 |
| 6. Stream | `notified`, the download started at the C6's next poll (2.5 min); 8 KiB later the hub panicked (t = 576 s: 20.5 KB free, minimum 4.6 KB, a 5 464-B allocation failed, the base64 text of a 4 KiB chunk). Rerun after the restart: resumed at 1 %, panic again 20 s into the download (t = 296 s: 33 KB free, minimum 10 KB, a 4 096-B allocation failed). 2 of 2 runs |
| 7. Client away 60 s | (1 KiB build below) `running download, notified again`, continued at 20 % |
| 8. Client away 150 s | (1 KiB build) after the grace time the stream was dropped (`available` gone, job `image` null) but the job stayed `downloading`, not `aborted`; the C6 kept working. The rerun before it continued at 25 % |
| 9. Hub restart | covered by the 4 panics: the job survived each restart, the rerun printed `running download, notified again` and the C6 continued from its saved offset every time |
| 10. Done | not reached: the C6 still runs version 1 (best 27 %, offset 112 608) |

Experiment, **not part of this branch**: the same commit with
`hub_ota::stream::CHUNK` = 1024 (clients follow `len`). The download ran
for 8 minutes: 74 752 B in 455 s = 164 B/s (3.4 × 48-B blocks per s, the
whole file in about 41 min). Heap: 50.7 KB at boot, 39.2 KB after 4 min,
38.0 KB after 8 min (largest block 10.7 KB, minimum 14.6 KB). Then the hub
panicked about a minute after the step-7 rerun (t = 661 s: 21 KB free,
minimum 7.6 KB) and, after the next rerun, 50 s into the download
(t = 580 s: 12.5 KB free, minimum 3.0 KB). Smaller chunks only delay the
exhaustion.

Other findings:

- NCP: `last_error` null throughout, APS 0 failed (1 541 accepted in 8 min),
  no `TOO_LARGE`/`UNSUPPORTED` for the 48-B block responses.
- WAIT_FOR_DATA responses are not counted in the API, so the run could not
  report them.
- `hubctl ota-stream` does not notice a hub reset: without a ping or read
  timeout it hangs silently and has to be stopped and run again by hand.
- Crash streak 2 after two panics within 10 minutes of boot: a few more
  would put the hub into the bridge's safe mode.

Needed before this works on the 4 MB board without PSRAM: find the second
heap consumer (the 1 KiB run still drained about 17 KB within a minute
after a client reconnect; candidates are the per-session send queues of up
to 32 frames and Wi-Fi/lwIP receive buffers), parse `ota/block_data`
without a `serde_json::Value` copy and decode into the chunk buffer, a heap
guard that answers WAIT_FOR_DATA instead of allocating below a threshold
(like the job guard of #31), a smaller chunk, and a ping/read timeout in
`hubctl`. Retest on top of main 0.1.4+ (#36, lighter snapshots and
uploads); this branch does not contain it and conflicts with main.

**Changes after this run** (rebased on main 0.1.7 with #36 and #31; retest
pending): a fixed 4 × 1 KiB ring per stream, `ota/block_data` and
`ota/upload/chunk` decoded from the request text (no `Value`, no buffer per
chunk), one chunk asked for at a time, the heap guard (20 KiB free, a 6 KiB
block), `ota_device` events as text and every API event serialized once
for all subscribers, owned frames to the WebSocket sinks; a job ends
`aborted` when its stream is dropped after the grace time; `hubctl
ota-stream` pings an idle connection, notices a dead one within 25 s and
connects again by itself; `ota/status` counts `asks`, `waits` and
`memory` per stream. Not addressed: the per-session WebSocket send queue
(up to 32 frames) and Wi-Fi/lwIP buffers are shared with the rest of the
hub; the heap guard keeps OTA from taking the last of it.
