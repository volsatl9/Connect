# Performance notes

Where frame time goes between the host's screen and the viewer's, what has been measured, and
what still limits high-refresh streaming.

## The pipeline

Host: DXGI desktop duplication → copy to a CPU-readable staging texture → BGRA to YUV on the
CPU (multi-threaded, straight from the mapped texture) → H.264 (Media Foundation hardware
encoder, or OpenH264 on the CPU) → Reed-Solomon recovery packets → QUIC datagrams paced by BBR.

Viewer: datagrams → frames, rebuilt from recovery packets where needed and put back in order →
OpenH264 decode on the CPU → Y, U and V planes uploaded to the GPU → colour conversion in a
shader → screen.

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
  colour conversion runs in a shader (wgpu or OpenGL), the upload is a third of the size, and a
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

## Reading the stats (Ctrl+Alt+S)

- **fps**: frames decoded per second on the viewer.
- **Mb/s (target)**: received video rate; the host's current bitrate target follows the
  connection and stays at or below the Settings cap.
- **latency**: host capture to decoded here, through an estimated clock offset.
- **frames lost / repaired / skipped**: lost is frames that couldn't be rebuilt (each costs a
  short freeze); repaired is frames rebuilt from recovery packets; skipped is frames decoded
  but replaced by a newer one before the screen showed them.
- **packet loss**: what the host sees going missing on the path, over the last second or so.

Steady growth in *lost* means the path loses more than recovery covers. *Skipped* climbing
fast means the viewer can't display as fast as frames arrive.

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

- **GPU readback.** Capture still maps a staging texture on the CPU, which waits for the GPU to
  finish the copy; under heavy GPU load this adds delay. Converting to NV12 on the GPU and
  handing the encoder a GPU texture would remove the readback entirely.
- **CPU decoding.** The viewer decodes with OpenH264, about 6 ms a frame at 1440p on the
  development machine. Hardware decoding (Media Foundation / DXVA on Windows) is the next step
  for 1440p and 4K above 144 fps.
- **Audio** is uncompressed float PCM, about 3 Mb/s; a codec such as Opus would need a tenth.
- Physical two-PC gaming, AMD and Intel hardware encoders, and Linux audio playback have not
  been measured here.
