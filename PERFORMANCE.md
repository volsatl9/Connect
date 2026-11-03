# Performance notes

Where frame time goes between the host's screen and the viewer's, what has been measured, and
what still limits high-refresh streaming.

## The pipeline

Host: DXGI desktop duplication → a GPU copy → BGRA to NV12 in Direct3D 11's video processor →
Media Foundation hardware encoder reading the texture → Reed-Solomon recovery packets → QUIC
datagrams paced by BBR. Without that path (no video processor, no hardware encoder that reads
textures, or a failure), it falls back to the 0.5.0 path: a CPU-readable staging texture → BGRA
to YUV on the CPU (multi-threaded, straight from the mapped texture) → the hardware encoder from
memory, or OpenH264.

Viewer: datagrams → frames, rebuilt from recovery packets where needed and put back in order →
H.264 decode on the GPU through Vulkan Video (OpenH264 on the CPU where that's unavailable) →
Y, U and V planes uploaded to the GPU → colour conversion in a
shader → screen.

## Version 0.7.0: frames straight to the screen, latency breakdown, pacing

- **Hardware-decoded frames go straight to the screen.** When a graphics card decodes H.264
  through Vulkan, the window now renders on the decoder's own device and draws the decoded NV12
  texture. Before, each picture was copied back to memory (about 5.5 MB a frame at 1440p),
  converted to I420 by the processor, and uploaded again.
- **Every frame carries its host timings**: from the image reaching the host's screen (DXGI's
  present time) to capture, the capture itself, encoding, and waiting to send. The viewer adds
  network, reassembly, decode and draw, so the stats show where the latency goes.
- **Smooth frame pacing** (Settings → Viewing) shows frames a steady delay after capture,
  the 90th percentile of recent transit times plus 1 ms, so uneven network and decode times
  don't show as uneven motion. Lowest latency stays the default.
- **The hardware encoder's waits spin for the first 3 ms** instead of sleeping: a sleep can
  overshoot by half a millisecond on Windows, about as long as a whole 1080p encode.

## Version 0.6.0: GPU-only capture to encoder, codec negotiation

- **No more round trip through memory on the host.** Through 0.5.0 each frame was copied from
  the GPU, mapped, converted to NV12 by the processor (on several threads),
  then copied into a Media Foundation buffer for the GPU encoder to upload again. Now the
  video processor converts on the GPU and the encoder reads the texture it wrote. The processor
  stays out of the per-frame work entirely, and nothing waits on a GPU-to-CPU copy, which
  stalled behind a busy game's rendering.
- **Codecs are negotiated.** Each frame carries its codec, and the PCs choose by hardware on
  both ends first, then efficiency. Only H.264 has implementations so far; see
  [CODECS.md](CODECS.md).

## Version 0.5.0: GPU decoding and Opus audio

- **Decoding moved to the GPU.** The viewer opens the graphics card's Vulkan Video H.264 decoder
  (preferring a dedicated card over integrated graphics) while the host user is still deciding,
  so it's ready when video arrives. Each frame is finished as soon as its data is in, instead of
  when the next frame's data starts, since the host never reorders frames. A keyframe the GPU can't
  decode switches the session to OpenH264 on the spot, starting with that keyframe.
- **Desktop audio is Opus**, 128 kb/s instead of about 3.1 Mb/s of float PCM: 5 ms packets in
  restricted low-delay mode (2.5 ms of lookahead). Lost packets of up to 20 ms are concealed by
  the decoder rather than played as silence.

## Version 0.3.1: stutter fixes

Measured causes of stutter in 0.3.0, and what changed:

- **A lost packet froze the picture.** Video datagrams are never retransmitted, and any missing
  packet lost its whole frame; the viewer then held every frame until the next keyframe, about
  one round trip plus a large keyframe later. At high frame rates there are thousands of packets
  a second, so even 0.1% loss meant several freezes a second. Each frame now carries
  Reed-Solomon recovery packets (8% of the frame, at least one; more on lossy paths, and half as
  many again for keyframes), and any lost packets up to that number are rebuilt on arrival.
- **Reordered packets counted as loss.** A frame completing before an earlier one made the
  earlier one count as lost, with the same freeze. Complete frames now wait up to 12 ms (or four
  frames) for an earlier one.
