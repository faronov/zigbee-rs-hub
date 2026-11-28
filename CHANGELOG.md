# Changelog

User-facing changes of the hub firmware releases (`hub-v*` tags). The
release workflow (`.github/workflows/hub-release.yml`) puts the release's
section at the top of its GitHub release notes, above the generated
install instructions and the list of merged pull requests. A pre-release
`X.Y.Z-rc.N` uses its own section if there is one, else `X.Y.Z`'s.

## Unreleased

### New

- **Tuya devices run zigbee-herdsman-converters' definitions that need the
  `tuya` vendor template**, which the hub refused before ("requires
  template.tuya": the device stayed generic or ran TS011F_plug_1): TS011F
  plugs (`TS011F_plug_2`, `_3`), multi-gang strips, switch modules and DIN
  relays with child lock, indicator mode, power-on behaviour or outage
  memory, switch type and a countdown per switch (On With Timed Off);
  TS004F remotes and knobs such as HOBEIAN ZG-101ZD and Moes
  ERS-10TZBVB-AA (operation mode command/event; single, double, hold and
  rotation actions, and in command mode the Level Control steps); scene
  remotes such as the Lidl FB21-001 (scene buttons besides their on/off
  and dimming events); the measurements of relays and plugs whose
  definition asks for it (TS011F_din_smart_relay_polling; TS011F_plug_3's
  voltage, current and power with the next catalog) read every 60 s while
  they are reachable, less what the device reported meanwhile. 58 of
  the 223 generated definitions that use `tuya` run; the others name what
  they lack (e.g. `template.tuya.tuya_light`) and fall back as before.
  Upgrading: a TS004F paired before keeps its mode (command or event),
  as Zigbee2MQTT does; change it with its `operation_mode` select or
  Reconfigure, or remove the device and pair it again.
- **Xiaomi/Aqara devices run zigbee-herdsman-converters' definitions that
  need the `lumi` vendor template**, which the hub refused before
  ("requires template.lumi": they ran on the generic tier): battery,
  voltage, device temperature, power outage count and the other values of
  the Basic 0xFF01/0xFF02 and 0xFCC0 0x00F7 structs (both read for
  definitions with both, e.g. WXKG04LM); WXKG01LM single,
  double (and more) clicks, hold and release (a hold released between two
  ticks of the hub is still a hold); mini switches', wall switches' and
  Opple remotes' clicks (Opple: event mode written at pairing, release
  after a long hold, every button of the model even when the interview
  got only some endpoints); the 0xFCC0 settings (operation
  mode, power outage memory, LED, flip indicator, click mode, …); motion
  sensors' occupancy timeout, illuminance, detection interval and
  sensitivity; a switch's reset or leave announcement is answered as by
  Zigbee2MQTT. 99 of the 178 generated definitions that use `lumi` run;
  the others name what they lack (e.g. `template.lumi.lumi_light`).
  Upgrading: an Opple remote paired before keeps its mode (command or
  event); change it with its `operation_mode` select or Reconfigure, or
  remove the device and pair it again.

### Fixes

- **A TS011F plug's zero power, current or voltage could be lost** (template
  `tuya_metering_zero_debounce`): a zero held back the 11 s was dropped
  when the next reading came after those 11 s but before the hub's next
  1 s tick. It is published first now.

## 0.4.2 (2026-11-28)

A fix release for 0.4.1, with the Dongle Max's MG24 firmware update and
API 1.4 for the Home Assistant integration, which now has its own
repository. An update keeps all data.

### Highlights

