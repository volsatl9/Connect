//! Direct hardware-only Media Foundation encoding.
//!
//! The object, COM apartment and MF startup all belong to the capture thread. Pictures arrive
//! as NV12, either in CPU memory (converted straight from the captured image) or, opened with
//! the capture's Direct3D device, as textures on the graphics card that never leave it. The
//! selected encoder itself must be a hardware MFT.

use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

use super::{Encoded, Input, InputKind, VideoEncoder};
use crate::yuv::Layout;
use connect_proto::Codec;
use windows::{
    Win32::{
        Foundation::{E_NOTIMPL, S_FALSE, S_OK},
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::*,
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
            Variant::VARIANT,
        },
    },
    core::{GUID, Interface},
};

const MAX_BITSTREAM: usize = 16 * 1024 * 1024;
const MAX_RAW_BYTES: usize = 128 * 1024 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(2);
const GOP_FRAMES: u64 = 600;
/// How long to wait between looks at a hardware encoder that isn't ready yet: short, as a
/// frame takes only a few milliseconds and every one waited past adds to the delay.
const WAIT_STEP: Duration = Duration::from_micros(200);
/// For this long into a wait, only yield instead of sleeping: a sleep can overshoot by half a
/// millisecond on Windows, as long as a whole encode at 1080p.
const SPIN: Duration = Duration::from_millis(3);

/// Waits a moment before looking at the encoder again, `since` the wait began.
fn pause(since: Instant) {
    if since.elapsed() < SPIN {
        std::thread::yield_now();
    } else {
        std::thread::sleep(WAIT_STEP);
    }
}

pub(crate) struct HardwareFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub capture_time_us: u64,
}

pub(crate) struct HardwareEncoder {
    transform: IMFTransform,
    /// Shares the capture's Direct3D device with the encoder, when it reads textures.
    manager: Option<IMFDXGIDeviceManager>,
    codec: ICodecAPI,
    events: Option<IMFMediaEventGenerator>,
    _activation: Activation,
    name: String,
    width: usize,
    height: usize,
    fps: u32,
    input_id: u32,
    output_id: u32,
    input_credit: u32,
    output_credit: u32,
    frames: u64,
    last_timestamp: Option<i64>,
    /// Whether the driver has already turned down a bitrate change, so it isn't asked again.
    fixed_bitrate: bool,
    video_codec: Codec,
    bitstream: Bitstream,
    // Declared last: all COM interfaces are released before MF/COM shutdown.
    _runtime: Runtime,
}

struct Runtime(PhantomData<Rc<()>>);

impl Runtime {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().context("initializing video COM apartment")?;
            if let Err(error) = mfplat::startup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(error).context("starting Media Foundation");
            }
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            mfplat::shutdown();
            CoUninitialize();
        }
    }
}

struct Activation(IMFActivate);

impl Drop for Activation {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ShutdownObject();
        }
    }
}

/// The codecs a hardware encoder here can produce, as far as Connect can drive them.
pub(crate) fn hardware_codecs() -> Vec<Codec> {
    let Ok(_runtime) = Runtime::new() else { return Vec::new() };
    // AV1 hardware encoders are listed by `encoder_names` but not driven yet. H.265 is only
    // chosen when the viewer can decode it.
    [Codec::H264, Codec::H265]
        .into_iter()
        .filter(|&codec| hardware_encoders(subtype(codec)).is_ok_and(|list| !list.is_empty()))
        .collect()
}

/// The names of the hardware encoders registered for `codec`, for diagnostics.
pub(crate) fn encoder_names(codec: Codec) -> Result<Vec<String>> {
    let _runtime = Runtime::new()?;
    Ok(hardware_encoders(subtype(codec))?
        .iter()
        .map(|activation| {
            attribute_string(&activation.0, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|_| "unnamed".into())
        })
        .collect())
}

fn subtype(codec: Codec) -> &'static GUID {
    match codec {
        Codec::H264 => &MFVideoFormat_H264,
        Codec::H265 => &MFVideoFormat_HEVC,
        Codec::Av1 => &MFVideoFormat_AV1,
    }
}

