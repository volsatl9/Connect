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
- **Packaging:** the manifest no longer has examples or workspace lints.
