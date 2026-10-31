# Connect

Version 0.7.0 draws hardware-decoded video straight from the graphics card, shows where every
millisecond of latency goes and how busy each graphics card is (Ctrl+Alt+S), and adds a
**Smooth** frame pacing option. Video uses H.265 in hardware when both PCs' graphics cards handle
it (sharper at the same bandwidth), and H.264 otherwise. Since 0.6.0 the host keeps the screen on its graphics card from capture to
encoder, and the two PCs agree on a codec from what each one's hardware can do. Its protocol is
7, so both PCs need 0.7.0; older peers receive a version mismatch message. See [CODECS.md](CODECS.md) for how codecs are chosen and the plans for
H.265 and AV1. See [performance notes](PERFORMANCE.md) for measurements
and remaining stutter risks.

Peer-to-peer remote desktop for gaming, built on [iroh](https://iroh.computer). Click **Generate
Account** to get a User-ID. Give it to someone and they can connect to your PC from anywhere:
direct P2P when possible, through a relay when not. You allow or deny every connection, and choose
whether they may use your mouse, keyboard and gamepad, or hear desktop audio. You can switch
each permission on or off at any time.

## Status

| | |
|---|---|
| Works | Generate Account, connect by User-ID across networks, Allow / Deny with per-device permissions, "Always allow" list, desktop audio (Opus) with viewer mute, automatic hardware H.264 and H.265 encoding and decoding with CPU fallback, a GPU-only capture → encode path on the host, codec negotiation, host cursor in the viewer, a distinct viewer pointer on the host, remote mouse (normal or captured for games), keyboard by scancode, up to 4 gamepads as virtual Xbox controllers |
| Not yet | Monitor selection, microphone sharing, AV1 (planned, see [CODECS.md](CODECS.md)) |
| Platforms | Windows can host and connect. Linux can connect only |

## Download

Grab the latest build from [Releases](../../releases):

- `connect.exe`: Windows 10 / 11. It's unsigned, so SmartScreen asks first: **More info → Run anyway**.
  Allow it through the firewall so direct connections work.
- `connect-linux-x86_64`: Linux with glibc 2.39+ (Ubuntu 24.04, Fedora 40, Debian 13 or newer).

Desktop audio playback on Linux is not yet verified.

Connect updates itself: when a new release is out it shows **Update available**, and **Update now**
downloads it, checks it against the release's checksums, and restarts into it. Settings → Updates
checks on demand.

## Using it

The **Connect** page shows this PC and every computer you've connected to as cards:

- **This PC** (Windows) has the **Sharing** switch, which lets people ask to connect. While
  someone is connected it says so, and **Manage** changes what they may use or disconnects them.
- Each saved computer has its owner's name for it and a **Connect** button. A computer is saved
  the first time it lets you in, and its name follows the owner's if they rename it. Hover a card
  and click **✕** to remove it.
- **+ Connect** asks for a User-ID, for a computer you haven't connected to before.

**Settings** has your User-ID, hidden until you click **Show** so it doesn't end up on stream or in
a screenshot (**Copy** works either way), this PC's name, what people may use by default, the
devices that connect without asking, stream quality and updates. The arrow at the top of the
sidebar collapses it to icons; click the logo to open it again.

Gamepads work on the host through the ViGEmBus driver, which is built into `connect.exe`. The
welcome screen offers to set it up, and so does Settings → Sharing. Windows asks for permission once.
The remote user's controller shows up as an Xbox 360 controller in the next free slot: player 2
if the host has one plugged in, player 1 if not. The driver stays installed if Connect is deleted;
remove it under Windows *Installed apps* ("ViGEm Bus Driver").

The host's own mouse and keyboard always win: while the host is using one, the remote user's
input on it pauses (and anything they held is released) until a second after the host stops.

Windows won't let a normal program control one running as administrator, so to control such
programs, run Connect as administrator on the host.

## While connected

The glass shelf on the left contains fullscreen, mouse capture, audio mute, stats and
disconnect controls. Collapse it with the arrow; shortcuts remain available while the mouse
is captured. The shelf's clicks stay local.

Windows hosts keep each captured frame on the graphics card: Direct3D 11's video processor
converts it to NV12 there, and a Media Foundation hardware H.264 encoder reads it straight from
the GPU. Where that isn't available, frames go through memory to the hardware encoder, and to
parallel OpenH264 if initialization or encoding fails (or if Media Foundation is missing, as on
Windows "N" editions). The two PCs pick the codec together, preferring one both graphics cards
handle. The viewer decodes on its graphics card through Vulkan Video where the
driver supports H.264 decoding (current NVIDIA, AMD and Intel drivers on Windows, and NVIDIA's
driver or a recent Mesa on Linux), and on the processor with OpenH264 otherwise, or if Settings → Viewing →
**Hardware video decoding** is off. The shelf shows the actual encoder and decoder, and the stats
say whether decoding runs on the GPU or CPU. The stream follows the host
monitor's refresh rate, up to 240 Hz, subject to capture, encoder, network, decoder and display
throughput.

Video is built to keep moving on real internet connections:

- Every frame carries a few recovery packets, so a lost packet or two (or a short burst) is
  rebuilt on arrival instead of freezing the picture until a new keyframe. More are sent when
  the connection is losing more.
- Packets arriving out of order are put back in order rather than counted as lost.
- The host lowers the bitrate the moment packets start queueing up, and raises it back towards
  your setting once the connection keeps up; it never builds up seconds of delay.
- The viewer decodes every frame but only displays the newest, and converts it to colour on the
  graphics card, so a busy moment doesn't make it fall behind.

Desktop audio is the Windows engine's 48 kHz stereo mix, compressed with Opus in 5 ms packets
at 128 kb/s (uncompressed PCM was about 3.1 Mb/s). Opus's low-delay mode adds 2.5 ms, and the
viewer fills a short run of lost packets (up to 20 ms) with Opus's concealment, not silence. If the
output device can't give a 48 kHz stereo mix, audio is sent in its own format: Opus where that
format allows, otherwise PCM. Windows may downmix or resample the output device. No microphone is recorded. The host must allow **Desktop audio**;
existing saved permission sets leave it off until enabled. Missing audio devices leave video
and input working. The viewer can mute playback independently.

Cursor updates travel independently of video. The host sees a cyan viewer pointer that cannot
take focus or intercept clicks, including view-only sessions. It hides during relative mouse
capture and after updates stop. Windows may cover the overlay in exclusive fullscreen; animated
cursor shapes currently use their first frame. If Windows cannot exclude the viewer overlay
from capture, the overlay is hidden to prevent duplicate pointers.

| Keys | |
|---|---|
| Ctrl+Alt+M | Capture the mouse for games (raw movement, hidden pointer), and release it again |
| Ctrl+Alt+F | Full screen |
| Ctrl+Alt+S | Stats: connection path, round trip, fps, bitrate (and the host's current target), frames lost / repaired / skipped, packet loss; latency stage by stage, from the host's screen to this window; how busy each PC's graphics card is |

All other keys, including F-keys and Esc, go to the host.

## Build

Windows builds are cross-compiled from Linux (Ubuntu 24.04):

```sh
sudo apt install gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 nasm
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p connect   # Windows
cargo build --release -p connect                                    # Linux
cargo test --workspace
```

Native Linux builds also need the ALSA and udev development packages (`libasound2-dev`,
`libudev-dev` and `pkg-config` on Ubuntu) for audio playback and gamepad input.

NASM is needed at build time for OpenH264's assembly optimizations. Without it, the dependency
silently falls back to slower C++ code. On native Windows builds, use the Visual Studio C++
toolchain and put `nasm.exe` on PATH, or set the `NASM` environment variable to its full path.
If OpenH264 was already built without NASM, run `cargo clean -p openh264-sys2 --release`
before rebuilding the release executable (omit `--release` for a dev/test build).

The software fallback uses multiple slices and up to four workers to reduce frame processing
stalls while keeping CPU use bounded. Input is forwarded before GUI image upload, and queued
gamepad transitions are preserved. Network and display latency still apply. See
[performance notes](PERFORMANCE.md) for verification and remaining limits.

## Headless commands

For testing without a display:

```text
connect host [--test-pattern[=WxH@FPS]] [--print-input]
                                    share this PC (Windows); Allow/Deny is asked on the terminal
connect join <USER-ID> [--seconds N]
connect id                          print this PC's User-ID
connect gpu-check                   check the graphics hardware for sharing and viewing,
                                    and save the report to connect-gpu-check.txt
```

`--test-pattern` shares a generated image (1280×720 at 60 fps, or e.g. `--test-pattern=1920x1080@144`)
and `--print-input` prints the remote input instead of acting on it, both for development. On Windows the app has no console window, so start these as
`start /wait connect.exe id` for the terminal to wait for them. `CONNECT_CONFIG_DIR` selects a separate identity, e.g. to run two
copies on one PC. `CONNECT_GPU_PIPELINE=0` makes a host send frames through memory instead of
keeping them on the graphics card, for comparison.

## Third-party components

`connect.exe` includes the installer for [ViGEmBus](https://github.com/nefarius/ViGEmBus) 1.22.0
(BSD-3-Clause, © Nefarius Software Solutions e.U.); see `crates/host/vendor/vigembus/`.

The app's text is set in [Inter](https://rsms.me/inter/) (SIL Open Font License 1.1) and its icons
are a subset of [Phosphor](https://phosphoricons.com/) (MIT); both are in `app/assets/fonts/` with
their licenses.

Audio is compressed with [libopus](https://opus-codec.org/) (BSD-3-Clause). Hardware decoding uses
[gpu-video](https://github.com/software-mansion/smelter) (MIT) and AMD's
[Vulkan Memory Allocator](https://github.com/GPUOpen-LibrariesAndSDKs/VulkanMemoryAllocator) (MIT).