impl HardwareEncoder {
    /// Opens a hardware `codec` encoder. Given `device`, it reads NV12 textures on that device
    /// (see [`Input::Texture`]); otherwise NV12 pictures in memory.
    pub fn new(
        codec: Codec,
        width: u32,
        height: u32,
        bitrate_bps: u32,
        fps: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<Self> {
        ensure!(codec != Codec::Av1, "Connect doesn't drive hardware {codec} encoders yet");
        ensure!(
            width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "hardware H.264 requires nonempty even dimensions"
        );
        ensure!(fps > 0 && fps <= 240 && bitrate_bps > 0, "invalid H.264 frame rate or bitrate");
        let bytes = u64::from(width) * u64::from(height) * 3 / 2;
        ensure!(bytes <= MAX_RAW_BYTES as u64, "hardware H.264 image exceeds input limit");
        let runtime = Runtime::new()?;
        let manager = device.map(device_manager).transpose()?;
        let activations = hardware_encoders(subtype(codec))?;
        let mut failures = Vec::new();
        for activation in activations {
            let name = attribute_string(&activation.0, &MFT_FRIENDLY_NAME_Attribute)
                .unwrap_or_else(|_| "unnamed hardware H.264 MFT".into());
            match Self::configure(&activation, codec, width, height, bitrate_bps, fps, manager.as_ref()) {
                Ok((transform, controls, events, input_id, output_id)) => {
                    return Ok(Self {
                        transform,
                        manager,
                        codec: controls,
                        events,
                        _activation: activation,
                        name: format!("GPU: {name}"),
                        width: width as usize,
                        height: height as usize,
                        fps,
                        input_id,
                        output_id,
                        input_credit: 0,
                        output_credit: 0,
                        frames: 0,
                        last_timestamp: None,
                        fixed_bitrate: false,
                        video_codec: codec,
                        bitstream: Bitstream::new(codec),
                        _runtime: runtime,
                    });
                }
                Err(error) => failures.push(format!("{name}: {error:#}")),
            }
        }
        bail!(
            "no usable hardware {codec} encoder: {}",
            if failures.is_empty() {
                format!("MFTEnumEx returned no hardware NV12/{codec} transforms")
            } else {
                failures.join("; ")
            }
        )
    }

    fn configure(
        activation: &Activation,
        video_codec: Codec,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        manager: Option<&IMFDXGIDeviceManager>,
    ) -> Result<(IMFTransform, ICodecAPI, Option<IMFMediaEventGenerator>, u32, u32)> {
        unsafe {
            // Both the enumeration flag and hardware registration are required. Never instantiate
            // the Microsoft software encoder as an alternative within this module.
            ensure!(
                !attribute_string(&activation.0, &MFT_ENUM_HARDWARE_URL_Attribute)?.is_empty(),
                "MFT has no hardware registration"
            );
            let transform: IMFTransform = activation.0.ActivateObject().context("activating hardware MFT")?;
            let attributes = transform.GetAttributes()?;
            let asynchronous = attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            let events = if asynchronous {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };
            if let Some(manager) = manager {
                // Before any media type: the encoder picks its texture handling from the device.
                ensure!(
                    attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0,
                    "hardware encoder can't read Direct3D 11 textures"
                );
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .context("handing the capture device to the encoder")?;
            }
            let codec: ICodecAPI = transform.cast().context("hardware MFT has no codec controls")?;
            set_value(&codec, &CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
                .context("hardware encoder does not accept low-latency mode")?;
            ensure!(
                bool::try_from(&codec.GetValue(&CODECAPI_AVLowLatencyMode)?)?,
                "hardware encoder did not enable low latency"
            );
            // Where the B-frame control is exposed, require the driver to accept and report zero
            // instead of ignoring a failed setting. Without it (NVIDIA's HEVC encoder), H.264
            // Baseline rules B frames out anyway, and for H.265 low-latency mode should; an
            // encoder that reorders all the same fails `encode_frame`'s one-frame-in-flight
            // check on its first B frame, and the stream falls back to H.264.
            if supports(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount)? {
                set_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0)?;
                ensure!(
                    u32::try_from(&codec.GetValue(&CODECAPI_AVEncMPVDefaultBPictureCount)?)? == 0,
                    "hardware encoder enables B frames"
                );
            }
            ensure!(
                supports(&codec, &CODECAPI_AVEncVideoForceKeyFrame)?,
                "hardware encoder cannot request recovery keyframes"
            );
            set_u32(&codec, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32)?;
            set_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
            // Explicit periodic force-IDR below also covers drivers without a GOP size control.
            if supports(&codec, &CODECAPI_AVEncMPVGOPSize)? {
                set_u32(&codec, &CODECAPI_AVEncMPVGOPSize, GOP_FRAMES as u32)?;
            }
            let (mut inputs, mut outputs) = (0, 0);
            transform.GetStreamCount(&mut inputs, &mut outputs)?;
            ensure!(inputs == 1 && outputs == 1, "unsupported hardware MFT stream count");
            let (mut input_ids, mut output_ids) = ([0], [0]);
            if let Err(error) = transform.GetStreamIDs(&mut input_ids, &mut output_ids) {
                ensure!(error.code() == E_NOTIMPL, "querying hardware MFT stream IDs: {error}");
            }
            let (input_id, output_id) = (input_ids[0], output_ids[0]);
            let profile = match video_codec {
                Codec::H264 => eAVEncH264VProfile_Base.0 as u32,
                _ => eAVEncH265VProfile_Main_420_8.0 as u32,
            };
            let output = video_type(width, height, fps, subtype(video_codec))?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            output.SetUINT32(&MF_MT_VIDEO_NO_FRAME_ORDERING, 1)?;
            transform
                .SetOutputType(output_id, &output, 0)
                .with_context(|| format!("setting hardware {video_codec} output type"))?;
            let input = video_type(width, height, fps, &MFVideoFormat_NV12)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
            input.SetUINT32(&MF_MT_SAMPLE_SIZE, width * height * 3 / 2)?;
            input.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            transform.SetInputType(input_id, &input, 0).context("setting hardware NV12 input type")?;
            // Verify the negotiated profile, as it is our guarantee when a driver omits B control.
            let actual = transform.GetOutputCurrentType(output_id)?;
            ensure!(
                actual.GetUINT32(&MF_MT_MPEG2_PROFILE)? == profile,
                "hardware encoder did not negotiate {} {video_codec}",
                if video_codec == Codec::H264 { "Baseline" } else { "Main" }
            );
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok((transform, codec, events, input_id, output_id))
        }
    }

    fn encode_frame(
        &mut self,
        input: Input<'_>,
        capture_time_us: u64,
        force_keyframe: bool,
    ) -> Result<Vec<HardwareFrame>> {
        match input {
            Input::Picture(image) => {
                ensure!(self.manager.is_none(), "this hardware encoder reads textures");
                ensure!(image.size() == (self.width, self.height), "recreate hardware encoder after resizing");
                ensure!(image.layout() == Layout::Nv12, "hardware encoder takes NV12");
            }
            Input::Texture(_) => ensure!(self.manager.is_some(), "this hardware encoder reads memory"),
        }
        let timestamp = i64::try_from(capture_time_us.checked_mul(10).context("capture timestamp overflow")?)?;
        ensure!(self.last_timestamp.is_none_or(|last| timestamp > last), "capture timestamps must increase");
        let timeout = if self.frames == 0 { START_TIMEOUT } else { FRAME_TIMEOUT };
        let waiting = Instant::now();
        let deadline = waiting + timeout;
        while self.events.is_some() && self.input_credit == 0 {
            self.poll_events()?;
            ensure!(Instant::now() < deadline, "hardware encoder timed out requesting input");
            if self.input_credit == 0 {
                pause(waiting);
            }
        }
        let keyframe_requested = force_keyframe || self.frames.is_multiple_of(GOP_FRAMES);
        if keyframe_requested {
            set_u32(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, 1).context("requesting hardware IDR frame")?;
        }
        let sample = self.input_sample(input, timestamp)?;
        unsafe {
            self.transform.ProcessInput(self.input_id, &sample, 0).context("submitting NV12 frame to hardware")?;
        }
        self.input_credit = self.input_credit.saturating_sub(1);
        self.last_timestamp = Some(timestamp);
        self.frames += 1;
        // Exactly one in-flight frame. A driver that buffers/reorders despite the requested
        // controls fails boundedly, so the owner can recreate or fall back to OpenH264.
        loop {
            if self.events.is_some() {
                self.poll_events()?;
            }
            if (self.events.is_none() || self.output_credit > 0)
                && let Some(sample) = self.output_sample()?
            {
                let output_time =
                    unsafe { sample.GetSampleTime() }.context("hardware output has no capture timestamp")?;
                ensure!(output_time == timestamp, "hardware encoder reordered or changed sample timestamps");
                self.read_sequence_header()?;
                let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
                let data = read_buffer(&buffer)?;
                let (data, keyframe) = self.bitstream.frame(&data)?;
                ensure!(!keyframe_requested || keyframe, "hardware encoder did not honor recovery IDR request");
                return Ok(vec![HardwareFrame { data, keyframe, capture_time_us: (output_time / 10) as u64 }]);
            }
            ensure!(Instant::now() < deadline, "hardware encoder timed out producing a frame (one frame in flight)");
            pause(waiting);
        }
    }

    fn poll_events(&mut self) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        // Bound a driver flooding events as well as the wait for a missing event.
        for _ in 0..64 {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error).context("polling hardware encoder events"),
            };
            unsafe {
                event.GetStatus()?.ok().context("hardware encoder asynchronous failure")?;
                match event.GetType()? {
                    value if value == METransformNeedInput.0 as u32 => {
                        let stream = event.GetUINT32(&MF_EVENT_MFT_INPUT_STREAM_ID)?;
                        ensure!(stream == self.input_id, "hardware encoder requested an unknown stream");
                        self.input_credit = self
                            .input_credit
                            .checked_add(1)
                            .filter(|v| *v <= 64)
                            .context("hardware encoder excessive input requests")?;
                    }
                    value if value == METransformHaveOutput.0 as u32 => {
                        self.output_credit = self
                            .output_credit
                            .checked_add(1)
                            .filter(|v| *v <= 64)
                            .context("hardware encoder excessive output events")?;
                    }
                    value if value == MEError.0 as u32 => {
                        bail!("hardware encoder reported MEError")
                    }
                    _ => {}
                }
            }
        }
        bail!("hardware encoder flooded its event queue")
    }

    fn input_sample(&self, input: Input<'_>, timestamp: i64) -> Result<IMFSample> {
        let buffer = match input {
            Input::Picture(image) => {
                let nv12 = image.data();
                ensure!(nv12.len() <= MAX_RAW_BYTES, "NV12 image exceeds input limit");
                unsafe {
                    let buffer = mfplat::memory_buffer(nv12.len() as u32)?;
                    let mut pointer = std::ptr::null_mut();
                    buffer.Lock(&mut pointer, None, None)?;
                    std::ptr::copy_nonoverlapping(nv12.as_ptr(), pointer, nv12.len());
                    buffer.Unlock()?;
                    buffer.SetCurrentLength(nv12.len() as u32)?;
                    buffer
                }
            }
            Input::Texture(texture) => texture_buffer(texture)?,
        };
        unsafe {
            let sample = mfplat::sample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps))?;
            Ok(sample)
        }
    }

    fn output_sample(&mut self) -> Result<Option<IMFSample>> {
        // A stream change can accompany the first output. Renegotiate at most four times.
        for _ in 0..4 {
            unsafe {
                let info = self.transform.GetOutputStreamInfo(self.output_id)?;
                let sample = if info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                    != 0
                {
                    None
                } else {
                    ensure!(
                        info.cbSize > 0 && info.cbSize as usize <= MAX_BITSTREAM,
                        "invalid hardware output buffer size"
                    );
                    let sample = mfplat::sample()?;
                    // cbAlignment is an alignment mask (0 means no alignment requirement).
                    sample.AddBuffer(&mfplat::aligned_memory_buffer(info.cbSize, info.cbAlignment)?)?;
                    Some(sample)
                };
                let mut output = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_id,
                    pSample: ManuallyDrop::new(sample),
                    ..Default::default()
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut output, &mut status);
                let sample = ManuallyDrop::take(&mut output[0].pSample);
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                match result {
                    Ok(()) => {
                        self.output_credit = self.output_credit.saturating_sub(1);
                        ensure!(sample.is_some(), "hardware encoder returned no output sample");
                        return Ok(sample);
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        self.output_credit = self.output_credit.saturating_sub(1);
                        return Ok(None);
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let media_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                        ensure!(
                            media_type.GetGUID(&MF_MT_SUBTYPE)? == *subtype(self.video_codec),
                            "hardware encoder changed output codec"
                        );
                        ensure!(
                            media_type.GetUINT64(&MF_MT_FRAME_SIZE)?
                                == pack_pair(self.width as u32, self.height as u32),
                            "hardware encoder changed output size"
                        );
                        self.transform.SetOutputType(self.output_id, &media_type, 0)?;
                    }
                    Err(error) => return Err(error).context("receiving hardware H.264 output"),
                }
            }
        }
        bail!("hardware encoder repeatedly changed its output format")
    }

    fn read_sequence_header(&mut self) -> Result<()> {
        unsafe {
            let media_type = self.transform.GetOutputCurrentType(self.output_id)?;
            if let Ok(size) = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                ensure!(size <= 1024 * 1024, "oversized hardware H.264 sequence header");
                if size > 0 {
                    let mut header = vec![0; size as usize];
                    media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)?;
                    self.bitstream.sequence_header(&header)?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for HardwareEncoder {
    fn drop(&mut self) {
        unsafe {
            // Flush discards outstanding work; do not drain/wait during disconnect or resizing.
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

impl VideoEncoder for HardwareEncoder {
    fn codec(&self) -> Codec {
        self.video_codec
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn hardware(&self) -> bool {
        true
    }

    fn input(&self) -> InputKind {
        if self.manager.is_some() { InputKind::Texture } else { InputKind::Picture(Layout::Nv12) }
    }

    /// Changes the target bitrate for the frames that follow, if the driver allows it mid-stream.
    fn set_bitrate(&mut self, _from: u32, to: u32) {
        if self.fixed_bitrate {
            return;
        }
        if let Err(error) = set_u32(&self.codec, &CODECAPI_AVEncCommonMeanBitRate, to) {
            tracing::info!("hardware encoder keeps its bitrate: {error:#}");
            self.fixed_bitrate = true;
        }
    }

    fn encode(&mut self, input: Input<'_>, capture_time_us: u64, force_keyframe: bool) -> Result<Vec<Encoded>> {
        Ok(self
            .encode_frame(input, capture_time_us, force_keyframe)?
            .into_iter()
            .map(|frame| Encoded {
                data: frame.data.into(),
                keyframe: frame.keyframe,
                capture_time_us: frame.capture_time_us,
            })
            .collect())
    }
}

/// A Media Foundation device manager sharing `device` with encoders.
fn device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let (token, manager) = mfplat::dxgi_device_manager()?;
    unsafe { manager.ResetDevice(device, token) }.context("sharing the capture device with Media Foundation")?;
    Ok(manager)
}

/// A media buffer wrapping `texture`, for a sample the encoder reads straight from the GPU.
fn texture_buffer(texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
    mfplat::dxgi_surface_buffer(texture)
}

fn hardware_encoders(codec: &GUID) -> Result<Vec<Activation>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: *codec };
        let (mut pointer, mut count) = (std::ptr::null_mut(), 0);
        mfplat::enum_ex(
            MFT_CATEGORY_VIDEO_ENCODER,
            (MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER).0 as u32,
            &input,
            &output,
            &mut pointer,
            &mut count,
        )
        .context("enumerating hardware H.264 transforms")?;
        if pointer.is_null() {
            return Ok(Vec::new());
        }
        let list = std::slice::from_raw_parts_mut(pointer, count as usize);
        let result = list.iter_mut().filter_map(Option::take).map(Activation).collect();
        CoTaskMemFree(Some(pointer.cast()));
        Ok(result)
    }
}

fn attribute_string(attributes: &IMFAttributes, key: &GUID) -> Result<String> {
    unsafe {
        let size = attributes.GetStringLength(key)?;
        ensure!(size <= 16384, "oversized MFT registration string");
        let mut value = vec![0; size as usize + 1];
        attributes.GetString(key, &mut value, None)?;
        Ok(String::from_utf16_lossy(&value[..size as usize]))
    }
}

fn set_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> Result<()> {
    set_value(codec, key, &VARIANT::from(value))
        .with_context(|| format!("setting hardware codec property {key:?} to {value}"))
}

fn supports(codec: &ICodecAPI, key: &GUID) -> Result<bool> {
    // The generated Result<()> wrapper accepts S_FALSE, which here means unsupported.
    unsafe {
        let status = (codec.vtable().IsSupported)(codec.as_raw(), key);
        if status == S_FALSE || status == E_NOTIMPL {
            return Ok(false);
        }
        ensure!(status == S_OK, "querying hardware codec property {key:?}: {status}");
        Ok(true)
    }
}

fn set_value(codec: &ICodecAPI, key: &GUID, value: &VARIANT) -> Result<()> {
    // SetValue also uses S_FALSE to report a read-only setting. Preserve that distinction.
    unsafe {
        let status = (codec.vtable().SetValue)(codec.as_raw(), key, value);
        ensure!(status == S_OK, "hardware codec did not accept property {key:?}: {status}");
        Ok(())
    }
}

fn pack_pair(first: u32, second: u32) -> u64 {
    (u64::from(first) << 32) | u64::from(second)
}

fn video_type(width: u32, height: u32, fps: u32, subtype: &GUID) -> Result<IMFMediaType> {
    unsafe {
        let media_type = mfplat::media_type()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(media_type)
    }
}

fn read_buffer(buffer: &IMFMediaBuffer) -> Result<Vec<u8>> {
    unsafe {
        let (mut pointer, mut length) = (std::ptr::null_mut(), 0);
        buffer.Lock(&mut pointer, None, Some(&mut length))?;
        let data = if length as usize <= MAX_BITSTREAM && !pointer.is_null() {
            Ok(std::slice::from_raw_parts(pointer, length as usize).to_vec())
        } else {
            Err(anyhow::anyhow!("invalid hardware bitstream buffer length {length}"))
        };
        buffer.Unlock()?;
        data
    }
}

/// The few Media Foundation functions used here, looked up when first needed rather than linked:
/// Windows "N" editions come without Media Foundation, and a hard link would stop Connect from
/// starting there at all. Without it, hosting simply encodes with OpenH264.
mod mfplat {
    use std::{ffi::c_void, ptr::null_mut, sync::OnceLock};

    use windows::core::BOOL;

    use anyhow::{Result, anyhow, ensure};
    use windows::{
        Win32::{
            Graphics::Direct3D11::ID3D11Texture2D,
            Media::MediaFoundation::{
                IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, MFT_REGISTER_TYPE_INFO,
            },
            System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        },
        core::{GUID, HRESULT, Interface, s, w},
    };

    type Out = *mut *mut c_void;

    struct Api {
        startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
        shutdown: unsafe extern "system" fn() -> HRESULT,
        enum_ex: unsafe extern "system" fn(
            GUID,
            u32,
            *const MFT_REGISTER_TYPE_INFO,
            *const MFT_REGISTER_TYPE_INFO,
            *mut *mut Option<IMFActivate>,
            *mut u32,
        ) -> HRESULT,
        memory_buffer: unsafe extern "system" fn(u32, Out) -> HRESULT,
        aligned_memory_buffer: unsafe extern "system" fn(u32, u32, Out) -> HRESULT,
        sample: unsafe extern "system" fn(Out) -> HRESULT,
        media_type: unsafe extern "system" fn(Out) -> HRESULT,
        dxgi_device_manager: unsafe extern "system" fn(*mut u32, Out) -> HRESULT,
        dxgi_surface_buffer: unsafe extern "system" fn(*const GUID, *mut c_void, u32, BOOL, Out) -> HRESULT,
    }

    fn api() -> Result<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe { load() })
            .as_ref()
            .ok_or_else(|| anyhow!("Media Foundation isn't installed (Windows N editions need the Media Feature Pack)"))
    }

    /// SAFETY: each pointer is cast to the function's documented signature, spelled out in `Api`.
    #[expect(clippy::missing_transmute_annotations, reason = "the target types are Api's fields")]
    unsafe fn load() -> Option<Api> {
        let module = unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
        macro_rules! get {
            ($name:literal) => {
                unsafe { std::mem::transmute(GetProcAddress(module, s!($name))?) }
            };
        }
        Some(Api {
            startup: get!("MFStartup"),
            shutdown: get!("MFShutdown"),
            enum_ex: get!("MFTEnumEx"),
            memory_buffer: get!("MFCreateMemoryBuffer"),
            aligned_memory_buffer: get!("MFCreateAlignedMemoryBuffer"),
            sample: get!("MFCreateSample"),
            media_type: get!("MFCreateMediaType"),
            dxgi_device_manager: get!("MFCreateDXGIDeviceManager"),
            dxgi_surface_buffer: get!("MFCreateDXGISurfaceBuffer"),
        })
    }

    /// An object a creation function handed back through `make`.
    fn created<T: Interface>(make: impl FnOnce(Out) -> HRESULT) -> Result<T> {
        let mut raw = null_mut();
        make(&mut raw).ok()?;
        ensure!(!raw.is_null(), "Media Foundation returned no object");
        // SAFETY: a successful creation call hands over one reference to a `T`.
        Ok(unsafe { T::from_raw(raw) })
    }

    pub unsafe fn startup(version: u32, flags: u32) -> Result<()> {
        Ok(unsafe { (api()?.startup)(version, flags) }.ok()?)
    }

    pub unsafe fn shutdown() {
        if let Ok(api) = api() {
            let _ = unsafe { (api.shutdown)() };
        }
    }

    pub unsafe fn enum_ex(
        category: GUID,
        flags: u32,
        input: &MFT_REGISTER_TYPE_INFO,
        output: &MFT_REGISTER_TYPE_INFO,
        activates: *mut *mut Option<IMFActivate>,
        count: &mut u32,
    ) -> Result<()> {
        Ok(unsafe { (api()?.enum_ex)(category, flags, input, output, activates, count) }.ok()?)
    }

    pub fn memory_buffer(len: u32) -> Result<IMFMediaBuffer> {
        let api = api()?;
        created(|out| unsafe { (api.memory_buffer)(len, out) })
    }

    pub fn aligned_memory_buffer(len: u32, alignment: u32) -> Result<IMFMediaBuffer> {
        let api = api()?;
        created(|out| unsafe { (api.aligned_memory_buffer)(len, alignment, out) })
    }

    pub fn sample() -> Result<IMFSample> {
        let api = api()?;
        created(|out| unsafe { (api.sample)(out) })
    }

    pub fn media_type() -> Result<IMFMediaType> {
        let api = api()?;
        created(|out| unsafe { (api.media_type)(out) })
    }

    /// A new device manager and the token for handing it a device.
    pub fn dxgi_device_manager() -> Result<(u32, IMFDXGIDeviceManager)> {
        let api = api()?;
        let mut token = 0;
        let manager = created(|out| unsafe { (api.dxgi_device_manager)(&mut token, out) })?;
        Ok((token, manager))
    }

    pub fn dxgi_surface_buffer(texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        let api = api()?;
        created(|out| unsafe {
            (api.dxgi_surface_buffer)(&ID3D11Texture2D::IID, texture.as_raw(), 0, false.into(), out)
        })
    }
}