- **Xiaomi (lumi) buttons and sensors finish their interview**, and a
  request to a sleepy device is retried a bounded number of times instead
  of at every wake-up for ever (#109).
- **Automations: the new trigger `reported`** ("received" in the editor)
  fires at every press of a button that reports the same state each time
  (an SOS button's `alarm`, an `action` that reads the same at every
  press), for which `changed` never fired (#110).
- **IKEA remotes with older firmware** (E1524/E1810 and others before
  2.3.075) are bound to group 901, so their presses reach the hub; a
  remote's or button's bindings to the hub are listed on its device page
  (#111).
- **API 1.4**: a network log of joins (also refused ones), rejoins and
  leaves, why a device is unavailable, interview details, the previous
  boot's crash record on the S3 (#112).
- **Security fixes**: no keys in the answers of `network/reset`,
  `network/restore` and `network/import`; another MQTT broker or user
  drops the stored password; removed devices that come back are asked to
  leave again. An explicit "leave network" (Network › New network)
  announces the leave on air; otherwise a zigbee-rs NCP 0.1.3 wipes
  without one. zigbee-rs crates at NCP 0.1.3 (#113).
- **SONOFF Dongle Max**: the hub replaces the MG24's firmware through its
  Gecko bootloader (SONOFF's EmberZNet ⇄ the zigbee-rs NCP; experimental),
  runs on either (EZSP or ZRHP backend), no longer crashes when it probes
  for the zigbee-rs NCP, and shows that NCP's log (#105–#107).
- **The Home Assistant integration moved** to
  [faronov/ha-zigbee-rs-hub](https://github.com/faronov/ha-zigbee-rs-hub)
  (HACS) (#108).

### Upgrading from 0.4.1

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and the hub's
  data, as before. The web flasher has the same entries as for 0.4.1.
- **Home Assistant:** install the integration from
  [faronov/ha-zigbee-rs-hub](https://github.com/faronov/ha-zigbee-rs-hub)
  (HACS › Custom repositories, type Integration), version **0.6.1** or
  later. A copied install: replace `custom_components/zigbee_rs_hub` with
  the one HACS installs and restart Home Assistant. The domain is the
  same, so config entries, devices and entities are kept (steps in its
  README).
- **Update the NCP to zigbee-rs NCP 0.1.3**
  ([ncp-v0.1.3](https://github.com/faronov/zigbee-rs/releases/tag/ncp-v0.1.3)):
  on the Thread BR board, flash the H2 with the web flasher (entry
  "Espressif Thread Border Router board, H2: ZRHP NCP"). It fixes the H2's
  RSSI (reported 10 dB low) and LQI (about 32 low), the secured rejoin of
  sleepy children, and wipes the NCP before a restore without a Leave on
  air. A Dongle Max running the zigbee-rs NCP takes that release's
  `efr32mg24-ncp-sonoff-dongle-m.gbl` through Settings › Zigbee radio
  firmware (MG24); the web flasher cannot reach the MG24.
- **Button automations that never fired:** edit the rule and choose the
  trigger "received" (`reported`) instead of "changed". Rules with
  `reported` or "changed to" are format 2: older firmware refuses them.
- **MQTT:** after changing the broker host or the user name, enter the
  password again.
- **API clients** that kept `backup` from the answers of `network/reset`,
  `network/restore` or `network/import`: fetch the file with the request
  named in `kept` (`network/backup/replaced` or `network/backup/last`).
- **tbr-bridge 0.2.7** (bridge mode): no functional change; rebuilt, its
  bytes differ from 0.2.6.

### New

- **Dongle Max: the MG24's firmware can be replaced from the hub**
  (experimental, run on the lab Dongle Max in both directions). Settings › Zigbee radio
  firmware (MG24) uploads a GBL file and flashes it through the MG24's
  Gecko bootloader, from the ESP32, without USB: SONOFF's EmberZNet NCP by
  the zigbee-rs ZRHP NCP, and back. The image is checked first (never one
  that would write the bootloader, an encrypted or compressed one, or a
  damaged one); the network is backed up with its keys and kept on the hub
  before anything is written; after the flash the hub restarts on the new
  firmware and puts the network back, under the same coordinator IEEE, so
  the devices keep working without pairing again. A failed or interrupted
  flash leaves the MG24 in its bootloader, which is never written: the hub
  starts anyway, says so and flashes again. Images without an
  application (an NV clear) write data only, leave the MG24's application
  running and restore nothing: the kept network waits for the next
  application image. The way back to SONOFF's firmware (an NV clear,
  then SONOFF's GBL, with their checksums) is in `docs/dongle-max.md`. The
  zigbee-rs NCP for the Dongle-M: `efr32mg24-ncp-sonoff-dongle-m.gbl` of
  zigbee-rs `ncp-v0.1.3`.
- **Dongle Max: the hub runs on either NCP firmware.** The image carries
  both coordinator backends (EZSP and ZRHP); at boot it asks the MG24
  which firmware it runs (setting `auto`, the default; `ezsp` or `zrhp` to
  fix it). `hub` on the console and `ncp/update/status` show what answered.
- **Dongle Max: `mg24 ezsp info` shows the custom EUI-64 and CTUNE
  tokens** (bring-up boot): whether the coordinator IEEE is the chip's own,
  and the crystal tuning, before the firmware is replaced.
- `network/backup` takes `keep`: the backup is also kept on the hub as a
  replaced network's (`network/backup/replaced`), whatever backend runs
  later.
- **The zigbee-rs NCP's log in the hub's log** (ZRHP backend). The hub
  now lets the NCP send its log: the records it kept since its reset
  (crystal tuning, where its IEEE comes from, NV state) and the ones after
  go into the hub's log (`hub/logs`, the console) as `ncp`, up to info by
  default, rate-limited. `network/ncp_log` (and `ncp log on|off|level <n>`
  on the Dongle Max console) sets the level, kept across restarts;
  `network/diagnostics` shows it with its counters, and whether the NCP
  keeps an EmberZNet backup's hashed link key root (`ezsp_hashed_tclk`).
- zigbee-rs crates at master `cb9b5c0e` (zigbee-rs NCP 0.1.3, PRs #22 to
  #26: restored EmberZNet devices are admitted by the zigbee-rs NCP's
  Trust Center; late LOG credit is delivered at once; `NWK_LEAVE_RESET`
  flags bit2, the Leave on air only when asked; `SEC_DERIVE_EZSP_KEY`).
- `network/reset` takes `leave` (Network › New network: "announce the
  leave on air"): the coordinator broadcasts a NWK Leave before the wipe,
  for an explicit "leave network". Without it (the default, and always
  before a restore or an import) the wipe is local on zigbee-rs NCP 0.1.3
  and later: the old network's routers keep the coordinator, so its kept
  backup can still be restored. Older zigbee-rs NCPs and EmberZNet always
  broadcast it.
- **Automations: a trigger at every press of a button that reports a
  state.** An SOS button's `alarm`, or an `action` sensor that reads
  `emergency` at every press, reports the same value each time, and a
  `changed` trigger never fired for it. The new state trigger `reported`
  ("received" in the editor, picked for `action`, `alarm` and `sos`
  sensors) fires at every value the device sends on its own (a report, a
  command, a Tuya data frame, a GP frame), of the value given; never at a
  read (the refresh after a restart, a poll), the answer to the hub's Tuya
  query or DP write (for 10 s, whatever the command: TS0601 devices send
  their own changes as dataResponse too), a value the hub set (a
  command's completion) or a restored state. An IAS Zone alarm counts when
  its bit is set and nothing else changed (not at a battery-low or tamper
  notification, nor when it clears); a notification sent again counts
  once. `changed` takes a value too ("changed to"). Rules that use either
  are format 2 (`v`): older firmware refuses them, other rules stay format 1.

### Security

- **No keys in the answers of `network/reset`, `network/restore` and
  `network/import`.** They carried the replaced network's automatic
  backup, network key and link keys included, to whoever made the
  request (with an API token), also in their error frames. They now
  carry `previous_backup`, a summary without keys (extended PAN ID, PAN
  ID, channel, devices, link keys, NWK and APS frame counters) and the
  request that returns the kept file (`network/backup/replaced` or
  `network/backup/last`): keys leave the hub only through the backup
  requests. The web UI fetches the file from there and offers it as
  before; `hubctl import` prints the summary. Clients that saved `backup`
  from these answers must fetch it with the request named in `kept`.
- **MQTT: another broker host or user name drops the stored password.**
  `mqtt/config/set` kept the password when only the host or the user name
  changed, so the hub sent it to whatever broker was set next. Now it goes
  unless the same request gives one (the settings form sends it only when
  typed: enter it again after changing the broker or user).
- **Removed devices are asked to leave again when they come back.** A
  device removed with `devices/remove` that still holds the network key is
  admitted again on its next secured rejoin (by EmberZNet, and by the
  zigbee-rs NCP since 0.1.3), and the hub took it back. Now, heard again
  while joining is closed (a rejoin, its announcement, a frame), it is
  asked to leave again (at most once a minute), stays removed, and a
  `removed_device_back` event (topic `network`) says so. Pairing it again
  while joining is open brings it back. The removal's leave request also
  reaches sleepy devices now (it waited for a wake-up that never came, as
  the device was gone). Only a new network key keeps a device out for
  good: `docs/device-support.md`, `devices/remove`.

### Home Assistant

- **The integration has its own repository,
  [faronov/ha-zigbee-rs-hub](https://github.com/faronov/ha-zigbee-rs-hub),
  and installs and updates through HACS** (HACS › Custom repositories,
  type Integration). Its version 0.6.1 is 0.6.0 with the new links (no
  functional change). The domain stays `zigbee_rs_hub`: a copied install
  moves to HACS without losing its config entry, devices or entities (the
  steps and a table of integration versions and hub firmware are in its
  README). `integrations/home-assistant` is gone from this repository; the
  release notes name the integration version to use.
- **API 1.4: what the integration's events, logbook, repairs and health
  need** (`docs/device-support.md` 14). A network log of joins (admitted
  or refused, and why: joining closed), rejoins, Device_annce frames and
  leaves, on both backends: `network/events` and the event
  `device_network` (the ZRHP backend now passes on refused joins and
  leaves too: a NWK leave without the rejoin flag, a parent's report or a
  trust center removal makes the device unavailable, `left`; a child the
  NCP dropped after a timeout, often a router that now routes through
  others, is only logged, `aged_out`, and keeps its host store record,
  which the import's inventory reads). The web UI lists it on the Network card
  ("Network log"): a device that "blinks then stops" shows its join
  attempts there. Device records gain `logical_type`, `rx_on_when_idle`,
  `rejoins`, `announces`, `leaves`, every `unavailable_reason` (`left`,
  `no_response`, `timeout`) with `unavailable_since`, and for a partial
  interview `interview_since`, `interview_failed` and `interview_quirk`;
  a reinterview sends `device_updated` at once. `hub/system` has a
  `boot_id` and, on the S3, the previous boot's `crash` (panic message,
  ESP-IDF panic record, crash streak). New events: `hub_updated` for the
  NCP session's `hub/info` fields (and `ncp_log` in `maintenance`),
  `definition_missing` (no catalog definition, or only refused ones),
  `topology_updated` (a new network map to fetch: maps up to 96 KiB are
  kept for every client, no longer 8 KB with every other result),
  `mqtt_status`; binding
  records say `queued` while a read waits for a sleeping device.
  `hubctl --sim --sim-devices serve` takes `sim/join`, `sim/announce`
  and `sim/leave` requests and `--sim-crash`, for the integration's tests.

### Fixes

- **Automations' run times were "20784 d ago"** when the hub started before
  its clock was set (SNTP): runs were timed on the clock from power-on,
  the page measured them against Unix time. They are now API times like
  `last_seen` (trace `time`, `last_run`), Unix time once the clock is set,
  runs from before it too.
- **Old Xiaomi buttons and sensors finish their interview.** A WXKG01LM
  (`lumi.sensor_switch`) stayed at "interviewing: basic_0" or
  "active_endpoints" for ever: it answers the interview's requests poorly,
  and each request was sent again at every frame from the device, without
  end. Now, as zigbee-herdsman and zigpy do, the model the device reports
  unasked completes the interview: the WXKG01LM and the MCCGQ01LM
  (`lumi.sensor_magnet`) get their endpoints from zhaquirks' signatures as
  soon as a request of their interview has failed once, other `lumi.`
  models when a step finally fails; the missing identity is filled in
  (LUMI, code 0x1037, a sleepy end device on battery) and the interview
  ends `partial`, with `interview_quirk: "lumi"` in `devices/diagnostics`;
  the next rejoin resumes the real interview. The button's presses (reports
  of its On/Off client cluster) fire the `action` event (`off` pressed, `on`
  released). Xiaomi devices get no binds or reporting a definition does not
  ask for, and their Basic 0xFF01/0xFF02 structs are accepted with any
  manufacturer code and with the malformed lengths they come with.
  Battery, voltage and the single/double/hold clicks need
  zigbee-herdsman-converters' `lumi` template, not in this firmware yet.
  If a button stays stuck, hold it until its LED blinks: it rejoins and
  reports its model.
- **Sleepy devices: retries are bounded, and any frame wakes the queue.**
  A request to a sleeping device that timed out went back into its queue
  as new, so a step the device never answers was retried at every wake-up
  for ever. Now its tries count across wake-ups: 6 for the Node
  descriptor, 2 for the endpoint list, a Simple descriptor and each Basic
  attribute, 3 for anything else; then the step is skipped (the interview
  ends `partial` and resumes at the next rejoin), a bind fails and a
  binding table read ends with its error instead of "reading…". A
  device's Device_annce and its rejoin now send what waits for it (they
  only resumed the interview), and after a rejoin under a new short
  address what waited goes to the new one (also when a frame with its IEEE
  shows the new address). A binding or group request no longer shows
  "until it wakes" after it was sent. The web UI says "press a button on
  the device to wake it" while a sleepy device's interview or binding
  table read waits.
- **IKEA remotes with old firmware reach the hub.** An E1524/E1810 TRADFRI
  remote (also E1743, E1766, E1841) before firmware 2.3.075 can only be
  bound to a group, and sent its presses to a group the hub was not in.
  As zigbee-herdsman-converters does, configure now binds its On/Off to
  group 901 and the hub endpoint joins 901 and 0x0000 (where an unbound
  remote sends, so a press also sends a bind that waited for it); its
  groupcasts fire its `action` events. Newer firmware is bound to the hub
  as before. The hub endpoint's groups for devices (also a definition's
  `configure: group`) are joined again after an NCP reset or a restore,
  on both NCPs, and when a Zigbee2MQTT/ZHA migration imports the remote;
  they count toward the EZSP multicast table, and a full table stops the
  joins (`TABLE_FULL`) until a group is left.
- **A remote's or button's bindings to the hub are listed** (Bindings in
  the device page, `bindings/list`): configure binds its client clusters
  (a SONOFF SNZB-01's On/Off) to the hub endpoint, and the record now
  shows them once the device confirmed them, so a button whose presses do
  not arrive shows whether it is bound.
- **Basic attributes are read one per request** (identity first), as
  zigbee-herdsman does: some devices do not answer a read of several. A
  failed optional attribute (power source, versions, date code, build ID)
  no longer makes the interview partial, and a Default Response instead of
  the answer no longer stalls it. The manufacturer, model and versions are
  also taken from any Basic report or read answer when the interview has
  not got them (strings end at a NUL); a device whose endpoint list is
  unknown gets stub endpoints from the frames it sends.
- Settings › Hub: the time zone field's example is the browser's own
  zone.
- **Dongle Max: the hub no longer crashes when it asks the MG24 for the
  zigbee-rs NCP.** The probe ran the ZRHP link driver on a stack smaller
  than the driver needs (its thread entry reserved 16 KB for code it never
  ran), so the check after flashing the zigbee-rs NCP, and every boot
  after it, crashed the hub. The driver's entry now takes 720 bytes and
  the probe runs it as the hub's own ZRHP backend does. A ZRHP probe or
  backend that crashes the hub at two boots in a row is now skipped: the
  hub starts without it (UART parked), says why, and stays reachable to
  flash another image; after 10 minutes without a crash the next boot
  tries ZRHP once more, and a hub update starts afresh.
- **Dongle Max: `mg24 bootloader` shows the whole menu.** The bootloader
  prints its menu by itself on entry; the wait for the menu after the
  newline stopped at that first copy and cut the second one short.
- **Dongle Max: the zigbee-rs NCP's boot log reaches the hub's log.** The
  hub reset the MG24 at every start, so the records the NCP kept from its
  own reset were lost, among them the NV it erased at its first boot after
  a flash and where its crystal tuning and IEEE come from. A zigbee-rs
  NCP that runs and answers is no longer reset when the hub starts; any
  other NCP is, as before.
- **LQI and RSSI after a hub restart are marked as old.** Devices showed
  the LQI and RSSI kept from before the restart as if measured. The device
  record now has `link_stale: true` until the device's next frame, and the
  web UI shows no LQI or RSSI meanwhile.
- **EZSP: a restore after a hub restart starts above the NCP's frame
  counters.** When the hub restarted between leaving a network and
  restoring a backup, the restore started from the backup's counters
  alone; the counters the EmberZNet NCP still reports are now floors, as
  without the restart.

### Tested on hardware

HIL on main 227471ad (2026-11-28):

- Thread BR board (lab S3, 4 MB octal image; the H2 updated to zigbee-rs
  NCP 0.1.3 with the web flasher, as above): the network resumed with the
  same PAN ID and extended PAN ID; rejoins and announcements in
  `network/events`; the API 1.4 fields, `topology_updated`, the crash
  record, no keys in a restore's answers; `min_free` about 120 KB, as
  with 0.4.1; stable across reboots.
- SONOFF Dongle Max: the hub image installed; the MG24 updated to the
  zigbee-rs NCP 0.1.3 GBL through the hub (`ncp/update`: XMODEM, 3,612
  blocks, none resent, about 52 s, the network kept); no NCP reset at the
  hub's start; the NCP's log in the hub's log; a `reported` trigger fired
  on a real report; the way back (NV clear, SONOFF's stock EmberZNet, the
  network restored, the sleepy C6 rejoining secured) and to 0.1.3 again.

### Known limitations

- Not run on hardware: binding an IKEA remote with old firmware to group
  901 (no such remote in the lab) and the leave requests of
  `devices/remove` and to a removed device that comes back.
- After the H2 was flashed, a session left in its ROM loader (e.g. an
  interrupted web-flasher session) makes the hub find no NCP: the hub
  does not reset the H2 at its start. Run `hub ncp reset hard` on the
  console or power-cycle the board (a fix is planned for the next
  release).
- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio, so Zigbee
  on a quad-PSRAM board is still untested.
- Resuming a partial interview is tested on the simulated backends only
  (a failing interview step cannot be forced on hardware).
- Migration: host-tested on both simulated backends (ZRHP and EZSP) with
  Zigbee2MQTT and ZHA files. On hardware a Zigbee2MQTT-format import of
  the Dongle Max's own network ran (preview `same`, names applied, devices
  complete); an import onto another radio or network and a ZHA import
  have not. Green Power devices are not imported (commission them again);
  after a change of the coordinator IEEE some devices keep the old trust
  center address (prefer a radio that takes the backup's IEEE); Z2M/ZHA
  per-device settings and external converters are not imported
  (`docs/migration.md`, Limits).
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- Definitions that need Zigbee2MQTT's vendor templates `tuya` or `lumi`
  are not run yet: TS011F devices for which Zigbee2MQTT picks
  `TS011F_plug_2`/`_3`, multi-gang strips and DIN relays use
  `TS011F_plug_1`, marked **approx.**; Xiaomi devices have no battery,
  voltage or single/double/hold clicks.
- An rx-on end device restored or imported from a backup becomes a
  sleepy child of the coordinator (the backup format has no capability).
- A zigbee-rs NCP (the Thread BR board's H2) may keep a stale child table
  entry for a device that moved to another network: backups list it, and
  a restore re-creates it as a partial device (remove it with
  `devices/remove`; a fix is tracked).
- SONOFF Dongle Max: experimental. Ran on the lab unit: the network,
  joining and interviews, reports, a network backup export and restore
  onto the same network, the hub name and version, a Zigbee2MQTT-format
  import of its own network, the 0.4.1 claim refusals and restore, the
  MG24 firmware update in both directions with the hub on either NCP
  (above). Not yet run: Ethernet, `stock confirm`, the self-update, installation
  through SONOFF's web console, a restore or import onto another radio,
  the recovery claim of 0.4.1. The stock EmberZNet 7.4.5 does not report
  the APS frame counter: its backups have no TCLK counter, and a restore
  gives the APS counter a margin of 2²⁰ (note `counters_unknown`). No
  Green Power; no crash record (`hub/system` `crash` is the S3's); at
  most 8 groups on the hub's endpoint with SONOFF's NCP (its multicast
  table; an IKEA remote's group 901 and group 0 count); if the first
  boot cannot claim SONOFF's `files` partition the hub keeps its data in
  RAM only (the banner says why). Not offered by the web flasher (its
  erase option would wipe SONOFF's firmware): use SONOFF's web console or
  esptool with the two USB parts, after the otadata check.
- SONOFF Dongle Max, while SONOFF's firmware is still in `ota_0` (USB or
  web-console install): a hub image in `ota_1` that fails to boot (e.g. an
  interrupted USB write) makes the bootloader start SONOFF's firmware,
  which reformats the `files` partition. The hub's devices, names,
  automations and settings are lost (the Zigbee network stays in the MG24
  and the devices come back through the NCP import, unnamed). The
  recovery claim and a hub backup bring back the devices and groups;
  names, areas, automations and settings have to be set again.

## 0.4.1 (2026-11-27)

A fix release for 0.4.0: an interview that ended partial finishes when the
device rejoins, backups restore routers as routers and no longer roll the
frame counters back on the Dongle Max, and the Dongle Max can take its
storage back after SONOFF's firmware reformatted it. Same boards, images
and web flasher entries as 0.4.0; an update keeps all data.

### Fixes

- **A partial interview resumes when the device rejoins.** A device whose
  interview ended `partial` (a descriptor or Basic read failed or timed out,
  e.g. a router busy right after its join) stayed so until
  `devices/reinterview` was called by hand. Now its next rejoin or
  Device_annce (the moment a sleepy device is awake) resumes the interview
  asking only the answers it lacks (e.g. just the Node descriptor of a
  device whose endpoints were read), so a failure then loses nothing it
  knew; the user's names and settings stay. Works on both coordinator
  backends. A device that keeps failing is retried at most 5 times per hub
  run, the first at once, then no sooner than 10, 20, 40 and 80 min after
  the previous try, however often it announces; a complete interview is
  never repeated. `devices/diagnostics` counts the resumptions
  (`interview_resumes`). A join and the Device_annce of the same rejoin no
  longer send each interview request twice, and an Active_EP_rsp with an
  error status now makes the interview partial (it was taken as a device
  without endpoints).
- **Dongle Max: a refused storage claim says why, and the hub's region
  can be claimed again after SONOFF's firmware reformatted it.** Every
  refusal of `storage/claim` (and of the first boot's claim) now has a
  code and a message, in the request's error and in `storage/status`
  `last` (now `{"code", "message"}`); 0.4.0 refused the claim after such
  a reformat without a word. That state (the hub's claim recorded in NVS,
  which every boot that finds the region the hub's now writes if it is
  missing; SONOFF's new file system over the hub's data; the claim's copy
  of SONOFF's files intact) is shown as `reformatted` in `storage/status`
  and in the RAM-only banner, and `storage/claim` with `"recover": true`
  (console `files recover confirm`; Settings › Storage asks for it)
  claims the region again, keeping the copy of SONOFF's original files
  for `stock confirm`. The hub restarts with an empty store, takes the
  MG24's network and imports its devices; the most recent hub backup
  restored onto it brings back its groups and devices (not names or
  automations: `docs/dongle-max.md`, "Recovery after SONOFF's firmware
  reformatted `files`"). Make the first update a self-update: the
  bootloader's fallback is then a hub image, not SONOFF's firmware.
- **EZSP: a backup restored over its own running network no longer rolls
  the frame counters back.** The restore started from the backup's NWK
  and APS frame counters plus 10 000; the coordinator sends thousands of
  frames a day, so a backup a day or two old made the devices drop its
  frames. It now starts from the NCP's own counters when they are higher
  (read before the network is left; note `floor_raised`), and a stored
  network whose counters the NCP does not report gets a margin of 2²⁰
  (note `counters_unknown`). A zigbee-rs NCP (ZRHP) already started above
  the counters it used.
- **EZSP: after a restore the NCP resumes above the frame counters used
  before it, and the report says where.** EmberZNet stores the counters a
  restore writes truncated to 4096 and resumed from there at the
  restore's reset (lab unit, 7.4.5: `nwk_fc_start` 96 058 reported,
  94 208 in use): up to 4095 below the intended margin, and below the
  counters the restored network had already sent (devices drop such
  frames). The start is now on a 4096 boundary and the NCP is reset a
  second time: the session after that reset starts above every counter
  used before (lab Dongle Max: start 110 592 written, the NCP resumed at
  118 785), and `nwk_fc_start`/`aps_fc_start` are read back then. The
  short session between the two resets may still reuse a few counters
  (its many-to-one route requests and link status only).
- **Backups restore routers as routers.** A zigbee-rs NCP (the Thread BR
  board's H2) lists a router that joined directly through the coordinator
  as its child, and `network/backup` wrote it so (`is_child`). Restoring
  that backup made the router a sleepy end-device child: the NCP held
  every frame to it for a poll a router never sends (seen on hardware: the
  interview hung, ZCL requests got no APS ACK, `devices/remove` never
  reached it). New backups write such a router as a plain device (as
  zigpy and Zigbee2MQTT mean `is_child`), and a restore or import of an
  older file gives no child entry to a device the hub knows as a router
  (its node descriptor) or the file lists as one (`type: "router"`), on
  both backends. End devices stay children.

### Upgrading from 0.4.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and the hub's
  data (`hubdata` on the S3, the claimed `files` region on the Dongle
  Max), as from 0.3.0 to 0.4.0. The web flasher has the same entries as
  for 0.4.0.
- **Make a new backup after the update** (Settings › Backup). A backup
  made by 0.4.0 or earlier on the Thread BR board lists routers that
  joined directly through the coordinator as children. 0.4.1 restores
  such a file correctly onto a hub that knows those devices as routers
  (e.g. the same hub, its devices interviewed); restored onto an empty
  hub, such a router would still become a sleepy child.
- **Frame counters on restore:** a restore starts the network's frame
  counters from the higher of the backup's and the NCP's own counters,
  plus a margin (10 000; 2²⁰ for a counter the NCP stores but does not
  report, note `counters_unknown`). The NCP's counters count on EZSP
  since 0.4.1 (note `floor_raised`); a zigbee-rs NCP already kept them.
  On EZSP the start is rounded up to a multiple of 4096 and the NCP
  resumes above it after the restore's second reset, so the reported
  `nwk_fc_start` is higher than with 0.4.0. An older backup restored
  onto the same network is therefore safe. A restore
  onto another radio cannot know what the old coordinator sent after the
  backup: take the backup last and do not run the old coordinator again.
- **SONOFF Dongle Max:** download a hub backup (Settings › Backup) before
  every update over USB or through SONOFF's web console, and prefer the
  hub's self-update (Settings › Hub firmware; the first one with *Replace
  SONOFF's firmware* ticked): the bootloader's fallback is then a hub
  image, not SONOFF's firmware, which would reformat the hub's region. If
  that happened (`storage/status` and the banner say `reformatted`):
  [Recovery after SONOFF's firmware reformatted `files`](https://github.com/faronov/zigbee-rs-hub/blob/hub-v0.4.1/docs/dongle-max.md#recovery-after-sonoffs-firmware-reformatted-files).
- **Home Assistant:** integration **0.6.0** as in 0.4.0 (unchanged).
  **tbr-bridge 0.2.6** as in 0.4.0 (unchanged, the same bytes).

### Tested on hardware

- Thread BR board (lab S3, zigbee-rs NCP on the H2): install, devices,
  `interview_resumes` in `devices/diagnostics`, a backup (an end device
  listed as a child) and a `network/restore` of a fresh backup.
- SONOFF Dongle Max (stock EmberZNet 7.4.5): the new `storage/status`
  fields, the claim refusals `confirm_required` and `already_claimed`; a
  restore over its own network: start 110 592 written, the NCP resumed at
  118 785 after the second reset (one 4096 step above the model, the safe
  side), its counter at or above the start 1 and 5 min later, a sleepy
  end device (C6) reporting and answering reads after it, a normal reboot
  resuming ahead.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio, so Zigbee
  on a quad-PSRAM board is still untested.
- Resuming a partial interview is tested on the simulated backends only
  (a failing interview step cannot be forced on hardware).
- Migration: host-tested on both simulated backends (ZRHP and EZSP) with
  Zigbee2MQTT and ZHA files. On hardware a Zigbee2MQTT-format import of
  the Dongle Max's own network ran (preview `same`, names applied, devices
  complete); an import onto another radio or network and a ZHA import
  have not. Green Power devices are not imported (commission them again);
  after a change of the coordinator IEEE some devices keep the old trust
  center address (prefer a radio that takes the backup's IEEE); Z2M/ZHA
  per-device settings and external converters are not imported
  (`docs/migration.md`, Limits).
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- Definitions that need the vendor template `tuya` are not run yet
  (TS011F devices for which Zigbee2MQTT picks `TS011F_plug_2`/`_3`,
  multi-gang strips, DIN relays): they use `TS011F_plug_1`, marked
  **approx.**, and the device's diagnostics name Zigbee2MQTT's definition.
- An rx-on end device restored or imported from a backup becomes a
  sleepy child of the coordinator (the backup format has no capability).
- A zigbee-rs NCP (the Thread BR board's H2) may keep a stale child table
  entry for a device that moved to another network: backups list it, and
  a restore re-creates it as a partial device (remove it with
  `devices/remove`; a fix is tracked).
- SONOFF Dongle Max: experimental. Ran on the lab unit: the network,
  joining and interviews, reports, a network backup export and restore
  onto the same network (0.3.0; the joined sensor kept working), the hub
  name and version, a Zigbee2MQTT-format import of its own network, the
  0.4.1 items above. Not yet run: Ethernet, `stock confirm`, the
  self-update, installation through SONOFF's web console, a restore or
  import onto another radio, the recovery claim of 0.4.1 itself (it needs
  a region SONOFF's firmware reformatted; host-tested, also with real
  LittleFS). The stock EmberZNet 7.4.5 does not report the APS frame
  counter: its backups have no TCLK counter, and a restore gives the APS
  counter a margin of 2²⁰ (note `counters_unknown`). No Green Power, no
  MG24 firmware update; at most 8 groups on the hub's endpoint (the NCP's
  multicast table); if the first boot cannot claim SONOFF's `files`
  partition the hub keeps its data in RAM only (the banner says why). Not
  offered by the web flasher (its erase option would wipe SONOFF's
  firmware): use SONOFF's web console or esptool with the two USB parts,
  after the otadata check.
- SONOFF Dongle Max, while SONOFF's firmware is still in `ota_0` (USB or
  web-console install): a hub image in `ota_1` that fails to boot (e.g. an
  interrupted USB write) makes the bootloader start SONOFF's firmware,
  which reformats the `files` partition. The hub's devices, names,
  automations and settings are lost (the Zigbee network stays in the MG24
  and the devices come back through the NCP import, unnamed). The
  recovery claim and a hub backup bring back the devices and groups;
  names, areas, automations and settings have to be set again.

## 0.4.0

### Highlights

- **Migration from Zigbee2MQTT and ZHA without pairing again.** The hub
  restores the network (key, PAN, channel, frame counters) from the
  source's backup and takes the devices from its records: they appear at
  once with their names, areas and entities, and nothing is sent to them
  until they are heard from (with the same coordinator IEEE their
  configuration is kept; on another radio they are configured again).
  Three ways in, each with a preview first (`network/import/preview`; the
  running network's backup is kept before an apply):
  - web UI: Settings › *Migrate from Zigbee2MQTT or ZHA* (reads Z2M's
    `database.db`, `coordinator_backup.json` and names in the browser);
  - Home Assistant: the admin services `zigbee_rs_hub.import_from_zha`
    (ZHA's `zigbee.db` with HA's names and areas; apply refused while ZHA
    runs) and `zigbee_rs_hub.copy_names_areas` (names and areas from ZHA or
    Zigbee2MQTT devices);
  - `hubctl import z2m DIR` / `hubctl import zha zigbee.db`.

  Both backends (the Thread BR board's zigbee-rs NCP and the Dongle Max's
  EmberZNet NCP). Never run the old coordinator again on that network.
  What carries over and the limits: `docs/migration.md`.

### Fixes

- **MQTT: why the broker refused the bridge.** `mqtt/status` `last_error`
  now says `connection refused: not authorized`, `bad user name or
  password` or `the broker closed the connection after CONNECT (it may
  require a user name and password)` instead of `disconnected`, and the
  web UI points to a Home Assistant user or the Mosquitto add-on's logins
  when an anonymous client is refused (the add-on does not accept
  anonymous clients; checked against it). A password without a user name
  is no longer sent (MQTT 3.1.1 forbids it).
- **Dongle Max: hub version.** `hub/info` `name` carries the image's
  version (0.3.0 said `zigbee-rs-hub 0.2.0`), and the Home Assistant
  integration reads the version from the name on every board (the Dongle
  Max's hub device had no software version).
- **Tuya TS011F plugs get their definition** (`TS011F_plug_1` and its white
  labels, e.g. NEO NAS-WR01B for `_TZ3000_gjnozsaz`): the hub implements
  the vendor template `tuya_metering_zero_debounce` they need. As in
  Zigbee2MQTT, a zero power, current or voltage reading is published only
  if it stands for about 11 s (spurious zeros are dropped), and power is 0
  while the plug is off. TS011F devices for which Zigbee2MQTT picks a
  definition that needs template `tuya` (not implemented yet:
  `TS011F_plug_2`/`_3`, multi-gang strips, DIN relays) use `TS011F_plug_1`
  meanwhile, marked **approx.** on the device card and page (the tooltip
  names Zigbee2MQTT's definition and what it needs; `definition_approximate`
  in the device record and diagnostics). The definitions a device could
  not run stay with it across restarts and are checked again, shard by
  shard, after a catalog update or a firmware that gains the feature.
  After the update
  use Find definition on the device page (or Catalog › Fetch missing
  definitions) to fetch it.
- **A removed device stays removed.** A device removed with
  `devices/remove` (with or without `force`) that the coordinator still
  listed came back at the next hub restart or NCP reset, as a partial
  device without a name (the hub imports the devices the NCP knows). The
  removal now purges the NCP's tables (ZRHP: the NCP's Trust Center record
  and link key, `TBL_INVALIDATE`, and the host store's device record;
  EZSP: an APS Remove-Device to a router, the device's key table entry),
  and the hub remembers the last 64 removed devices so that the NCP's
  child or neighbor entries (kept until the device ages out) do not bring
  them back. A removed device that joins or announces itself again is
  added as before. Forming, resetting or restoring the network forgets
  them. Checked on hardware: a force-removed sensor stayed removed after
  an NCP hard reset and a hub reboot.
- **Find definition no longer says "the catalog has no definition" for a
  definition the firmware cannot run.** Catalog definitions that need a
  feature the hub lacks (a vendor template, e.g. every Tuya TS011F plug
  definition in 0.3.0) were dropped without a word, so such devices stayed
  generic for no visible reason. The hub now names them: Find definition
  says which definition matched and what it needs, and the device's
  diagnostics list it (`catalog_refused`).
- A definition fetched from one catalog shard no longer keeps a better one
  from a later shard out (a device's keys span several shards).
- A catalog shard or definition shared by several devices is no longer
  checked or fetched for the first of them only: uploading it for one
  device does not end the check of the others.

### Other changes

- **Web UI: tabs in a sidebar.** On screens 900 px wide or more the tabs
  are a sidebar on the left; on phones a menu button (☰, with the open
  tab's title) opens them over the page (a tab, Escape or a tap on the
  dimmed page closes it). No horizontal scroll at 320 px any more (the
  device grid overflowed there).
- **Device cards: status at the top.** Battery, LQI, RSSI, last seen and
  power source sit right under the card's title, in the device page's
  order, in the same place on every card (they were under the entities).
- **Device diagnostics in the web UI.** The device page's **Diagnostics**
  button shows what the device reported (manufacturer, model ID,
  application, stack and HW version, SW build ID, date code, ZCL version,
  power source, logical type, endpoints with their clusters) and what the
  hub made of it (definition and its layers, catalog definitions this
  firmware cannot run, unknown attributes, Tuya DPs), with **Copy JSON**
  and **Download JSON** (`zigbee-rs-hub-diagnostics-<IEEE>.json`: the
  record, entities, states and `devices/diagnostics`, as Home Assistant's
  "Download diagnostics"). A module loaded on the first click.
- Internal: every firmware crate pins esp-idf-svc 0.53.0 (the MQTT error
  reporting reads its event layout). The zigbee-herdsman-converters parity
  test also resolves the definitions that need features the hub lacks,
  with every feature, and lists them by feature.

### Upgrading from 0.3.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and `hubdata`
  (devices, settings, network backup), as from 0.2.0 to 0.3.0. The web
  flasher has the same entries as for 0.3.0 (standard images, the
  Developer edition, the Dongle Max listed without an install button).
- **Home Assistant:** integration **0.6.0** (`integrations/home-assistant`):
  the migration services above and the hub version read from the name.
- **tbr-bridge 0.2.6** (the S3 app for bridge mode on Espressif's stock
  layout): no functional change (esp-idf-svc pinned). Its image no longer
  carries a build time, so later releases rebuild it byte for byte and it
  keeps its version until its code changes.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio, so Zigbee
  on a quad-PSRAM board is still untested.
- Migration: host-tested on both simulated backends (ZRHP and EZSP) with
  Zigbee2MQTT and ZHA files. On hardware a Zigbee2MQTT-format import of
  the Dongle Max's own network ran (preview `same`, names applied, devices
  complete); an import onto another radio or network and a ZHA import
  have not. Green Power
  devices are not imported (commission them again); after a change of the
  coordinator IEEE some devices keep the old trust center address (prefer
  a radio that takes the backup's IEEE); Z2M/ZHA per-device settings and
  external converters are not imported (`docs/migration.md`, Limits).
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- Definitions that need the vendor template `tuya` are not run yet
  (TS011F devices for which Zigbee2MQTT picks `TS011F_plug_2`/`_3`,
  multi-gang strips, DIN relays): they use `TS011F_plug_1`, marked
  **approx.**, and the device's diagnostics name Zigbee2MQTT's definition.
- SONOFF Dongle Max: experimental. Ran on the lab unit: the network,
  joining and interviews, reports, a network backup export and restore
  onto the same network (0.3.0; the joined sensor kept working), the hub
  name and version, a Zigbee2MQTT-format import of its own network. Not
  yet run: Ethernet, `stock confirm`, the self-update, installation
  through SONOFF's web console, a restore or import onto another radio.
  No Green Power, no MG24 firmware update; at most 8
  groups on the hub's endpoint (the NCP's multicast table); if the first
  boot cannot claim SONOFF's `files` partition the hub keeps its data in
  RAM only (a banner says so). Not offered by the web flasher (its erase
  option would wipe SONOFF's firmware): use SONOFF's web console or
  esptool with the two USB parts, after the otadata check.
- SONOFF Dongle Max, while SONOFF's firmware is still in `ota_0` (USB or
  web-console install): a hub image in `ota_1` that fails to boot (e.g. an
  interrupted USB write) makes the bootloader start SONOFF's firmware,
  which reformats the `files` partition. The hub's devices, names,
  automations and settings are lost (the Zigbee network stays in the MG24
  and the devices come back through the NCP import, unnamed). Download a
  hub backup (Settings › Backup) before updating over USB; after a hub
  self-update the fallback is the previous hub image. Re-claiming after
  such a reformat is refused without a reason (fixed in 0.4.1: `recover`).

## 0.3.0

### Highlights

- **Crash fix (important for every 0.2.0 hub).** Rust atomics now go through
  ESP-IDF's PSRAM-safe functions: the S3's compare-and-set instruction is
  not atomic between the two cores on PSRAM, where the hub keeps its data.
  0.2.0 hubs could crash or reboot under API load (`LoadProhibited`,
  `Cache error`, mutex asserts; on the lab board after 40–80 network
  diagnostics); with the fix it ran 1000 in a row without a crash
  (measurements in the README, `hub atomics`). Updating every 0.2.0 hub
  is recommended.
- **Zigbee device OTA** (Upgrade server, cluster 0x0019) in the 8 MB
  images: an Updates tab, `ota/*` API, Home Assistant `update` entities,
  an image store in the inactive app slot, larger images streamed from a
  client. The 4 MB images are built without it.
- **Reporting configuration:** editable defaults per value class
  (Settings › Reporting defaults), per-entity overrides on the device
  page (`min`, `max`, `change` in the entity's units, or `off`), and
  readback of what the device applied.
- **Device page** (`#device/<id>`) with compact device cards on the
  dashboard; event entities (remotes) show their last action; grouped
  sections (Sensors, Controls, Events, Configuration, Diagnostic).
- **Developer edition (devtools)** of the 8 MB images
  (`tbr-hub-0.3.0-8mb-devtools[-opi]-…`, its own entry in the web flasher)
  and of the Dongle Max (`zrh-donglem-0.3.0-devtools-app.bin`): the web UI's
  developer tools (the ZCL/Tuya console) always on, `edition: "devtools"`
  in `hub/info`. For developers; everyone else keeps the standard image.
  Same layout and data as the standard image: switching keeps `nvs` and
  `hubdata`.
- **Last panic in RTC memory:** the console's `info` shows the last
  ESP-IDF panic and how many boots ago it happened.
- **SONOFF Dongle Max (experimental):** the hub application on the
  dongle's stock EmberZNet NCP (`zrh-donglem`, EZSP backend), installed
  into `ota_1` of SONOFF's unchanged partition table. It runs on one lab
  unit (network formed, a sleepy device joined, interviewed and reporting;
  EZSP session up in 2.6 s); the rest of its hardware checklist is pending
  (`docs/dongle-max.md`).

### Other changes

- 4 MB images: the Zigbee2MQTT-compatible MQTT bridge is now included;
  the advanced ZCL/Tuya console and frame tap are 8 MB only (feature
  `advanced`). `hub/system` reports the memory mode, build features and
  limits. A board without the PSRAM its image was built for keeps the
  Zigbee network but does not start the application and says which image
  to flash.
- Definitions: device selection matches zigbee-herdsman-converters for
  6,081 of its 6,082 fingerprint vectors. White labels by model ID,
  software build (Paulmann `RGBWW`), logical type, hardware version and
  endpoints (SONOFF SNZB-03/SNZB-05, BTicino K4027C), with zhc's exact
  names; `date_code` clauses. Tuya week-schedule thermostats (ENGO EONE,
  TS0601_thermostat_2, …) get their definition instead of staying generic.
  The interview keeps the Green Power endpoint 242 for matching, so 13
  more models match (e.g. Shelly 2PM, OSRAM DALI, Gledopto GL-C-007-1ID).
  A *generic* badge and *Find definition* for unmatched devices.
- SWBuildID and DateCode are read again after an OTA upgrade and on
  rejoin, so the definition and white label follow the new firmware
  without a re-interview.
- Web UI: the rarely used tabs (Converters, Catalog, ZCL console,
  Updates) load when first opened, which keeps the page small; a page left
  open over a firmware update reloads once.
- "Find definition" no longer fails with `SHA-256 differs from the
  installed manifest`: the catalog is updated before shards are fetched.
- Automations: device-then-entity pickers that tell identical devices
  apart; the run mode is labelled and explained in the editor (*single*
  ignores triggers while a run is in progress, *restart* starts over); the
  network map lays out small networks readably.
- **Time zone picker** in Settings › Hub: search the zone by name or take
  the browser's (the hub keeps the IANA name, `hub/time` `tz_name`, next to
  its POSIX string); Home Assistant can keep the hub's zone in sync with
  its own (integration option). Settings no longer scrolls sideways on
  phones.
- `network/info` reports the coordinator's table sizes (`capacity`).
- SONOFF Dongle Max: network backup and restore through EZSP (Open
  Coordinator Backup, the format zigpy/ZHA and Zigbee2MQTT use, so a
  network can move between them and the hub); host-tested against the
  simulated NCP (EZSP 13–18), not yet run on a Dongle Max.
- Hub records are tagged with the network they belong to (`hub/owner`):
  `network/info.registry`, `network/registry/adopt` and
  `network/registry/clear` (both need `"confirm": true`). After an adopt,
  or once the records are readable again, groups, bindings, last values,
  automations and OTA states load too (a start-up race left them hidden
  until the next restart).
- Internal: the coordinator boundary (host store, coordinator traits, the
  non-blocking ZRHP pump, ticketed device transport) and the EZSP backend
  as a build option. The TBR board images stay on ZRHP and need no NCP
  update (ZRHP 1.0, H2 NCP 0.1.2).

### Upgrading from 0.2.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and `hubdata`
  (devices, settings, network backup). At its first boot 0.3.0 adds one
  record to the host store, the owner tag (`hub/owner`, derived from the
  stored network); nothing else is rewritten and no data are lost. Going
  back to 0.2.0 is safe: it ignores the tag.
- **Web flasher:** pick the board, leave *Erase* unticked; the image is
  chosen from the detected flash and PSRAM. Changing the layout (4 MB dev
  ↔ 8 MB) still needs an erase: back up the network first.
- **ZCL/Tuya console (8 MB, Dongle Max):** the tab is now a developer
  tool: switch on **Developer mode** (Settings › Hub) to see it; the
  console's API (`zcl/*`, `tuya/send`, topic `frames`) stays available with
  the API token. The developer edition always shows it.
- **4 MB boards:** no Zigbee OTA (no Updates tab, `ota/*` answers
  `unsupported`) and no advanced console any more; MQTT is new there.
  **8 MB boards:** OTA, the advanced console and MQTT.
- **Home Assistant:** integration **0.5.0**, from this release's source
  (`integrations/home-assistant`): it adds the firmware `update` entities
  of the Zigbee OTA server (8 MB hubs) and the option to sync the hub's
  time zone from Home Assistant.
- **tbr-bridge 0.2.5** (the S3 app for bridge mode on Espressif's stock
  layout): no functional change; a new version because the rebuilt image
  differs from the 0.2.4 file of release 0.2.0.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio (in-place
  upgrade with the data kept, web UI and API under load, no crash), so
  Zigbee on a quad-PSRAM board is still untested.
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- SONOFF Dongle Max: experimental, full hub hardware test pending
  (Ethernet, `stock confirm`, self-update not yet run); installation
  through SONOFF's web console not yet verified on hardware; network
  backup/restore through EZSP host-tested only, not yet run on a Dongle
  Max; no Green Power, no MG24 firmware update; at
  most 8 groups on the hub's endpoint (the NCP's multicast table); if the
  first boot cannot claim SONOFF's `files`
  partition the hub keeps its data in RAM only (a banner says so). Not
  offered by the web flasher (its erase option would wipe SONOFF's
  firmware): use SONOFF's web console or esptool with the two USB parts.
