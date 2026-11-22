# Zigbee OTA Upgrade on the hub

The hub is the **OTA Upgrade server** (cluster 0x0019, ZCL r8 chapter 11) of
its network: it serves Zigbee OTA files to the devices' OTA clients. Code:
[`crates/hub-ota`](../crates/hub-ota) (file format, image sources — store
and streams —, server state machine, simulated client; sans-IO, host
tested), [`crates/hub-app/src/ota.rs`](../crates/hub-app/src/ota.rs) and
`ota_store.rs` (the server on the app thread, the `ota/*` API), the web
UI's **Updates** tab, the Home Assistant `update` platform and `hubctl
ota-upload` / `ota-stream` / `ota-make` / `--sim-ota`. The API is in
[device-support 14.7](device-support.md#147-ota-upgrade).

**The hub downloads nothing.** There is no TLS on the hub. Images reach
the devices in one of two ways:

| | 8 MB layout (product) | 4 MB dev layout |
|---|---|---|
| **Stream** (a connected client serves the image on demand) | yes | yes (the only way) |
| **Store** (uploaded, kept on the hub) | yes: the inactive app slot (3 MiB) | no (built without it) |

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
`{"type": "ota/block_needed", "session", "offset", "len"}` (4 KiB chunks);
the client answers `ota/block_data {session, offset, data}`. The hub keeps
**two chunks (8 KiB) per stream** and asks for the next chunk as soon as
the device reads the newest one, so after the first block the device does
not wait; the hub checks chunk 0 against the offered header. At most 4
streams.

If the client goes away (session closed), its streams wait **2 minutes**
for a client to offer the same image again (same session ID, e.g. Home
Assistant reconnecting after a hub restart); devices get WAIT_FOR_DATA
meanwhile, then ABORT. A device keeps what it has (zigbee-rs: suspended
download) and continues when the image is offered again and the upgrade
is started again. Home Assistant keeps streamed files on disk
(`.storage/zigbee_rs_hub_ota/`) and, after any reconnect, offers the files
of running jobs again and restarts them (`ota/start … force`); the file is
deleted when the job is done.

## Server behaviour

| Client command | Server |
|---|---|
| Query Next Image Request (0x01) | Records manufacturer code, image type, current file version and hardware version (persisted). Answers SUCCESS with the image when the device is approved (an `ota/start`, or the `auto` policy inside the window) and a download slot is free; otherwise NO_IMAGE_AVAILABLE (0x98) — the newer image is still shown as `available`. A query with the version a job installed completes the job (`done`) |
| Image Block Request (0x03) | Answers any offset of a stored or streamed image the device's job is for (so a download continues after a hub restart, after a device reset, from any offset the device asks for); WAIT_FOR_DATA (1 s) while a streamed block is fetched from the client; ABORT (0x95) for unknown or aborted images, a stream whose client did not come back, and devices that are not approved. Block = min(the device's MaxDataSize, `block_max` (48), NCP `max_asdu` − 17) |
| Image Page Request (0x04) | Serves up to 4 KiB as Image Block Responses spaced by the request's ResponseSpacing, through the same pacing |
| Upgrade End Request (0x06) | SUCCESS: Upgrade End Response with upgrade time 0/0 ("now"; `upgrade_delay_s` > 0 sets a time), job `installing`; the hub then reads CurrentFileVersion (0x0002 of the client) every minute from 30 s on, and the device's next query also confirms; not confirmed within 30 min → `failed`. Other status (INVALID_IMAGE, REQUIRE_MORE_IMAGE, ABORT) → job `failed` with the reason, Default Response SUCCESS |
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
  opens. `manual` jobs ignore the window.
- **Concurrency**: `max_concurrent` (2) devices download at once; a device
  approved beyond that is `queued`, told NO_IMAGE_AVAILABLE and notified
  when a slot frees. A download without a block request for `stall_s`
  (300 s) frees its slot (`stalled`, it may continue).
- **Pacing**: block responses leave at most `blocks_per_s` (20) per second
  over all devices and per device not faster than `min_block_period_ms`
  (0), told once with WAIT_FOR_DATA's MinimumBlockPeriod; a response that
  would wait over 3 s becomes WAIT_FOR_DATA. Responses to a sleepy device
  go out at once: it fast-polls during the download and the request woke
  the engine's sleepy queue.
- **Abort** (`ota/abort`): the next block request gets ABORT.
- **Persistence**: per device `ota/d/<device>` in the host store journal
  (identity, policy, job; progress every 10 %), settings `ota/cfg`.

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
`ota/upload/commit`. Stream: see above. The web UI hides the Updates tab
on a hub without an OTA server (`ota` missing in `hub/info`), shows the
upload only with a store, and offers **Install from this tab** (a file,
or an index entry): the tab serves the image and asks to stay open until
the update is done (it also warns before closing).

The index URL is a hub setting (`index_url`, default Koenkk's), so the web
UI and Home Assistant use the same one. Home Assistant fetches it a minute
after set-up and then daily
and shows a newer image from it as the update entity's latest version;
Install downloads the file, checks its SHA-512 (Python), cuts a prefix,
then uploads it (store) or streams it (no store) and starts the upgrade
with that image. The web UI's **Find updates** lists the index's images
for the hub's devices with **Download to hub** (store) or **Install from
this tab** (stream).

**Koenkk/zigbee-OTA** ([index.json](https://raw.githubusercontent.com/Koenkk/zigbee-OTA/master/index.json),
643 KB): `raw.githubusercontent.com` sends `Access-Control-Allow-Origin: *`
for the index and the images, and every entry's `url` is on that host
(checked 2026-11-21), so the web UI fetches both from the browser. Fields
used: `manufacturerCode`, `imageType`, `fileVersion`, `fileSize`, `url`,
`sha512`, `minFileVersion`/`maxFileVersion`, `hardwareVersionMin`/`Max`,
`fileName`. The UI checks SHA-512 with WebCrypto only in a secure context
(`https://` or `localhost`); on `http://<hub>/` it cannot, and says so
(the hub still checks the format and its own SHA-256). There is no GitHub
Pages copy of that repository (koenkk.github.io/zigbee-OTA: 404).

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
     (`NWK_ADD_ENDPOINT`: the hub's endpoint 1 lists 0x0019 as a server
     cluster); devices that look for the OTA server with Match_Desc find
     the hub — to verify on hardware.
  4. Hub side, not ZRHP: the engine's transport does not hand
     `EVT_APS_DATA_CONFIRM` per request to the app, so an Image Notify that
     never reached a sleepy device is only noticed by the missing query.

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
# or stream it (any hub, the 4 MB one included); exits when the device runs it
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
  `auto` with window and clock, hardware/version limits, unknown commands;
  streamed: a whole download with one WAIT_FOR_DATA, the client away and
  back, ABORT after the grace time. Both with and without the `store`
  feature.