- **The viewer couldn't keep up at 1440p.** Per frame at 2560×1440 on the development machine:
  decode 5.8 ms, a fresh 15 MB RGBA buffer 6.4 ms, YUV to RGBA 1.1 ms, and egui's RGBA to
  `Color32` conversion on the GUI thread 9.0 ms. That is more than a 144 Hz frame (6.9 ms), so the
  four-frame decode queue filled and video skipped to a keyframe. Now frame buffers are reused,
  colour conversion runs in a shader (through wgpu), the upload is a third of the size, and a
  decoder that falls briefly behind decodes every frame but only shows the newest.
- **The software encoder dropped frames in motion.** With rate-control frame skipping on,
  OpenH264 skipped 56 of 60 frames of busy 1440p content at 35 Mb/s. Skipping is off; when the
  network can't take more, the host holds frames back at capture instead.
- **Datagrams could queue for seconds.** Nothing matched the video to the connection, and the
  send buffer held 4 MB (1.6 s at 20 Mb/s). The host now measures the queue and loss every
  250 ms, lowers the bitrate as soon as packets queue up and raises it back towards the
  configured cap once the path keeps up (both encoders change bitrate mid-stream). The buffer is
  2 MB, and BBR replaces CUBIC, which cut its rate at every stray lost packet.
- **The hardware path copied every frame three times.** Capture copied each row out of the
  staging texture, `read_bgra8` converted to I420 on one thread, and NV12 was packed byte by byte
  into a new allocation. Now one multi-threaded pass converts the mapped texture straight to
  NV12, and the encoder's wait for output polls every 0.2 ms instead of every 1 ms. The encoder is
  told the monitor's real refresh rate rather than 240 Hz, so per-frame bit budgets are right.

The same 720p, 120 fps test source over a relay that drops packets and holds back one in fifty
by 2 ms (`cargo test -p connect-host --release --test lossy_path -- --ignored --nocapture`),
8 seconds, at most 960 frames:

| Packet loss | 0.3.0 frames shown | 0.3.0 frames lost | 0.3.1 frames shown | 0.3.1 frames lost |
| ---: | ---: | ---: | ---: | ---: |
| 0% (reordering only) | 580 | 0 | 955 | 0 |
| 1% | 788 | 7 | 958 | 0 |
| 3% | 624 | 43 | 951 | 2 |

Windows "N" editions ship without Media Foundation; 0.3.0 linked to it, so Connect wouldn't
start there. It is now loaded when first needed, and hosting falls back to OpenH264 without it.

## Version 0.9.1: frame delivery

The 0.9.0 test on the RTX 5060 host reported 44 frames lost and 34 skipped. The 0.8.2 run before
it reported none. The host trace and a local reproduction show three things.

- **The count was inflated.** When a frame was lost, the frames skipped while the viewer waited
  for the next keyframe were counted again as lost when it came. One lost frame read as about
  five. The 44 lost came from about six loss events, roughly one lost frame each. Frames turned
  away by a full decoder queue weren't counted at all. Both are fixed, with tests. The 0.8 runs
  were inflated the same way.
- **The losses were burst losses from overrunning the path.** Midway through that session the
  path's capacity fell from about 80 to about 40 Mb/s. The rate controller climbed back towards
  80 Mb/s 24 times. Each climb filled the send queue (up to 483 KB), and most loss events came
  within a second or two of one.
  - A full queue at the bottleneck drops packets in runs, so the recovery data can't rebuild the
    frames, and nothing got repaired. Losses don't have to be frequent: in a local test, 0.1% of
    packets dropped this way cost 15 frames.
  - In the 0.8.1 run, the path carried the full 75 Mb/s, so nothing was lost.
- **Viewer-side causes found nothing.** Nothing in the trace points to the viewer, the reorder
  wait, the decoder, or the transport's buffers. The send buffer is 2 MB, the receive buffer
  8 MB, and only the selected path carries data. The new per-frame loss records will confirm
  this on the next hardware run.

What changed:

