# lagless

Stream everything your Windows PC plays to your Mac, with the lowest latency and highest quality the link allows. Built to replace AudioRelay-style apps that lag, cut out and slowly drift.

- **Lossless hi-res:** uncompressed 24-bit PCM at whatever rate Windows outputs (44.1 / 48 / 96 / 192 kHz). The Mac's output is switched to the same rate automatically.
- **Tiny latency:** 1 ms frames, a 64-sample Mac device buffer, and a jitter buffer that starts at 2 ms and only grows if your link actually needs it.
- **No dropouts:** every packet also carries a copy of the previous frame, so a lost packet is repaired from the next one with no extra delay.
- **No drift:** the PC's and Mac's sound cards never tick at exactly the same speed. lagless tracks this and corrects it inaudibly, so the delay doesn't creep up over an hour and then glitch. It uses a 32-tap windowed-sinc resampler that is flat to about 22.5 kHz at 48k.
- **Any link, and it picks the fastest:** Wi-Fi, Ethernet, USB-Ethernet, Thunderbolt bridge or Bluetooth PAN. The receiver announces itself on every network, and the sender measures each path and uses the quickest. If you plug in a cable mid-song, it hops onto it.
- **Self-healing on bad links:** if the link starts losing audio (bad Wi-Fi, Bluetooth), it switches to Opus at 320 kbps with heavier redundancy. Once the link is clean for 30 s it switches back to lossless.

## Download

Grab the latest from **[Releases](https://github.com/leoble123/lagless/releases/latest)**:

- **Mac:** `Lagless.dmg`. Open it and drag Lagless to Applications.
- **Windows:** `lagless-windows-x64.exe`. It's a single file with no installer; put it anywhere and run it.

Both are the same app. Windows starts as the **sender** and the Mac starts as the **receiver**; you can flip either one with the Send / Receive switch.

**First launch prompts**, because these builds aren't signed with paid Apple/Microsoft certificates:
- **Mac:** if macOS says it can't verify the developer, right-click Lagless → **Open** → **Open** (or System Settings → Privacy & Security → **Open Anyway**). Click **Allow** when it asks about finding devices on your local network.
- **Windows:** if SmartScreen appears, click **More info** → **Run anyway**. If the firewall asks, allow **Private networks**.

That's all the setup. The PC finds the Mac by itself and starts streaming as soon as something plays.

## The app

- The **orb** shows live end-to-end latency. It's green under 25 ms, amber under 45 ms and red above that, and it pulses with the music.
- The **L/R meters** show what's being sent (on the PC) or received (on the Mac).
- **Chips** show the link (Cable / Wi-Fi / Bluetooth…), the format (e.g. *24-bit · 96 kHz*), *Lossless*, and the round trip.
- The **graph** shows latency over the last minute, and **Dropouts** counts packets repaired, lost and rebuffered.
- The **sliders icon** opens settings: output device (DACs and IEM amps), latency floor, device buffer, capture source, quality and loss protection.

## Auto-update

Every 6 hours the app checks this repo's latest GitHub Release. If there's a newer one, it downloads it in the background and verifies its SHA-256 checksum against the one published with the release. Then an **Update to x.y.z** button appears. Click it to restart into the new version, or just quit: the update installs on exit, so the next launch is the new version. You can turn this off in settings, and `lagless update` does it from a terminal.

**Shipping a new version:** bump `version` in `Cargo.toml`, commit, then `git tag v0.2.0 && git push --tags`. GitHub Actions builds the `.dmg` and `.exe`, publishes the release, and every installed copy picks it up.

## About "zero latency"

True zero isn't physically possible, but the goal is to get under what you can perceive. Typical end-to-end latency (PC playing → sound leaving the Mac's output):

| Link | Typical |
|---|---|
| Ethernet / USB-Ethernet / Thunderbolt bridge | **~12–18 ms** |
| 5 GHz Wi-Fi, good signal | ~15–30 ms |
| Bluetooth PAN (auto-switches to Opus) | ~50 ms+ |

Where the time goes on a cable: Windows' audio engine hands loopback audio over in roughly 10 ms chunks (the biggest piece, and outside lagless's control). The rest is about 1 ms for the frame, under 0.5 ms on the network, 2–5 ms in the jitter buffer, and about 1.3 ms in the Mac's device buffer. For comparison, lip-sync errors become noticeable at about 45 ms, so wired and good Wi-Fi are both well under that.

## Getting the best result

1. **Use a cable if you can.** Wired is the most stable link and has the lowest latency.
   - **"USB":** the most reliable way is a USB-C or USB-A Ethernet adapter plugged into the Mac, with a short Ethernet cable straight to the PC's Ethernet port. No router or IP setup is needed: both sides give themselves 169.254.x.x addresses and lagless finds the link.
   - A Thunderbolt/USB4 cable between two Thunderbolt machines also works if both show a Thunderbolt Bridge network.
2. **Pick your hi-res rate on the PC:** Settings → System → Sound → your output device → Output settings → Format → for example, *24 bit, 96000 Hz*. lagless follows whatever Windows uses.
3. **For IEMs or a DAC/interface on the Mac:** `./lagless receive --device "<part of its name>"`. Run `./lagless devices` to list the names. Leave Mac volume at 100% and use your amp's volume for the cleanest signal.
4. **On Wi-Fi:** use 5 GHz, keep the Mac near the router, and turn off VPNs. lagless marks its packets as "voice" traffic, which Wi-Fi routers prioritise.

## Command line

The same binary works headless when given arguments (on a Mac: `/Applications/Lagless.app/Contents/MacOS/lagless`):

```
lagless send     [--to 192.168.1.20] [--name studio] [--codec auto|pcm24|pcm16|opus]
                 [--frame-ms 1.0] [--fec 1] [--bitrate 320] [--source loopback|input|tone] [--device NAME]
lagless receive  [--device NAME] [--min-latency 2] [--max-latency 120] [--buffer 64] [--name NAME]
lagless devices
lagless update
```
- `--to`: skip auto-discovery. Use this if your Wi-Fi blocks devices from seeing each other (common on guest or work networks).
- `--source tone`: sends a beeping test tone, for checking the link without playing anything.
- `--fec 2`: more redundancy for flaky Wi-Fi. It costs bandwidth, not latency.
- `--min-latency 0 --buffer 32`: the most aggressive settings for wired links. If you hear crackles, go back up.

## Building from source

Needs [Rust](https://rustup.rs) and CMake (used to build the bundled Opus library).
- **macOS:** `brew install cmake && ./packaging/macos/package.sh` builds `dist/Lagless.app`, `Lagless.dmg` and the update zip.
- **Windows:** `cargo build --release` builds `target\release\lagless.exe`.

Set `LAGLESS_UPDATE_REPO=owner/repo` at build time to choose where auto-update looks. The release workflow sets it to this repo.

## How it works

```
PC: WASAPI loopback → 1 ms frames → PCM24 (or Opus) → UDP [frame N + copy of N-1]  ──►  fastest path
Mac: UDP → reorder / repair from copies / conceal → ring buffer → drift-tracking sinc resampler → CoreAudio
          ▲                                                                                   │
          └─────────────── 1 s reports (loss, buffer, latency) + pings for path RTT ◄─────────┘
```
Receivers broadcast a beacon on UDP 47800 on every interface; audio uses UDP 47801.
