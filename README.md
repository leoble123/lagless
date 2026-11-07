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

The window is kept deliberately quiet: nothing moves while you're streaming.

- **The big number** is the live end-to-end latency, refreshed once a second. Under it is one status line, and the dot is green under 25 ms, amber under 45 ms and red above.
- **Volume:** a digital volume control that goes far below your speakers' lowest click. It fades smoothly from full down to about -60 dB and then silent. Click the speaker icon to mute. On the Mac it sets playback volume; on the PC it sets the level of the stream you send. It's remembered between launches.
- **Mic:** one switch to send the Mac's mic to the PC (see below).
- **Details** (collapsed by default): the link, format, round trip, buffer and dropout counts.
- **Settings** (sliders icon): output device (DACs and IEM amps), latency floor, device buffer, capture source, quality and loss protection.

## Use the Mac's mic on the PC

lagless can also send your Mac's microphone (for example the iMac's built-in mic) back to the PC at the same time as the PC's audio plays on the Mac, so you can use it in Discord, Zoom, OBS or games.

1. **On the PC:** install the free **[VB-Audio Virtual Cable](https://vb-audio.com/Cable/)** and restart the PC. Windows apps can only use microphones that the system lists, and this gives you one. The app shows a **Get VB-Cable** button if it's missing.
2. **On the Mac:** turn on **Send this Mac's mic to the PC** and allow microphone access when macOS asks. Pick a different mic in settings if you like.
3. **In your PC app:** choose **CABLE Output** as the microphone.

The mic travels uncompressed with the same 1 ms frames and loss protection as the speaker stream, on its own port (UDP 47802), so the two directions don't affect each other. Keep **CABLE Input** from being your PC's default speaker; lagless refuses to start the mic link if it is, because the mic would loop back to the Mac. To avoid echo in calls, use headphones or IEMs on the Mac, or leave your call app's echo cancellation on.

## Control from other apps

The desktop app runs a small control API so other apps on the same computer (like the Nova launcher) can show its status and drive it. It listens on `127.0.0.1` only, on a random port, and every request needs a secret token. Both are written to `control.json` next to `settings.json` (`%APPDATA%\Lagless` on Windows, `~/Library/Application Support/Lagless` on the Mac) while the app is open, and the file is removed when it quits. Requests that come from a web page (an `Origin` header, or a non-loopback `Host`) are refused.

Send `Authorization: Bearer <token>` with each request:

| Request | Body | Does |
|---|---|---|
| `GET /v1/status` | | Live status: phase, latency, link, format, peer, volume, mute, mic |
| `POST /v1/volume` | `{"volume": 0.0–1.0}` | Same as the volume slider |
| `POST /v1/mute` | `{"muted": true}` | Same as the speaker icon |
| `POST /v1/mic` | `{"on": true}` | "Use Mac mic" on the PC, "Send mic to PC" on the Mac |
| `POST /v1/pause` | `{"paused": true}` | Stop or resume streaming without quitting; the window shows a Resume button |

Commands apply even while the window is minimized.

## Auto-update

Every 6 hours the app checks this repo's latest GitHub Release. If there's a newer one, it downloads it in the background and verifies its SHA-256 checksum against the one published with the release. Then an **Update to x.y.z** button appears. Click it to restart into the new version, or just quit: the update installs on exit, so the next launch is the new version. You can turn this off in settings, and `lagless update` does it from a terminal.

**Shipping a new version:** bump `version` in `Cargo.toml` and push to `main`. GitHub Actions builds the `.dmg` and `.exe`, publishes release `v<version>`, and every installed copy picks it up.

## About "zero latency"

True zero isn't physically possible, but the goal is to get under what you can perceive. Typical end-to-end latency (PC playing → sound leaving the Mac's output):

| Link | Typical |
|---|---|
| Ethernet cable (direct or via a router) | **~7–12 ms** with the Windows engine trick, ~12–18 ms without |
| 5 GHz Wi-Fi, good signal | ~12–30 ms |
| Bluetooth PAN (auto-switches to Opus) | ~50 ms+ |

Where the time goes on a cable: Windows hands loopback audio over once per audio-engine period, which is 10 ms normally and about 2.7 ms with the trick below. The rest is about 1 ms for the frame, under 0.5 ms on the wire, a 2–5 ms jitter buffer, and about 0.7 ms of Mac device buffer plus the Mac's own output latency. Lip-sync errors become noticeable at about 45 ms.

## Latency tricks lagless uses

On by default:
- **Windows audio-engine speed-up.** Windows mixes audio in 10 ms chunks, and loopback capture can't be faster than that. But the engine runs each speaker device at the smallest period any app requests through the low-latency API (`IAudioClient3`, Windows 10+). So lagless quietly plays silence at the minimum period, often 128 samples (2.7 ms) at 48 kHz, which makes the whole engine and the capture 3–4x more frequent. Details shows *Windows engine: 2.7 ms (sped up from 10.0)* when it works. Some third-party drivers don't allow it; nothing changes then.
- **Two links at once.** With a cable and Wi-Fi both up, every packet goes over both, and the first copy to arrive wins. A hiccup on one link costs nothing.
- **Frames that fit the capture.** Frame size is matched to Windows' chunk size, so no leftover partial frame waits for the next chunk.
- **Scheduling.** On Windows: 1 ms timer resolution, high process priority, and the "Pro Audio" thread class for the audio threads. On the Mac: user-interactive priority for the network thread.
- **Small Mac buffer.** A 32-sample device buffer (0.7 ms at 48 kHz).
- **Smarter jitter buffer.** It waits about 300 ms at the start of a stream to measure how bursty arrivals are, instead of guessing and glitching once. After a hiccup it covers that hiccup for 20 s, then walks back down to the floor within seconds.

