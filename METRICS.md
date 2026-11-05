# Performance metrics

Connect 0.8 traces every frame from the host's screen to the viewer's window, and every input
event from the viewer to the host. This page covers what each figure measures, which clock it
uses, and where it stops being exact. The goal is a baseline you can trust before anything is
optimised.

## Getting the numbers

| What | How |
|---|---|
| Live summary | **Ctrl+Alt+S** in a session. Percentiles over the last second. |
| Save the viewer's trace | **Ctrl+Alt+E** in a session. CSV files go to `connect-traces` in the folder the app was started from, or to the temp folder if that one isn't writable. |
| Session files, on by default in the app | Each PC keeps its last 5 hosted and last 5 viewed sessions in a `session-files` folder (Settings → Session files → Open folder). The viewer sends its files to the host during the session, and the host writes `report-<session>.txt` when it ends. |
| Trace every session, both sides | Set `CONNECT_TRACE` to a directory. Each side streams its CSV files there as the session goes, every 2 seconds. |
| Follow every frame | Put both PCs' files in one folder and run `connect trace-report <folder>`. See [Following frames](#following-frames). |
| Terminal sessions | `connect host --trace DIR` and `connect join <USER-ID> --trace DIR`. These do the same as the setting above. |
| Turn tracing off | `CONNECT_TRACE=0` (also `off` or `false`). Nothing is recorded; the overlay falls back to the running averages it showed before 0.8. |

Tracing is on by default; the overlay's percentiles need it. Without a trace directory, each
buffer keeps the last 4,096 records (about 17 s at 240 fps), so Ctrl+Alt+E saves the most recent
stretch, and older records are overwritten. With a trace directory, each buffer keeps 65,536
records (about 4.5 minutes at 240 fps, what Ctrl+Alt+E saves), and everything also goes to the CSV
files as the session runs, so a session of any length keeps every record, and a crash loses at most
the last few seconds. `records_overrun` in the session files says if any were missed. Allow about
200 MB an hour for the viewer's files and 120 MB for the host's at 240 fps; half that at 120.

## Clocks

Three clocks appear in the traces. The CSV columns say which one each figure uses.

- **The host's clock** (`host_*` columns in the viewer's files, and every time in `host-*` files).
- **The viewer's clock** (`viewer_*` columns). Both clocks come from `connect_proto::now_micros()`:
  the wall-clock time when the process first asked, plus time since on the monotonic clock.
  They never jump when the system clock is adjusted. They read as Unix-epoch µs only roughly,
  because they don't follow the system clock after start-up.
- **Windows' performance counter** (`viewer_painted_qpc`). This is the raw QueryPerformanceCounter
  value, the clock PresentMon logs. Its frequency is in `viewer-*-session.csv`.

**The two PCs' clocks are never subtracted directly.** Figures that span both PCs go through
an estimated **clock offset** (viewer clock minus host clock):

- The viewer pings the host every second. Each reply implies an offset: the middle of the round
  trip minus the host's stamp. It is off by at most half the round trip.
- The estimate is the fastest round trip of the last 10 s. Only recent pings count, because PC
  clocks drift apart by tens of µs per second.
- The **uncertainty** is half that round trip. It is shown in the overlay ("clocks ±x ms") and
  stored with every record.
- If the path is slower one way than the other, the offset is wrong by up to half the
  difference. That error stays within the stated uncertainty.

Only three figures go through the offset: **network**, **total**, and **input to host**. All are
labelled "est.". Every other figure is a difference of two readings of one PC's clock.

## Frame stages

Every stage below is measured for every frame. The overlay shows the 50th percentile of each
over the last second. The CSV has the per-frame values.

### On the host

The host's clock throughout. The frame header carries these to the viewer: present to capture,
capture, encode, and sent after. Only the host's own trace records the rest.