- `hub-app`: upload, query, start, download and confirmation through the
  simulated NCP (`SimNcp::set_aps_hook` with the simulated OTA client;
  every frame ≤ the simulated NCP's 82-B `max_asdu`), app restart, no
  store; a streamed download served by the test's API session.
- Home Assistant: the update entity (versions, install, progress, a
  device added later, a hub without OTA, install from the index with
  upload, streaming to a hub without a store incl. a reconnect that
  offers the file again) against the fake hub, and end to end against
  `hubctl --sim --sim-ota serve` with `hubctl ota-make` and `ota-upload`
  (`HUBCTL=…`).
- By hand: `hubctl --sim --sim-ota serve` with `hubctl ota-stream` and the
  web UI's Updates tab.

**Hardware: not done yet** (procedure below). The store needs an 8 MB
board (host tests only so far).

## Hardware test

The lab's 4 MB BR board with the **standard 4 MB dev image of this
branch** (streaming only; no special build, no layout change) and the
zigbee-rs **ESP32-C6 sensor** (`products/esp32-zigbee-devkit`, image type
0x0001, hardware version 1) joined to it. The Mac runs `hubctl` and
reaches the hub over Wi-Fi. `<hub>` is the hub's IP, `<t>` its API token
(hub console `token`), `<c6>` the C6's `zb:<ieee>`.

1. **Lock and record.** Take the BR-board lock (queue protocol). Note the
   hub's current app image and the C6's firmware (build features,
   version) — both are restored at the end.
2. **Build.** In this branch: `cargo build -p hubctl --release`; hub image:
   ```sh
   . ~/export-esp.sh
   (cd firmware/hub && cargo build --release --no-default-features --features app)
   cp target/xtensa-esp32s3-espidf/release/tbr-hub /tmp/hub-4mb.elf
   FLASH_SIZE=4MB MAX_APP_BYTES=3014656 ELF=/tmp/hub-4mb.elf \
     tools/make-app-image.sh release /tmp/hub-4mb.bin tbr-hub
   ```
   (or the `thread-br-s3-hub-4mb-dev` app image of the PR's CI/release
   build). Flash **only the app** at 0x10000 (`factory`); do not erase
   `nvs` or `hubdata` (network, registry and OTA records stay).
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
   5 min note `hub/system` free heap; the S3 log must show no NCP
   `TOO_LARGE`/`UNSUPPORTED` for the block responses.
7. **Client away, back in time** (at ~20 %): Ctrl-C `hubctl`, wait 60 s,
   run the same command again. Expected: `running download, notified
   again`, progress continues from where it was (not from 0 %).
8. **Client away too long** (at ~40 %): Ctrl-C, wait 150 s. Expected:
   `ota/devices/list` shows the job `aborted` (stream gone), the C6 keeps
   working. Run the command again: progress continues from the C6's saved
   offset.
9. **Hub restart** (at ~60 %): reset the S3 (keep `hubctl` stopped), and
   once the hub is back run the command again. Expected: the C6 got ABORT
   while no stream was offered; after the rerun (`notified again`) it
   continues from its saved offset.
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