In settings (sliders icon → *Latency tweaks*):
- **Lowest latency** (Mac): forget network hiccups after 4 s instead of 20 s. You'll get lower latency most of the time, and an occasional glitch if your network has periodic spikes.
- **Stop AirDrop Wi-Fi hopping** (Mac, Wi-Fi only): AirDrop/Continuity makes the Mac's Wi-Fi radio hop channels about once a second, which shows up as regular latency spikes. This turns that radio off (`ifconfig awdl0 down`, admin password needed) until you restart or use AirDrop.
- **Device buffer** (Mac): try 16 for the absolute minimum. Go back up if you hear crackles.

Worth doing by hand on Windows:
- **Turn off audio enhancements** for your output device (Settings → System → Sound → device → *Audio enhancements: Off*). Enhancement plug-ins add processing delay and can stop the engine speed-up from working.
- **Use 48 kHz** as the Windows output format. The low-latency period is most widely supported there.
- **Mic link:** in VB-Cable's control panel (*VBCABLE_ControlPanel*, run as administrator), lower the latency/buffer setting if your version offers it. Its default buffer is generous.

## Using a cable

A cable is the most stable link and has the lowest latency. lagless switches to it automatically when it appears, and keeps Wi-Fi as a backup.

**What you need:** an Ethernet cable (any Cat5e/Cat6; no "crossover" cable needed) and Ethernet ports on both machines.
- **iMac:** Intel 21.5"/27" iMacs have Ethernet on the back. On the 24" Apple Silicon iMac, it's on the power adapter if yours came with that one; otherwise use a USB-C to Ethernet adapter (about $15–20).
- **PC:** the motherboard's Ethernet port. If it's already used for internet, either use a USB Ethernet adapter for the Mac link, or use option A below.
- A plain USB-C-to-USB-C cable between the two **won't** work: it doesn't create a network connection between a PC and a Mac.

**Option A, both into your router (easiest):** plug the PC and the iMac into the router or a switch. Done. Internet keeps working on both.

**Option B, PC straight to iMac (lowest latency, no router hop):**
1. Connect the cable. Leave Wi-Fi on for internet.
2. Wait about 30–60 seconds. Both machines give themselves a `169.254.x.x` address automatically. Windows calls it an "Unidentified network" and the Mac shows "Self-assigned IP" with a yellow dot. Both are normal, so don't change them.
3. On the PC, open lagless settings → **Allow lagless on all networks…** and approve the prompt, once. Windows treats a direct cable as a Public network. Sound to the Mac works without this, but the mic link needs it.
4. Check **Details** on either side: *Link: Cable* and a round trip of about 0.2–0.5 ms.

## Getting the best result

1. **Use a cable if you can** (see above).
2. **Pick your hi-res rate on the PC:** Settings → System → Sound → your output device → Output settings → Format → for example, *24 bit, 96000 Hz*. lagless follows whatever Windows uses. 48 kHz gives the lowest latency (see above).
3. **For IEMs or a DAC/interface on the Mac:** choose it under Output in settings. Leave Mac volume at 100% and use lagless's volume slider or your amp for level.
4. **On Wi-Fi:** use 5 GHz, keep the Mac near the router, turn off VPNs, and try *Stop AirDrop Wi-Fi hopping*. lagless marks its packets as "voice" traffic, which Wi-Fi routers prioritise.

## Command line

The same binary works headless when given arguments (on a Mac: `/Applications/Lagless.app/Contents/MacOS/lagless`):

```
lagless send     [--to 192.168.1.20] [--port 47801|47802] [--name studio] [--codec auto|pcm24|pcm16|opus]
                 [--frame-ms 1.0] [--fec 1] [--bitrate 320] [--source loopback|input|tone] [--device NAME]
                 [--no-multipath] [--no-engine-boost]
lagless receive  [--device NAME] [--min-latency 2] [--max-latency 120] [--buffer 32] [--lowest] [--name NAME]
lagless devices
lagless update
```
- `--to`: skip auto-discovery. Use this if your Wi-Fi blocks devices from seeing each other (common on guest or work networks).
- `--source tone`: sends a beeping test tone, for checking the link without playing anything.
- `--fec 2`: more redundancy for flaky Wi-Fi. It costs bandwidth, not latency.
- `--min-latency 0 --buffer 32`: the most aggressive settings for wired links. If you hear crackles, go back up.

## Building from source

Needs [Rust](https://rustup.rs) and CMake (used to build the bundled Opus library). With CMake 4+, set `CMAKE_POLICY_VERSION_MINIMUM=3.5` (the macOS script does this for you).
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
