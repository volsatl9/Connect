# Video codecs and the GPU pipeline

How Connect picks and runs video codecs, what 0.6.0 built, and the plan for H.265 (0.7.0) and
AV1 (later).

## How a codec is chosen (0.6.0)

1. Right after its hello, the viewer sends `ClientMsg::Decoders`: each codec it can decode, and
   whether its graphics card does it. Hardware decoding counts only while **Settings → Viewing →
   Hardware video decoding** is on. This is a separate message so that a host on another
   version can still read the hello and reply "update both PCs".
2. The host lists what it can encode (`encode::supported`). Hardware encoders come from Media
   Foundation's registered hardware MFTs; OpenH264 is always available.
3. `connect_proto::codec::choose` ranks the codecs both sides share:
   - The number of ends doing the work on a graphics card (2, 1 or 0) comes first. A processor
     struggling at 144 fps costs more than a less efficient codec saves.
   - Then efficiency: AV1, then H.265, then H.264.
   - Software H.264 on both sides is the floor, so two Connect builds always share a codec.
4. Every video datagram carries the codec in two flag bits. The host only changes codec at a
   keyframe, and the viewer opens a matching decoder when the codec changes, so the switch
   doesn't depend on a control message arriving first.
5. If the viewer can't decode the codec after all (no decoder opens, or a hardware decoder
   fails on a keyframe with no software decoder to fall back to), it sends
   `ClientMsg::DecoderFailed`. The host drops that codec and runs `choose` again.

A failing hardware encoder on the host falls back to OpenH264 by itself. A codec it can't
continue with is dropped for the rest of the stream.

## Encoders and decoders

- **Host** (`crates/host/src/encode.rs`): `trait VideoEncoder`, which takes an `Input`.
  - **Implementations:** `software::OpenH264` (I420 in memory) and `mf::HardwareEncoder` (NV12 in
    memory, or NV12 textures when opened with the capture device).
  - **To add a codec:** implement the trait, list the codec in `supported`, and open it in
    `open_memory_encoder` / `open_gpu_path`.
- **Viewer** (`crates/client/src/decoder.rs`): `trait VideoDecoder`.
  - **Implementations:** `OpenH264`, and `GpuDecoder` (Vulkan Video).
  - **To add a codec:** implement the trait, add it to `open`, `software` and `supported`.
  - **Only real decoders are offered:** a card that reports H.265 decoding isn't offered as
    such until a decoder exists for it.

## The GPU-only host pipeline (0.6.0)

Through 0.5.0, every frame went from the GPU to the CPU and back to the GPU:

- **0.5.0:** desktop image → staging copy → map → convert to NV12 on the CPU (multi-threaded)
  → copy into a Media Foundation buffer → hardware encoder.
- **0.6.0:** desktop image → our own texture (a GPU copy) → D3D11 video processor converts to
  NV12 (BT.601 limited range, like the CPU path) → the same texture goes to the encoder through
  an `IMFDXGIDeviceManager`.

What makes this work:

- **One shared device.** Capture, conversion and encoder all use the capture's D3D11 device,
  created with video support and multithread protection.
- **Fallbacks:**
  - If the video processor, texture-reading encoders or the device's video support are
    missing, it goes through memory as before.
  - If conversion or encoding fails mid-stream, the current picture is read back from the GPU,
    so the stream continues without waiting for the screen to change.
- **Testing:**
  - `CONNECT_GPU_PIPELINE=0` turns the GPU path off, for comparisons.
  - `connect gpu-check` runs the path for 3 seconds and compares the GPU conversion with the
    CPU one.

## H.265 (0.7.0)

**Encoding (Windows):**
- **Encoder MFTs:** NVIDIA, AMD and Intel all register hardware HEVC encoder MFTs.
  `mf::encoder_names` and `gpu-check` already list them. `HardwareEncoder` needs the
  `MFVideoFormat_HEVC` subtype, Main profile (`eAVEncH265VProfile_Main_420_8`), low-latency and
  no-B-frame settings, as for H.264.
- **Bitstream handling:** an HEVC version of `H264Bitstream` must recover VPS/SPS/PPS (NAL types
  32–34) and put them in front of each IDR (types 19/20).

**Decoding:**
- **Vulkan Video is the only practical route.** `gpu-video` 0.4 decodes only H.264; it reports
  H.265 support but has no H.265 parser or decoder.