- **The rate controller remembers the path's ceiling.**
  - When the path proves too slow (backlog, heavy loss, or the viewer losing frames), the
    controller sets a ceiling at 80% of BBR's bandwidth estimate, leaving room for recovery data
    and probing.
  - It climbs quickly to 90% of the ceiling, then creeps at about 1% a second. It forgets the
    ceiling after 20 s without congestion.
  - In a model of the 0.9.0 session, bitrate cuts went from 41 to 9, and data sent beyond the
    path's capacity from 9.0 to 2.4 MB, for 97% of the throughput.
  - `CONNECT_RATE_CEILING=0` restores 0.9.0's controller, to compare.
- **A viewer keyframe request counts as congestion.** A lost frame means the path overflowed.
- **The send hold is time-based.** The host stops encoding new frames while more than 30 ms of
  video is queued, with a floor of 64 KB. It used to be 50 ms with a 128 KB floor, which on slow
  paths allowed 100 ms or more.
  - On a 10 Mb/s test path, latency fell from about 160 to about 105 ms. The CPU encoder there
    can't go below the path's rate.
- **New traces:**
  - `viewer-*-loss.csv`: why each frame was lost or skipped, and whether its datagrams came late.
  - `viewer-*-network.csv` and `host-*-network.csv`: each side's view of the path over time,
    including BBR's estimate and the controller's reasons.
- **Intra refresh** uses the range the driver reports. Where it reports none, it tries shorter
  waves, both before and after the stream format is set. Unaccepted, keyframes come from a
  30 s safety interval and the viewer's requests.
- **`gpu-check` reports, for each encoder:**
  - what it offers of the intra-refresh, buffer, QP, GOP, LTR and slice settings: supported or
    not, the default, the range or allowed values, and the values it actually takes;
  - a frame-size test on an identical synthetic busy picture, with five setups: average, median,
    p95 and largest frame, keyframe sizes, bitrate and PSNR.
- **Overlay:**
  - Stages under 1 ms show in µs (GPU convert was showing 0.0 for 27 µs).
  - Frame sizes are the actual received p50/p95/p99/max.
  - The encoder's label calls its buffer a setting, not a limit.
- **Test fix:** the lossy-path and bottleneck tests now force every packet through their relay.
  Before, the endpoints found a direct path around it once connected.

Known limit: on busy content, the CPU encoder (OpenH264, no frame skipping) can't go below about
12 Mb/s at 720p60, whatever its target. A host without hardware encoding on a slower path still
overruns it.

## Version 0.9.0: even frames

The 0.8 traces from the RTX 5060 host showed the host itself steady: capture to send took 2.6 ms
(p50) and 4.5 ms (p99). The uneven frames came from bursts:

- **A full keyframe every 600 frames.** That was every 2.5–5 s at 120–240 fps: 47 of the 50
  keyframes in a 256 s session. Each was up to 690 KB, taking tens of milliseconds to arrive
  (reassembly p99 14 ms, max 70 ms).
- **No cap on frame size,** so any keyframe or scene cut could be many times the average frame.

What changed:

- **Intra refresh** (`CODECAPI_AVEncVideoGradualIntraRefresh`), where the encoder takes it. Each
  frame recodes a band of the picture, renewing all of it every half second. Keyframes go out
  only when the viewer asks, after a loss. Encoders without it send one every 30 s instead of
  every 600 frames.
- **Frame-size cap:** the encoder's buffer (`CODECAPI_AVEncCommonBufferSize`) is two frames at
  the starting bitrate, set once per encoder (changing it mid-stream could make a driver restart
  with a keyframe). Keyframes requested after a loss are capped too, so recovery is quick.
- **Shader colour conversion:** BGRA→NV12 is two pixel shaders writing the NV12 texture's planes
  directly. It replaces Direct3D 11's video processor, which remains the fallback.
  - It is bit-identical to the processor path; a test checks this against the CPU conversion.
  - Unlike the video processor on NVIDIA, its GPU time can be measured, so *GPU convert* in the
    stats has real figures.
- **Settings check:** `connect gpu-check` reports both conversions (identical or not, and GPU
  time), what the encoder accepted (intra refresh, cap), and how large frames and keyframes come
  out against the cap.

