# Vendored gpu-video

Copied from [gpu-video](https://crates.io/crates/gpu-video) 0.4.0 (Software Mansion,
[smelter](https://github.com/software-mansion/smelter), MIT; see `LICENSE`).

Changes for Connect:

- **wgpu 30**, the version eframe 0.37 uses, so the window can render with the decoder's own
  device and draw decoded textures directly.
  - `create_texture_from_hal` now takes the texture's starting state.
  - `Queue::submit` takes the fence by shared reference.
- **Decoded textures end in `SHADER_READ_ONLY_OPTIMAL`** rather than `GENERAL`, the state wgpu
  is told they start in.
- **H.265 decoding:**
  - `parser/h265` covers parameter sets, slice headers, POC and RPS, with tests against x265
    streams in `tests/h265`.
  - `vulkan_decoder/h265.rs` holds the session and decode submission.
  - `BytesDecoderH265` and `WgpuTexturesDecoderH265`, created by
    `VulkanDevice::create_{bytes,wgpu_textures}_decoder_h265`.
  - `Queues::decode_h265` and `VulkanDevice::decoding_device_h265` are the device side.
- **Packaging:** the manifest no longer has examples or workspace lints.