- **Plan:** fork it (MIT) and add H.265 decoding: VPS/SPS/PPS and slice-header parsing,
  reference picture sets, and `VkVideoDecodeH265PictureInfoKHR`. This is the biggest single job
  in 0.7.0.
- **Rejected alternatives:**
  - Media Foundation's HEVC decoder on Windows needs the paid "HEVC Video Extensions" from the
    Store.
  - FFmpeg, and libde265 for software decoding, would bring LGPL code and system libraries.
- **No software H.265 decoder.** A viewer only offers H.265 where its graphics card decodes it,
  and negotiation keeps H.264 as the fallback.

## AV1 (after 0.7.0)

**Why:** AV1 is about 30% smaller than H.265 for the same quality. It also has screen-content
tools (palette mode and intra block copy) that suit desktop and UI-heavy games.

**Encoding:**
- **Hardware:** Media Foundation AV1 encoder MFTs ship with NVIDIA RTX 40-series and later,
  AMD RX 7000 and later, and Intel Arc. `gpu-check` already lists them. The `HardwareEncoder`
  changes match H.265's: the `MFVideoFormat_AV1` subtype, low-delay settings, and periodic and
  forced key frames.
- **No software encoder.** SVT-AV1 and rav1e are too slow for real-time 1440p at 120+ fps.

**Wire format:**
- **Temporal units:** one AV1 temporal unit per frame, as an OBU stream with a temporal
  delimiter.
- **Keyframes:** a key frame starts with a sequence header OBU, so a viewer joining mid-stream or
  recovering can start there.
- **Keyframe flag:** the host sets it from the frame header's `frame_type`.

**Decoding:**
- **Hardware:** Vulkan Video AV1 decoding (`VK_KHR_video_decode_av1`) works on NVIDIA RTX 30+,
  AMD RX 6000+ and Intel Xe / Arc with current drivers. It needs the same fork work as H.265:
  sequence and frame header parsing, and reference frame management.
- **Software:** `rav1d` (a Rust port of dav1d, BSD-2) is fast enough to decode AV1 in software
  at 1080p60 and above on modern CPUs. AV1 could therefore get a software fallback on the viewer,
  unlike H.265. It cross-compiles without a C toolchain.
- **Ranking:** `choose` already ranks AV1 first among equals, so no negotiation changes are
  needed. A hardware AV1 encoder paired with a software-only AV1 decoder ranks 1 and loses to
  hardware H.264 at both ends (rank 2). That's the intended trade-off for high frame rates.

**Order of work:**
1. AV1 hardware decoding through the forked `gpu-video`, sharing the H.265 groundwork.
2. The `HardwareEncoder` AV1 subtype.
3. Optionally, `rav1d` as a software decoder.

## Decoded pictures straight to the screen (0.7.0)

**Today:** Vulkan Video decodes on the GPU, then each picture is copied back to memory as NV12
(about 5.5 MB a frame at 1440p), converted to I420, and uploaded again for the YUV shader.

**What I looked into:**
- **Rendering the decoded `VkImage` directly** needs the decoder and the window to share a
  device.
  - **What eframe allows:** it already renders with wgpu, and `egui_wgpu::WgpuSetup::Existing`
    lets the app supply the device.
  - **Version mismatch:** `gpu-video`'s wgpu integration can create that device, with the video
    queues enabled, but it targets wgpu 29 while eframe 0.37 uses wgpu 30. The fork for H.265
    would move it to wgpu 30.
  - **Windows:** wgpu picks Direct3D 12 by default, so the window would have to use wgpu's Vulkan
    backend whenever hardware decoding is on.
  - **The decoded picture:** a two-plane `G8_B8R8_2PLANE_420_UNORM` image. `gpu-video` already
    exposes it as a wgpu texture with separate plane views, and the existing YUV shader takes
    those planes instead of three uploaded ones.
- **Sharing a Vulkan image with Direct3D 12 or OpenGL through external memory** would avoid
  changing the window's backend. But it needs synchronisation and handle types that differ per
  driver, and is harder to test.

**Plan:**
1. Fork `gpu-video` and move it to wgpu 30.
2. When hardware decoding is on and the card supports it, start the window on Vulkan with
   `gpu-video`'s device.
3. Decode straight into textures the window samples.
4. Keep today's copy-back path when the window isn't on Vulkan, or any step fails.