/// Makes each codec's encoder output self-contained per keyframe.
enum Bitstream {
    H264(H264Bitstream),
    H265(super::hevc::HevcBitstream),
}

impl Bitstream {
    fn new(codec: Codec) -> Self {
        match codec {
            Codec::H264 => Self::H264(H264Bitstream::default()),
            _ => Self::H265(Default::default()),
        }
    }

    fn sequence_header(&mut self, data: &[u8]) -> Result<()> {
        match self {
            Self::H264(stream) => stream.sequence_header(data),
            Self::H265(stream) => stream.sequence_header(data),
        }
    }

    fn frame(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool)> {
        match self {
            Self::H264(stream) => stream.frame(data),
            Self::H265(stream) => stream.frame(data),
        }
    }
}

#[derive(Default)]
struct H264Bitstream {
    sps: Vec<u8>,
    pps: Vec<u8>,
    length_size: Option<usize>,
}

impl H264Bitstream {
    fn sequence_header(&mut self, data: &[u8]) -> Result<()> {
        if data.first() != Some(&1) {
            let units = nal_units(data, self.length_size)?;
            self.remember(&units);
            return Ok(());
        }
        // AVCDecoderConfigurationRecord: parameter sets use 16-bit lengths regardless of the
        // separately encoded sample length size. Do not confuse it with an Annex-B header.
        ensure!(data.len() >= 7, "truncated AVC configuration");
        let length_size = usize::from(data[4] & 3) + 1;
        ensure!(length_size != 3, "reserved AVC NAL length size");
        let mut offset = 6;
        let mut units = Vec::new();
        for _ in 0..data[5] & 31 {
            units.push(take_nal(data, &mut offset, 2)?);
        }
        let count = *data.get(offset).context("missing AVC PPS count")?;
        offset += 1;
        for _ in 0..count {
            units.push(take_nal(data, &mut offset, 2)?);
        }
        self.remember(&units);
        self.length_size = Some(length_size);
        Ok(())
    }