| Stage | From → to | Notes and limits |
|---|---|---|
| present→capture | Windows putting the image on screen (DXGI `LastPresentTime`) → capture acquiring it | 0/empty when Windows doesn't report it (e.g. no new present, only a pointer move) and with the test pattern. |
| capture | Image acquired → staged for the encoder | CPU time: the copy out of the duplication surface and *submitting* the colour conversion. GPU work still pending isn't included; see GPU convert. |
| GPU convert (µs in the overlay) | The BGRA→NV12 conversion's run time on the graphics card | D3D11 timestamp queries around the conversion's draws, read back a frame or two later without waiting. Only on the GPU-only path. Up to four in flight. A conversion whose slot is still busy goes untimed, as does one where the GPU clock changed frequency (disjoint). Joined to frames by `capture_index`. Since 0.9 the conversion is two pixel shaders, which timestamps time on every driver. **The video-processor fallback can't always be timed.** Some drivers run the video processor outside the command stream that timestamps mark, so start and end land almost together. On an RTX 5060 (driver 621.01), every conversion read 0 µs. Readings under 2 µs can't be a real conversion and are dropped. A timer that gives nothing else in its first 32 readings is switched off for the stream, and the stage shows "–". The conversion's cost then lands in *encode*, which waits for it. |
| held | Staged → the encoder starting on it | Waiting for the network backlog to clear, or for the frame's slot at the screen's refresh rate. Re-sent pictures (a requested keyframe of an unchanged screen) record 0 and are left out of percentiles. |
| encode | The encoder call | Includes waiting for the GPU conversion to finish, as the hardware encoder reads its result, and the encoder's own synchronous wait. |
| queue | Encoder done → the session task picking the frame up | The 2-frame channel to the network task, and the task being scheduled. |
| packetize | Cutting the frame into datagrams | Includes computing Reed-Solomon recovery chunks. |
| send | The `send_many_datagrams` call | Handing datagrams to the QUIC stack's buffer. It doesn't measure transmission; the stack paces packets out after this. `queued_bytes` is what that buffer already held. |
| sent after | Staged (`capture_time_us`) → packetizing starts | What the frame header carries. In the overlay, without the host's report, "send wait" is this minus encode. |

### On the viewer

The viewer's clock, except for the estimates.

| Stage | From → to | Notes and limits |
|---|---|---|
| network (est.) | The host starting to packetize → the frame's first datagram arriving | Through the clock offset; ± its uncertainty. Includes both PCs' network stacks and the QUIC stack's pacing. |
| reassembly | First datagram → the frame whole again | Mostly the rest of the frame's datagrams arriving. Includes `rebuild`. |
| rebuild | Putting the chunks together | CPU time, including Reed-Solomon decoding when chunks were lost (`rebuilt_chunks` > 0). |
| decode queue | Whole → decoding starts | Waiting for an earlier frame still arriving (reorder, up to 12 ms; `viewer_released_us` marks its end), then the decoder thread's queue. |
| decode | The decoder call | On the GPU (Vulkan Video), this includes the copy out and the CPU waiting for the GPU. |
| draw | Decoded → drawn into the window | The smooth-pacing wait, waiting for the UI frame, and for CPU-decoded frames the texture upload. The draw time is recorded when the window's renderer takes the frame. That is before its commands reach the GPU, and before the swap. |
| presentation | Drawn → on the display | **Not measured inside the app.** eframe has no hook after `present`. On Windows, join the trace with PresentMon (below). Up to one refresh with FIFO presentation, which Connect uses. |
| total (est.) | The image reaching the host's screen → drawn here | `present→capture + capture` (host), then staged → drawn, through the clock offset. When present→capture is unknown, it starts at capture. |

## Frame delivery and smoothness

| Figure | Meaning | Limits |
|---|---|---|
| fps | Frames decoded per second | |
| lost | Frames that never came together on the way: chunks missing beyond what recovery covered (or late past the wait), or none of their datagrams arrived | Running total. Each loss costs a keyframe request. Before 0.9.1 this also counted, a second time, the frames skipped while waiting for that keyframe, so one lost frame could read as five. `viewer-*-loss.csv` says why each one went, and whether its datagrams turned up later. |
| repaired | Frames rebuilt from recovery chunks | Running total. |
| skipped | Frames thrown away here, not lost: they built on a lost frame (so video waited for the next keyframe), or the decoder was too far behind | Running total. Before 0.9.1, frames the decoder's full queue turned away weren't counted at all. |
| superseded | Decoded, then replaced by a newer frame before being drawn | Running total in the first line, per window in `dropped`. Normal when the host sends faster than the display shows. |
| dropped | Lost + skipped + superseded, last second | |
| repeated (est.) | Extra frame times a picture stayed on screen, beyond what the host's own gap between frames explains | Counted against the stream's cadence: the median gap between captures or between draws, whichever is slower. A static screen sends nothing and isn't counted. This is an estimate from draw times, not display flips. |
| frame time | Mean ± standard deviation of the gaps between frames drawn, and their 99th percentile | Measured at draw time, not at the display. A gap includes a static screen's pauses. |

## Input

Each event carries a sequence number and the viewer's clock time when it was sent. The host
records when it arrived, when it was acted on, and what happened to it.