Environment switches, for comparisons:
- `CONNECT_GPU_CONVERT=video-processor`
- `CONNECT_INTRA_REFRESH=0`
- `CONNECT_FRAME_BUFFER=<frames>` (0 is the driver's default)

## Reading the stats (Ctrl+Alt+S)

Since 0.8.0 the figures come from the performance trace. [METRICS.md](METRICS.md) defines each
one, its clock and its limits.

- **fps**: frames decoded per second on the viewer.
- **Mb/s (target)**: received video rate. The host's current bitrate target follows the
  connection and stays at or below the Settings cap.
- **packet loss**: what the host sees going missing on the path, over the last second or so.
- **Frames lost / repaired / skipped / superseded**, running totals:
  - *lost*: frames that couldn't be rebuilt. Each costs a short freeze.
  - *repaired*: frames rebuilt from recovery packets.
  - *skipped*: complete frames thrown away because an earlier one was lost.
  - *superseded*: frames decoded but replaced by a newer one before the screen showed them.
- **last second: dropped, repeated**:
  - *dropped*: lost + skipped + superseded in the last second.
  - *repeated*: an estimate of the frame times a picture stayed up longer than the stream's
    cadence called for.
- **frame time**: mean ± standard deviation of the gaps between frames drawn, and their 99th
  percentile.
- **Latency to this window (est.)**: from the image appearing on the host's screen to it being
  drawn here. Shown as p50/p95/p99 and the slowest frame of the last second, through an
  estimated clock offset whose uncertainty is shown. The display adds up to one refresh on top.
  Then the p50 of each stage:
  - host: screen→capture, capture, GPU convert, encode, held, queue, packetize, send
  - here: network (est.), reassembly, decode queue, decode, draw
- **Input to host (est.)**: the time an input event takes to reach the host, p50/p95/p99, and how
  long the host takes to inject it.
- **Host / here**: each PC's CPU (share of all cores, and cores in use) and memory (working set
  or RSS).
- **GPU load**: the busiest engine of each kind (graphics, video encoder, video decoder) on
  each PC. Windows reports it for any vendor; on Linux, only AMD's driver does.

Steady growth in *lost* means the path loses more than recovery covers. *Superseded* climbing
fast means the viewer can't display as fast as frames arrive. With `CONNECT_TRACE=0` the
latency line falls back to plain averages.

## Version 0.3.0: GPU encoding

Windows screen sharing selects an actual Media Foundation hardware H.264 encoder when
available: low-latency mode, no B frames, one frame in flight. Hardware errors recover on
OpenH264 with a new keyframe.

Verified on 2026-10-29 with an NVIDIA GeForce RTX 5060: the hardware probe encoded and decoded
640×480, 1920×1080 and 800×600 frames, recreated the encoder on resize, checked a forced keyframe
with a fresh decoder, and checked timestamp preservation. A 240 Hz configuration was accepted;
this is not a sustained 240 fps benchmark. AMD and Intel hardware encoders are unverified.

OpenH264 uses size-limited 16 KiB slices and up to four workers; a single slice limits it to
one worker. On 60 precomputed 1920×1080 frames with NASM, median encode time went from 27.9 ms
to 10.9 ms (p95 29.7 ms to 14.1 ms) for about 4% more bytes.

## Build requirement

OpenH264 silently builds its C++ fallback if NASM is unavailable; install NASM so the codec
includes its assembly. On native Windows builds set `NASM` to `nasm.exe` (and run
`cargo clean -p openh264-sys2 --release` if it was built without). The Linux cross-build of
`connect.exe` uses the system `nasm`.

## Remaining limits

- **The GPU-only host path is untested on real hardware.** `connect gpu-check` measures it
  against the memory path on the PC it runs on.
- **Decoded pictures go through system memory.** Since 0.5.0 the GPU decodes, but each picture
  comes back to system memory as NV12 and is uploaded again for display. Rendering the decoder's
  image directly would save that round trip (about 5.5 MB a frame at 1440p).
- **Hardware decoding is untested on real GPUs here.** The development machine has none; it falls
  back to OpenH264 (about 6 ms a frame at 1440p) and its stats line ends "on CPU".
- Physical two-PC gaming, AMD and Intel hardware encoders, Vulkan Video decoding, and Linux
  audio playback have not
  been measured here.
