# Android companion API

Build on Linux from the repo root: `bash android/agent/build.sh` (only if the matching Agent payload is missing or changed), then `bash android/build-companion.sh`. Verify with `(cd android/dist/companion && sha256sum -c SHA256SUMS)`. See [corresponding source](licenses/SOURCE.md) and [upstream relationships](licenses/termux-api/ORIGIN.json).

In linked Termux, enable the matching `api.<name>` switch in the companion, then call:

```sh
maxa-device android api battery-status
maxa-device android api sensor --json '{"list":true}'
maxa-device android api storage-get --json '{}'
maxa-device android api storage-get --json '{"ticket":"<ticket returned above>"}'
```

`--json` is one object; omit it for `{}`. Unknown fields, unsafe legacy options and CLI/Unix FD transport are not accepted. Immediate results use `value`. Local-review calls first return `state: "awaiting_local_confirmation"` and a `ticket`. In the companion, with remote input disabled and screen recording stopped, open the local review and approve or reject. Poll the **same API name** with `{"ticket":"…"}`; before approval, `{"ticket":"…","cancel":true}` declines the request. A ticket is valid for three minutes within the current link, including a new CLI process. Do not resubmit a request whose outcome is unknown. The approval and the underlying Android permission are separate. Notification reading uses its own grant, unrelated to publishing.

### Arguments (optional fields marked `?`)

| Names | Parameters and scope |
|---|---|
| `battery-status`, `audio-info`, `camera-info`, `tts-engines`, `infrared-frequencies` | `{}`. `camera-info` enumerates visible camera metadata, not capture. |
| `brightness` | `{"brightness":0..255}` or `{"auto":true}`. |
| `volume` | `{}` lists streams; `{"stream":"music" or "alarm","volume":n}` changes an in-range volume. |
| `vibrate`, `torch` | `{"duration_ms"?:1..3000}`; `{"enabled":true/false}` (30 s automatic shutoff for owned torch). |
| `sensor`, `usb` | `{"list"?:true}` inventories only. No sensor samples/subscriptions or USB open/FD. |
| `infrared-transmit` | `{"frequency":Hz,"pattern":[positive µs,…]}`. Hardware-supported frequency, max 512 entries and 250 ms total. |
| `notification-channel`, `notification`, `notification-remove` | Channel `{"id":"…","name":"…"}` or `{"id":"…","delete":true}`; post `{"id":"…","channel":"…","title":"…","text":"…"}`; remove `{"id":"…"}`. Isolated app namespace; no action/command callbacks. |
| `toast` | `{"text":"…","short"?:true}`; requests a standard Android text toast. |
| `location`, `wifi-connectioninfo`, `telephony-cellinfo` | `{}` plus local review. Snapshot only; no listener, scan or Wi-Fi setting changes. |
| `notification-list`, `contact-list`, `call-log`, `sms-list` | `{"limit"?:1..50}` plus local review. Call log/SMS permissions are installer/role controlled. No background history capture. |
| `sms-send`, `telephony-call`, `share` | `{"number":"…","text":"…"}`, `{"number":"…"}`, `{"text":"…"}` respectively. System compose/dial/chooser handoff only; user controls the final action. |
| `dialog`, `fingerprint` | `{"title":"…","text":"…"}` and `{}`. Simple confirmation / Android strong biometric only. |
| `tts-speak`, `speech-to-text` | `{"text":"…"}` (preinstalled offline voice only); `{}` (on-device recognizer, API 31+, no network fallback). |
| `camera-photo`, `microphone-record`, `media-player`, `nfc` | `{}` (user accepts a JPEG thumbnail); `{"seconds"?:1..12}` (foreground stoppable audio); `{"seconds"?:1..60}` (selected local audio only); `{}` (one NDEF read, no UID/write/APDU). |
| `wallpaper`, `storage-get` | `{}` and a fresh, single-document system selection. `storage-get` returns up to 256 KiB; wallpaper accepts up to 4 MiB and does not return the image. No file paths, reusable grants or document CRUD. |
| `job-scheduler` | `{"action":"schedule","delaySeconds":5..120}` / `{"action":"pending"}` / `{"action":"cancel"}`; one fixed bounded reminder only, Android may defer it past expiry. |

The allowlist has 37 names (16 immediate and 21 local-review). Unknown names are rejected at both the CLI and APK dispatch boundaries. The general Termux Agent file operations are separate from this API.

**Upgrade:** Startup drops only retired capability toggles. For each recognized entry in the old private directory registry, it matches the exact URI to this APK's Android persisted read grants, releases that mode if present, and forgets the row only after Android confirms it is absent. Failures or unrecognized rows are retained for a later startup, without exposing their URI. Android does not distinguish different users of the same UID/URI/read grant. Unmatched grants, other modes, user files and existing Keystore keys are not touched.