| Figure | From → to | Limits |
|---|---|---|
| input to host (est.) | The viewer handing the event to the session → the host reading it off the input stream | The host measures `received − sent` across the two clocks. The viewer adds the offset, so this is ± its uncertainty. The send time is taken when the GUI processes the event, up to one UI frame after the OS delivered it. |
| injecting | Read off the stream → `SendInput` (or the gamepad driver) returned | Events dropped because the host is using that device, or not allowed, are excluded. Their `outcome` says why. |
| input to screen | — | **Not yet measured.** The traces are designed to be joined for it. Match an input's `done_us` to the first host frame with a later `capture_time_us`; both use the host's clock. Match that frame's `seq` to the viewer's frames file for its draw time. The result is an upper bound: whether that frame shows the input's effect depends on the application. |

## CPU and memory

Sampled once a second by each process, about itself. The host's figures reach the viewer in its
stats report.

| Figure | Windows | Linux |
|---|---|---|
| CPU | `GetProcessTimes`, user + kernel, all threads | `/proc/self/stat` utime + stime, at the kernel's 10 ms tick |
| memory | Working set (physical memory in use, including shared pages such as the graphics driver's) | VmRSS (resident set, including shared pages) |
| private (CSV only) | Private commit (`PrivateUsage`), resident or not | RssAnon |

The overlay shows CPU as a share of all cores, like Task Manager, and as cores in use.
"0.45 cores" means 45% of one core's time.

## Percentiles

- They use the nearest-rank method, so each one is a value that actually occurred.
- A window is about a second. At 60 fps, p99 is the slowest or second-slowest frame. The CSV
  files allow longer windows.
- Each `Percentiles` has a `count`, the number of samples it covers. A stage with no samples
  shows "–".

## CSV files

Files are named `<role>-<session>-<what>.csv`, where `<session>` is the session's start in Unix
seconds. Ctrl+Alt+E saves are numbered, `viewer-<session>-save<n>-<what>.csv`, apart from the
files streamed as the session goes. Empty cells mean "not measured". All times are µs.

**`host-*-frames.csv`**, one row per frame sent:
- `seq`, `keyframe`, `codec`, `resent`, `capture_index`, `bytes`
- `capture_time_us` (host clock)
- `present_to_capture_us`, `capture_us`, `held_us`, `encode_us`, `queue_us`, `packetize_us`,
  `send_us`, `sent_after_us`
- `datagrams`, `recovery_datagrams`, `queued_bytes`, `bitrate_kbps`
- `dropped`: 1 if the path's datagram size shrank and the frame was dropped
- `encoder_input_wait_us`: of `encode_us`, the wait for a hardware encoder to ask for the picture;
  the rest is the wait for the frame to come out (since 0.10.5)

**`host-*-convert.csv`**: `capture_index`, `convert_gpu_us`. Join to frames on `capture_index`;
captures replaced before encoding have no frame.

**`host-*-capture.csv`**, one row per image captured from the screen, encoded or not (since 0.9.2):
- `capture_index`: joins to frames and conversions. An index with no frame was replaced before
  encoding.
- `time_us`, `capture_us`, `present_to_capture_us`
- `accumulated_frames`: images that reached the screen since the last capture (DXGI's count). A gap
  before a row with 1 here means nothing new reached the screen: the game. Over 1 means images came
  and capture missed them: a capture stall.
- `previous_replaced`: if the image before this one was never encoded, why: `turn` (at most one
  picture goes per refresh of the host's screen) or `backlog` (held while the network caught up)

**`host-*-load.csv`**, about once a second (since 0.9.2): `time_us`, `connect_cpu_percent`,
`gpu_graphics_percent`, `gpu_encode_percent`. A stall that lines up with a full graphics card came
from the load.

**`host-*-session.csv`**: `key,value` rows: version, session stamp, frames recorded, records missed
or dropped, and on Windows the displays in use with their refresh rates (`displays`, since 0.10.5).

**`host-*-network.csv`**, every 250 ms (since 0.9.1):
- `time_us`
- `sent_packets`, `lost_packets`: on the path since the last row (QUIC's count)
- `queued_bytes`: datagrams waiting to be sent
- `bandwidth_estimate_bps`, `cwnd_bytes`, `rtt_us`, `congestion_events`: the congestion
  controller's (BBR's) view of the path
- `path`: `direct` or `relay`
- `recovery_requests`: keyframe requests from the viewer since the last row, each a loss
- `bitrate_kbps`, `ceiling_kbps`, `reason`: the encoder's target after this tick, the ceiling the
  rate controller keeps under, and why it changed (`steady`, `backlog`, `loss`, `viewer_loss`,
  `climb`, `creep`, `delay` (the round trip stayed well past its best with the video near the
  estimate), `viewer_rate` (the viewer reported getting well under what was sent), `viewer_decode`
  (its decoder is at its limit), `limit` (over the settings, this viewer's share of the upload, or
  what the encoder keeps up with), `loss_held`: the viewer lost frames, but the last cut did nothing for
  the loss, so the bitrate holds)
- `video_bytes`: video handed to the network since the last row
- `local_mouse_moves`, `local_mouse_pixels`, `local_mouse_tagged`, `local_mouse_buttons`,
  `local_keys`: what the host's own mouse and keyboard did since the last row, as the low-level
  hooks saw it, without injected input. This is what sets host priority off. Pixels tell a hand
  (hundreds) from a jittery sensor (a few). Tagged events came from something that marked them, as
  mouse software does. (since 0.9.2)
- `host_priority_drops`: remote events dropped for host priority since the last row (since 0.9.2)

**`host-*-input.csv`**: `seq`, `kind`, then the times:
- `client_time_us_viewer_clock` (the viewer's clock; don't compare it with the others)
- `received_us`, `done_us`, `inject_us` (host clock)

and `outcome`: `pointer_only` (moved the connector's labelled pointer, as others share the mouse),
`injected`, `refused` (Windows blocked it, e.g. an elevated window has focus),
`host_priority` (the host user is using that device), `not_allowed`, or `channel` (test sink).

**`viewer-*-frames.csv`**, one row per frame decoded:
- `seq`, `keyframe`, `codec`, `bytes`
- `status`: `shown` or `superseded`
- `decoder`: `gpu` or `cpu`
- `host_capture_time_us`, `host_present_to_capture_us`, `host_capture_us`, `host_encode_us`,
  `host_sent_after_us`, all on the host's clock
- `viewer_first_seen_us`, `viewer_completed_us`, `viewer_released_us`, `viewer_decode_start_us`,
  `viewer_decoded_us`, `viewer_painted_us`, all on the viewer's clock
- `viewer_painted_qpc` (Windows)
- `data_chunks`, `recovery_chunks`, `rebuilt_chunks`, `rebuild_us`
- `reassembly_us`, `decode_queue_us`, `decode_us`, `render_us`
- `clock_offset_us`, `clock_uncertainty_us`: the estimate when the frame was recorded
- `network_est_us`, `total_est_us`

**`viewer-*-loss.csv`**, one row per frame that never reached the decoder (since 0.9.1):
- `seq`, `keyframe`
- `reason`: `incomplete`, `never_seen` or `evicted` (lost: `lost` is 1), or `skipped_after_loss` or
  `skipped_decoder_behind`
- `chunks_received`, `chunks_needed` (data chunks; empty if none came)
- `viewer_first_seen_us`, `viewer_last_seen_us`: its first and last datagram before it was given up
- `viewer_given_up_us`
- `late_datagrams`, `viewer_last_late_us`: its datagrams that came after it was given up on, within
  a second. A lost frame with late datagrams was delayed, not dropped by the network.

Join to the host's frames on `seq` for its size and when it was sent.

**`viewer-*-network.csv`**, once a second: `viewer_time_us`, `path` (`direct` or `relay`), `rtt_us`,
`mbps`, `fps`, the running totals `frames_lost`, `frames_skipped`, `frames_repaired` and
`frames_superseded`, the host's `host_target_kbps` and `host_loss_permille`, the clock estimate,
and `gpu_graphics_percent` and `gpu_decode_percent` (since 0.9.2). A change of `path` mid-session
shows here.

**`viewer-*-input.csv`**: `seq`, `kind`, `viewer_sent_us`, `clock_offset_us`,
`clock_uncertainty_us`. Join to the host's input file on `seq`. The one-way estimate is
`received_us + clock_offset_us − viewer_sent_us`.

**`viewer-*-session.csv`**: `key,value` rows. They hold the version, the session stamp,
`qpc_frequency_hz`, frames recorded, and records missed (`records_overrun`) or dropped.

Join the two frames files on `seq`. Within a session it counts up by one per frame encoded.

## Following frames

`seq` identifies a frame from the moment it's encoded: it's in `host-*-frames.csv`, and travels with
the frame to `viewer-*-frames.csv` (frames decoded) and `viewer-*-loss.csv` (frames that weren't).
`capture_index` goes back further, to the capture, in `host-*-capture.csv`. `connect trace-report
<folder>` joins them for the newest session in the folder. It writes `report-<session>.txt`, and
`report-<session>-events.csv` with every run of frames in trouble: when, why, and from which `seq`
to which.

Every frame gets at most one reason:

| Reason | What it means | Evidence |
|---|---|---|
| `network_loss` | Its datagrams never came, or too few to rebuild it | Loss file: `incomplete`, `never_seen` or `evicted`. Compare `lost_packets` in the host's network file. |
| `network_late` | Its datagrams came, after the viewer gave up on it | Loss file: `late_datagrams` above 0 |
| `skipped_after_loss` | Arrived, but built on a lost frame | Loss file |
| `network_queue` | Drawn late, mostly lost on the way | Network estimate, reassembly and the host's send stages over their usual. Not always the network itself: it includes BBR's pacing on the host. `queued_bytes` shows what waited on the host; a rising `rtt_us` means a queue on the path. |
| `host_capture` | Drawn late, mostly before staging | `present_to_capture_us`, `capture_us` |
| `host_held` | Drawn late, held back before encoding | `held_us` |
| `encoder` | Drawn late, mostly in the encoder | `encode_us` |
| `viewer_decode` | The decoder fell behind, or the frame was drawn late from waiting for it or decoding | Loss file: `skipped_decoder_behind`; `decode_queue_us`, `decode_us` |
| `replaced_by_newer` | Decoded, but a newer frame replaced it before it could be drawn | `status` is `superseded` |
| `viewer_draw` | Drawn late, mostly in drawing | `render_us` |
| `unaccounted` | The host sent it and the viewer has no record | The viewer's trace missed it, or that session's trace is incomplete |

A frame counts as late over 16 ms above the session's median latency; the stage to blame is the one
furthest over its own median. Latency here is `total_est_us`, so it's as good as the clock estimate.

The report also lists capture gaps of 50 ms or more, split into stalls (images missed) and quiet
(nothing new on screen), and what the host's own input was doing when host priority dropped remote
events. Gaps in capture aren't charged to frames: a frame that was never captured has no number.

`replaced_by_newer` covers what the app knows. A frame drawn by the app and then dropped on its way
to the display is invisible here; only PresentMon sees that (below).

## Joining with PresentMon (Windows)

Run PresentMon on the viewer with QPC timestamps, for example:

```
PresentMon.exe --process_name connect.exe --qpc_time --output_file present.csv
```

For each viewer frame row, take the first present whose start time (QPC) is at or after
`viewer_painted_qpc`. That present carries the frame. Its PresentMon display columns give
when it reached the screen. QPC ticks convert to µs with `qpc_frequency_hz`.

## Overhead and bounds

- Writing a record never waits. Each buffer is a preallocated ring behind a lock that writers
  only *try*. If an export or the once-a-second summary is reading it at that moment, the record
  is dropped and counted. The overlay shows the count only if it isn't zero.
- No video thread writes files. With a trace directory, a thread of its own appends new records to
  the CSV files every 2 seconds, copying them out of the rings under the same lock, for a few
  microseconds. Ctrl+Alt+E saves on its own thread too.
- With tracing off, each record costs one relaxed atomic load, and nothing is allocated.
- Memory is bounded:
  - Default: 4,096 records per ring, about 0.6 MB for the viewer's frames.
  - With a trace directory: 65,536 records, about 10 MB.
  - Pages are only touched as records are written.
- No frame waits on tracing, and nothing is buffered for it. The video pipeline's own queues are
  unchanged.

## Known limitations

- Forwarded viewer files stop a couple of seconds short of the session's end, because they go
  out after being written and the connection closes first. Files from a viewer older than 0.10.1
  don't arrive, so there's no report; put both PCs' files in one folder as before.
- Presentation isn't measured in the app (see PresentMon above).
- GPU convert isn't measurable on drivers that run the video processor outside the timestamped command stream, including NVIDIA's (see above). It also covers only the colour conversion. The encoder's own GPU time is inside
  "encode", which also includes waiting.
- Viewer-side GPU decode time is measured from the CPU, including the copy and the CPU's wait.
- The decoder's queue and the reorder wait are one figure in the overlay. The CSV separates
  them (`viewer_released_us`).
- On Linux, process CPU has 10 ms resolution: at 1-second samples, about ±1% of a core.
- Superseded frames that the decoder decoded without a picture buffer have no draw time, by
  definition.
- "Repeated" is an estimate from draw times, not display flips.
- The input send time is when the GUI processes the event, not when the OS received it.