    fn remember(&mut self, units: &[&[u8]]) {
        for unit in units {
            match unit.first().map(|b| b & 31) {
                Some(7) => self.sps = unit.to_vec(),
                Some(8) => self.pps = unit.to_vec(),
                _ => {}
            }
        }
    }

    fn frame(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool)> {
        ensure!(!data.is_empty() && data.len() <= MAX_BITSTREAM, "empty or oversized H.264 frame");
        let units = nal_units(data, self.length_size)?;
        self.remember(&units);
        ensure!(
            self.sps.get(1).is_none_or(|profile| *profile == 66),
            "hardware bitstream is not baseline H.264 (cannot guarantee no B frames)"
        );
        let keyframe = units.iter().any(|unit| unit[0] & 31 == 5);
        ensure!(units.iter().any(|unit| matches!(unit[0] & 31, 1 | 5)), "hardware output contains no video slice");
        let mut result = Vec::with_capacity(data.len() + self.sps.len() + self.pps.len() + 16);
        if keyframe {
            ensure!(!self.sps.is_empty() && !self.pps.is_empty(), "hardware IDR has no SPS/PPS for decoder recovery");
            // An access-unit delimiter, when present, precedes the parameter sets.
            for unit in units.iter().filter(|unit| unit[0] & 31 == 9) {
                result.extend_from_slice(&[0, 0, 0, 1]);
                result.extend_from_slice(unit);
            }
            for unit in [&self.sps, &self.pps] {
                result.extend_from_slice(&[0, 0, 0, 1]);
                result.extend_from_slice(unit);
            }
        }
        for unit in units {
            // Cached parameter sets are placed together before every IDR, exactly once.
            if keyframe && matches!(unit[0] & 31, 7..=9) {
                continue;
            }
            result.extend_from_slice(&[0, 0, 0, 1]);
            result.extend_from_slice(unit);
        }
        ensure!(result.len() <= MAX_BITSTREAM, "H.264 frame with parameter sets exceeds limit");
        Ok((result, keyframe))
    }
}

