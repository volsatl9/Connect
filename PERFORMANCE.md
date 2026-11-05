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

## Version 0.9.2: quiet scenes

The first long-ish session on real hardware (5 minutes, RTX 5060 to RTX 3070 Ti, both on Ethernet,
43 ms apart) changed what 0.9.1 said about lost frames. 0.9.1 blamed overrunning the path. That
session's three bursts of lost packets (10, 7 and 35, of 761,000) came with no queue, at about a
third of the path's rate or less, so sending less wouldn't have prevented them. What did go wrong
was how Connect took quiet scenes.

- **BBR forgot the path.** With no loss at all, its estimate fell from 76 to 1.6 Mb/s and its window
  from 883 to 30 KB as a game went quiet, and the next large frame took up to 247 ms to send. BBR
  is now wrapped by a controller that remembers the best rate for two minutes. When BBR's estimate
  falls under half of that, pacing and the window hold to the remembered rate
  (`crates/net/src/congestion.rs`). It keeps out of the way otherwise: BBR shrinks its window on
  purpose to measure the round trip, and can't finish, or ever probe for more, unless what's in
  flight falls to match. An early version that always held the window up froze BBR that way. Loss,
  or a round trip 50% over the best, halves what's remembered. `CONNECT_PATH_MEMORY=0` turns it off.
  Replaying 80 s of that session's frames over an 80 Mb/s path:

  | To the last datagram | Plain BBR | With memory |
  |---|---|---|
  | p99 | 647 ms | 27 ms |
  | Worst | 756 ms | 65 ms |
  | Frames over 60 ms | 463 | 5 |

- **Queue pressure is judged by what lasts.** All 16 backlog cuts in that session came from one
  tick of queue, or a frame's worth at a low bitrate. The bitrate spent 151 of 308 seconds under
  25 Mb/s and 38 under 5, on a 76 Mb/s path. A backlog now has to last two ticks and be over 32 KB.
- **Stray loss keeps the bitrate.** One burst while the video uses under 80% of the path no longer
  costs anything. Heavy loss (20% of a tick) or loss lasting two ticks still counts, as the estimate
  lags when a path really slows down.
- **The bitrate comes back.** A ceiling now rises when the path measures over 25% faster than it
  did when the ceiling was set, as well as being forgotten after 20 calm seconds.

### The trade-off: sudden drops, skipped frames and picture quality

Holding the bitrate through bursts is a bet that a burst is a burst. It's right for what real
sessions have shown so far, and it means better pictures: the bitrate no longer falls with the
scene, or stays down after a stray burst. It also means reacting about a tick (250 ms) later when
a path really does collapse, where the old controller cut at once.

Measured with a CPU-encoded stream over a 40 Mb/s path falling to 10 at once, then back:

