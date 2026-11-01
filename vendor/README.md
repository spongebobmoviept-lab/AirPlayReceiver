# Modified copy of shairplay

`shairplay/` is [shairplay-rust](https://github.com/metaneutrons/shairplay-rust) 0.10.0 by Fabian
Schmieder, licensed LGPL-3.0-or-later (see `shairplay/LICENSE`), modified for this app on 2026-11-01.
`shairplay-changes.patch` has every change. To move to a newer version, take the new crate and reapply it.

**What changed and why:**
- **Lost packets:** it never asked the sender to resend them, so each packet lost on Wi-Fi was a click. It does now, and waits longer for them.
- **Pause:** real senders send `RTP-Info: seq=N;rtptime=T`, which never parsed, so pause played on for 2 seconds. It also only took effect when the next packet arrived.
- **Remote control:** finding the phone's port blocked connecting for up to 2 seconds and fell back to a port iPhones don't use. It now runs in the background and looks again when needed.
- **`RemoteCommand::Raw`:** for commands the crate has no name for, like `volumeup` and `shuffle_songs`.
- **`TrackMetadata::play_status`:** the `caps` value, which is how an iPhone says whether it is playing.
- **Renaming:** waits for the mDNS goodbye to go out, so phones drop the old name.
- **Smaller:** `Audio-Latency: 11025` instead of 0, TXT records in a fixed order and matching a known working AirPlay 1 speaker, `image/none` clears old artwork.

`shairplay/airport.key` is not a secret. It is the well-known AirPort Express key every AirPlay 1 receiver uses, and it ships in the original crate.