fn take_nal<'a>(data: &'a [u8], offset: &mut usize, length_size: usize) -> Result<&'a [u8]> {
    let length = data
        .get(*offset..*offset + length_size)
        .context("truncated H.264 NAL length")?
        .iter()
        .fold(0usize, |value, byte| (value << 8) | usize::from(*byte));
    *offset += length_size;
    ensure!(length > 0 && length <= MAX_BITSTREAM, "invalid H.264 NAL length");
    let end = offset.checked_add(length).context("H.264 NAL length overflow")?;
    let unit = data.get(*offset..end).context("truncated H.264 NAL payload")?;
    *offset = end;
    validate_nal(unit)?;
    Ok(unit)
}

fn validate_nal(unit: &[u8]) -> Result<()> {
    ensure!(!unit.is_empty() && unit[0] & 128 == 0 && (1..=23).contains(&(unit[0] & 31)), "invalid H.264 NAL header");
    Ok(())
}

fn nal_units(data: &[u8], length_size: Option<usize>) -> Result<Vec<&[u8]>> {
    // An AVCC length such as 0x00000100 resembles a start code. Prefer fully
    // validated AVCC when the sequence header advertised its length size.
    if let Some(length_size) = length_size {
        let parse = || -> Result<Vec<&[u8]>> {
            let mut offset = 0;
            let mut units = Vec::new();
            while offset < data.len() {
                units.push(take_nal(data, &mut offset, length_size)?);
            }
            ensure!(!units.is_empty(), "empty AVCC access unit");
            Ok(units)
        };
        if let Ok(units) = parse() {
            return Ok(units);
        }
    }
    let start_code = |offset: usize| {
        if data.get(offset..offset + 4) == Some(&[0, 0, 0, 1]) {
            4
        } else if data.get(offset..offset + 3) == Some(&[0, 0, 1]) {
            3
        } else {
            0
        }
    };
    let mut units = Vec::new();
    if start_code(0) > 0 {
        let mut offset = 0;
        while offset < data.len() {
            let begin = offset + start_code(offset);
            let mut end = begin;
            while end < data.len() && start_code(end) == 0 {
                end += 1;
            }
            let mut payload_end = end;
            while payload_end > begin && data[payload_end - 1] == 0 {
                payload_end -= 1;
            }
            let unit = &data[begin..payload_end];
            validate_nal(unit)?;
            units.push(unit);
            offset = end;
        }
    } else {
        let mut offset = 0;
        while offset < data.len() {
            units.push(take_nal(data, &mut offset, length_size.unwrap_or(4))?);
        }
    }
    ensure!(!units.is_empty(), "empty H.264 access unit");
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yuv::Picture;
    use openh264::{decoder::Decoder, formats::YUVSource as _};

    #[test]
    fn bitstream_annex_b_and_avcc_recover_parameter_sets() {
        let mut stream = H264Bitstream::default();
        stream.sequence_header(&[1, 66, 0, 31, 0xff, 0xe1, 0, 3, 0x67, 66, 31, 1, 0, 2, 0x68, 42]).unwrap();
        let (frame, keyframe) = stream.frame(&[0, 0, 0, 2, 0x65, 42]).unwrap();
        assert!(keyframe);
        assert_eq!(frame, [0, 0, 0, 1, 0x67, 66, 31, 0, 0, 0, 1, 0x68, 42, 0, 0, 0, 1, 0x65, 42]);
        let (frame, keyframe) = stream.frame(&[0, 0, 1, 0x41, 42]).unwrap();
        assert!(!keyframe);
        assert_eq!(frame, [0, 0, 0, 1, 0x41, 42]);
        let (frame, keyframe) = stream.frame(&[0, 0, 0, 1, 0x65, 43]).unwrap();
        assert!(keyframe);
        assert_eq!(nal_units(&frame, None).unwrap().iter().map(|n| n[0] & 31).collect::<Vec<_>>(), [7, 8, 5]);
    }

    #[test]
    fn bitstream_rejects_truncation_and_idr_without_parameter_sets() {
        for bytes in [&[][..], &[0, 0, 0, 0], &[0, 0, 0, 8, 0x41], &[0, 0, 1], &[0, 0, 1, 0xe5], &[0, 0, 1, 0x65, 42]] {
            assert!(H264Bitstream::default().frame(bytes).is_err(), "accepted {bytes:?}");
        }
        for bytes in [&[1][..], &[1, 66, 0, 31, 0xff, 0xe1, 0, 50], &[1, 66, 0, 31, 0xfe, 0xe0, 0]] {
            assert!(H264Bitstream::default().sequence_header(bytes).is_err());
        }
    }

    #[test]
    fn bitstream_preserves_aud_order_and_ambiguous_avcc_lengths() {
        let mut stream = H264Bitstream::default();
        let (frame, keyframe) = stream
            .frame(&[0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 66, 31, 0, 0, 1, 0x68, 42, 0, 0, 1, 0x65, 42])
            .unwrap();
        assert!(keyframe);
        assert_eq!(nal_units(&frame, None).unwrap().iter().map(|n| n[0] & 31).collect::<Vec<_>>(), [9, 7, 8, 5]);
        for length_size in [1, 2, 4] {
            let unit = [0x41, 42];
            let mut avcc = vec![0; length_size];
            avcc[length_size - 1] = unit.len() as u8;
            avcc.extend_from_slice(&unit);
            assert_eq!(nal_units(&avcc, Some(length_size)).unwrap(), [&unit[..]]);
        }
        let mut avcc = vec![0, 0, 1, 0, 0x41];
        avcc.extend(std::iter::repeat_n(42, 255));
        assert_eq!(nal_units(&avcc, Some(4)).unwrap()[0], &avcc[4..]);
    }

    fn moving_image(width: usize, height: usize, frame: usize) -> Picture {
        let mut pixels = vec![0; width * height * 4];
        for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (index % width + frame * 7, index / width + frame * 3);
            pixel.copy_from_slice(&[(x ^ y) as u8, (y * 3) as u8, (x * 5) as u8, 255]);
        }
        let mut picture = Picture::new(Layout::Nv12, width, height);
        picture.convert_from_bgra(&pixels, width * 4);
        picture
    }

    /// Real GPU encode/decode and resize/recovery check, using synthetic pixels only.
    #[test]
    #[ignore = "requires a local Windows hardware H.264 MFT"]
    fn native_hardware_encode_decode_and_resize() {
        for (width, height, fps) in [(640, 480, 60), (1920, 1080, 60), (800, 600, 60), (1920, 1080, 240)] {
            let mut encoder = HardwareEncoder::new(Codec::H264, width, height, 20_000_000, fps, None).unwrap();
            println!("{} {width}x{height} at {fps} fps ceiling", encoder.name());
            let mut decoder = Decoder::new().unwrap();
            let mut timings = Vec::new();
            for index in 0..20 {
                let image = moving_image(width as usize, height as usize, index);
                let timestamp = 1_798_500_000_000_000 + index as u64 * (1_000_000 / u64::from(fps));
                let start = Instant::now();
                let frames = encoder.encode_frame(Input::Picture(&image), timestamp, index == 10).unwrap();
                timings.push(start.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(frames.len(), 1);
                for frame in frames {
                    assert_eq!(frame.capture_time_us, timestamp);
                    let decoded = decoder.decode(&frame.data).unwrap().expect("no decoder frame buffering");
                    assert_eq!(decoded.dimensions(), (width as usize, height as usize));
                    if index == 0 || index == 10 {
                        assert!(frame.keyframe, "requested recovery must produce IDR");
                        let mut fresh = Decoder::new().unwrap();
                        assert_eq!(
                            fresh.decode(&frame.data).unwrap().unwrap().dimensions(),
                            (width as usize, height as usize)
                        );
                    }
                }
            }
            timings.sort_by(f64::total_cmp);
            println!("hardware encode median={:.2}ms p95={:.2}ms", timings[10], timings[18]);
        }
    }
}