| | Old (0.9.0's controller, plain BBR) | 0.9.2 (three runs) |
|---|---|---|
| Frames lost at the drop | 18–19 | 19–20 |
| Frames skipped waiting for a keyframe | 8 | 7–16 |
| Worst one-second latency | 233–254 ms | 214–295 ms |
| Latency back to normal after the path recovers | within 3 s | in 3–13 s |
| Bitrate target 32 s after the path recovers | still 2 Mb/s | the full cap (2 of 3 runs) |

So a sudden collapse costs a few more skipped frames, and a freeze of a quarter of a second either
way. In return the picture isn't throttled for no reason, and recovers when the path does. In that
third run the target was still near 3 Mb/s 32 s later: the CPU encoder doesn't follow low targets
(it sent 15 Mb/s against a target of 3), so the queue kept looking like a backlog. Hardware encoders
follow their target; in a model of that, the bitrate is back at 80% of the cap 19 s after the path.
That is a model. It hasn't been seen on a real path yet.

### Still open

- **Bursts of lost packets.** Recovery data can't repair 10 packets in a row, which take a whole
  frame. The frames after it arrive but can't be decoded, so the picture freezes until a keyframe
  comes back: 82 ms in that session, a round trip and a bit. They happened on Ethernet, with no
  queue on the host's side, and their cause is unknown. The host's and viewer's traces show when,
  and how many packets, but not where on the path. 0.9.2 changes nothing about this.
- **Host capture and encoder stalls.** 12 of the 13 encoder calls over 20 ms (up to 393 ms) came in
  two moments, at 169–170 s and 178–180 s, during a stretch of gaps between captures of up to
  half a second. The second lined up with a 113 ms round trip and 7 lost packets. That points at
  something on the host stalling everything, not at the encoder alone, but it isn't established.
  The bitrate was near its floor at the time, which 0.9.2 should stop, so the link can't be ruled
  out. 0.9.2 traces tell a stalled capture from a game with nothing new to show (METRICS.md).
- **Repeating network queue events.** Explained: see 0.9.3 and 0.9.4 below.
- **Host priority.** That session dropped 829 remote events to host priority, in runs around 180 s
  and 280–300 s, with no explanation of what set it off. It's not established that anyone was
  using the host's mouse. 0.9.2 records what the hooks saw (METRICS.md).

## Version 0.9.3: BBR's round-trip probe

Every 5–13 s under load, latency jumped about 80 ms for a moment, with no loss and a flat round
trip. BBR was measuring the path's round trip: for 200 ms it holds half the bandwidth-delay product
in flight. Video at 80 Mb/s needs about 440 KB in flight on a 45 ms path, and got about 240 KB, so
sending halved and 200–345 KB queued on the host. Every spike in a 20-minute session fits, and it
was what the p99 input delay of 121 ms was made of (71 ms without them).

BBR now keeps 90% of the bandwidth-delay product during the probe. That stays under the path's
capacity, so nothing stands in a queue and the measurement still holds. The gain has no setter in
noq-proto, so `vendor/noq-proto` adds one; nothing else in that copy is changed.
`CONNECT_PROBE_RTT_GAIN=0.5` goes back to BBR's own.

Replaying 85 s of that session's frames (80 Mb/s video) over a 100 Mb/s path, 40 ms round trip:

| To the last datagram | Gain 0.5 | 0.75 | 0.9 |
|---|---|---|---|
| p99 | 100.5 ms | 33.4 ms | 25.7 ms |
| Worst | 121.6 ms | 42.2 ms | 29.0 ms |
| Frames over 60 ms | 782 | 0 | 0 |

On a 90 Mb/s path, with video almost filling it, p99 goes from 111.8 to 26.7 ms and frames over
60 ms from 862 to 0. Scaled down to 35 or 50 Mb/s, the video fits inside the probe's window and has
no spikes at either gain, which confirms the cause. Drop and recovery is unchanged or better: the
full cap came back in 3 of 3 runs. Not tested: real hardware, and sharing a path with other traffic,
where a shallower probe drains a queue others built more slowly.

### The median

What's left of about 60 ms from input to drawn, on a 42 ms round trip, doesn't move much:

- The encoder already runs in low-latency mode with no B-frames, at 2.5 ms.
- Reassembly (3.2 ms) is a 41 KB frame crossing a path of about 100 Mb/s. Pacing can't beat the
  bottleneck; only smaller frames can.
- Held pictures take 0.06 ms at the median, and waiting for the next capture about 2.5 ms, which
  follows the game's frame rate.
- Drawing waits for the display (0–2.75 ms) on a surface already set to one queued frame.

### Encoder locked to 60 Hz

For 7 minutes of a 20-minute session (from 510 s to 943 s), every other encode took about 11.6 ms
instead of 2.5, with frames in step at exactly 16.67 ms. Encoding runs on the capture thread, so
capture kept up with only about 130 of the game's 230 images a second, and DXGI counted the rest as
missed. It began and ended within seconds, with no local input at either edge, no change in the
game's GPU load, and Connect's CPU use going down. Neither PC has a 60 Hz monitor, so it isn't a
display's refresh. Something else keeping 60 Hz time on the host looks responsible; what isn't
known. It adds 3–5 ms to the median, and halves the frame rate. 0.9.3 doesn't change this.

## Version 0.9.4: staying sharp through loss

A 19-minute session of 0.9.3 (80 Mb/s over a 44 ms path, both PCs on Ethernet) gave input delay
p50 55.5, p95 76.7, p99 95.0 ms. That's better than 0.9.2's 59.7, 78.0 and 121.4, but short of the
71 ms predicted, and the 60 Hz stall that slowed 0.9.2's session didn't happen this time. Three
things held it back.

**The probe still queued.** Spikes fell from about 80 to about 38 ms over usual, still every
5–14 s. The bandwidth-delay product uses the lowest round trip seen, about 37 ms, on a path that
usually takes 44. So the 90% window of 0.9.3 was about 410 KB, which carries 75 Mb/s, and the video
with its recovery data needs about 90. The replay in 0.9.3 had a steady delay, so it missed this.
The gain is now 1.25: the window stays clear of the video, and BBR keeps its schedule. It no longer
empties a queue during the probe, so a queue standing on the path can raise its lowest round trip.

With the delay wandering by 7 ms, 85 s of that session over a 90 Mb/s path:

| To the last datagram | Gain 0.9 | 1.25 |
|---|---|---|
| p99 | 38.5 ms | 30.4 ms |
| Worst | 53.1 ms | 33.5 ms |
| Lowest estimate | 86.4 Mb/s | 93.9 Mb/s |

Over a 100 Mb/s path, replay showed no spikes at either gain, so it still doesn't reproduce the
session fully. Only a real session can confirm this one.

**Stray loss took BBR down.** For 16 s the path lost 2–3% of packets whatever was sent, from
90 Mb/s down to 7, and the round trip stepped from 44 to 52 ms and back. Nothing in the traces says
why, and the user saw nothing happening. Connect's own loss limit (10%) never fired. BBR did: it
backs off a step each round trip with any loss, so its estimate fell from 101 to 8 Mb/s, video
backed up, and the bitrate followed it to 3.8 Mb/s. The same loss halved the path memory away.

Loss now counts as stray while it's at most 5% of the last half second and the round trip is under
1.5 times its best. BBR is told such loss was spurious, and the memory stays. The bitrate takes one
15% step down when the viewer loses frames; if the loss carries on as before, it holds there
(`loss_held` in the trace) and the recovery data does the rest.

2.5% lost for 16 s, 85 s of the same session over a 100 Mb/s path, no recovery data in this test:

| To the last datagram | Plain BBR | 0.9.4 |
|---|---|---|
| Lowest estimate | 4.0 Mb/s | 103.1 Mb/s |
| p99 | 247.7 ms | 29.8 ms |
| Worst | 281.2 ms | 31.3 ms |
| Frames over 60 ms | 3,811 | 0 |

Real video through the test path can't show the whole loop. The CPU encoder there makes about
10 Mb/s, where plain BBR doesn't collapse either. Results matched plain BBR's at that rate: no
harm, but no proof. The bitrate's side is covered by unit tests, and 80 Mb/s needs a real session.

**The trade-off.** When Connect can't tell a bad moment from another device using the connection,
it stays sharp. It gives way once loss passes 5% or the round trip passes 1.5 times its best, not
at the first lost packets. So it gives other traffic room later than BBR alone would. This hasn't
been tested alongside other traffic.

**Recovery stalled.** Full bitrate came back about 105 s after the loss ended. Two causes:

- `ceiling * 0.9` was rounded down to a whole bitrate and then compared with the unrounded value,
  so a bitrate climbing from below could never count as having reached it, and creeping never
  started. 279 of the 360 quarter-seconds from 780 to 870 s sat at exactly 90% of the ceiling.
- BBR's estimate can't see far past what's sent, about 1.6 times the bitrate here. Held at 90% of
  a ceiling worth 80% of the estimate, the bitrate lifted the estimate only 1.15 times, and the
  ceiling needed 1.25. Each waited for the other until the ceiling was forgotten. After 3 s of
  calm, the ceiling now follows the estimate.

Creeping now works, so on a path that really slowed down, the bitrate finds the limit a little
more often: in the model, 4.6 MB sent over the path where 4.1 went before, for 41.4 Mb/s delivered
over 39.8. In the test path's drop, the target was back to 80% of the cap 11–13 s after the path, 3
runs of 3, with 6–11 frames lost and 19–28 skipped.

## Version 0.10.2: fast paths and quiet scenes

The first session from a Linux viewer, on Wi-Fi 7 with a 1.6 ms best round trip, stalled for 1.25 s
and then 0.5 s with 300 KB queued, sending 4-9 packets per 250 ms. BBR's estimate had fallen to
0.06 Mb/s in a loading screen, the same collapse 0.9.2 fixed. Path memory didn't step in: it read
any round trip over 1.5 times the best as a queue and halved the remembered rate each round trip,
and jitter put 93% of that session's readings over the line. Stray loss protection used the same test.

Inflated now also means at least 15 ms over the best, and the remembered rate halves at most every
50 ms. Separately, a backlog with nothing lost, within 2 s of quiet video, cuts one step and doesn't
take the estimate as the path's limit: that's what took the stream from 80 to 2 Mb/s for 20 s.

Replaying the session's first 70 s, 150 Mb/s path, 2 ms of jitter:

| | 1 ms each way, before | after | 22 ms each way, before | after |
|---|---|---|---|---|
| Last datagram of a frame, p99 | 27.6 ms | 9.5 ms | 250.8 ms | 36.6 ms |
| p99.9 | 62.5 ms | 16.7 ms | 279.9 ms | 44.3 ms |
| Slowest | 75.5 ms | 33.6 ms | 285.0 ms | 67.1 ms |
| Frames over 60 ms | 6 | 0 | 309 | 1 |
| Frames incomplete | 0 | 0 | 24 | 0 |

The cost shows on a path that really slows down (40 to 10 Mb/s for 8 s): 26 frames lost against 20,
worst window 166 ms against 154, and the target starts climbing about a second later.

Replay doesn't reproduce Wi-Fi's own stalls, and the frame rate limit changes nothing here. Both
need a real session.

## Version 0.10.3: when Wi-Fi gives out

The first 0.10.2 session had the Wi-Fi link at its worst yet: 40-50 Mb/s at a round trip of 8-90 ms,
against 75 Mb/s at 3.2 ms the session before, two dropouts and 2,923 packets lost. At 3-6 Mb/s the
round trip was back to 3 ms, so the queue was in the link, which the sender can't see.

- A round trip 1.5 times past its best and at least 15 ms over it, for two ticks, now cuts the
  bitrate from what's actually being sent, by 15%, or 30% at 50 ms over it. It was never a signal
  before: the target sat at 80 Mb/s for a minute of 30-200 ms round trips.
- A tick that sent almost nothing with video waiting says nothing about the bitrate. A 3 s dropout
  had cut 28 Mb/s to 2.
- A path is taken to carry 20 Mb/s until it shows otherwise, and what's remembered lasts 30 minutes,
  not 2. A session had started with an estimate of 0.01 Mb/s and taken 2 s to send its first picture.

In a model of a 40 Mb/s link under a game wanting 55, the round trip settles at 4 ms on average and
27 ms at worst, at 38 Mb/s. Replay can't show the rest: it doesn't reproduce the start-up stall, and
the CPU encoder ignores targets that low. A real path falling from 40 to 10 Mb/s for 8 s lost 24
frames against 26 in 0.10.2, with a worst window of 184 ms against 166.

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
