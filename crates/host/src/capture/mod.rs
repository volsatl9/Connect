//! Screen capture sources. Capturers are created and used on the video thread only.

#[cfg(windows)]
pub(crate) mod dxgi;
mod test_pattern;

use std::time::Duration;

use anyhow::Result;

/// Where the host's video comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VideoSource {
    /// The primary monitor. Windows only: elsewhere every connection is turned away.
    #[default]
    Screen,
    /// A generated moving pattern, for testing without a display.
    TestPattern { width: u32, height: u32, fps: u32 },
}

impl VideoSource {
    pub const TEST_PATTERN: VideoSource = VideoSource::TestPattern { width: 1280, height: 720, fps: 60 };

    /// Whether this build can capture the real screen, and so can host. Other platforms can
    /// only connect to a host.
    pub const SCREEN_SUPPORTED: bool = cfg!(windows);

    pub(crate) fn open(&self) -> Result<Box<dyn Capturer>> {
        match *self {
            #[cfg(windows)]
            VideoSource::Screen => Ok(Box::new(dxgi::DxgiCapturer::new()?)),
            #[cfg(not(windows))]
            VideoSource::Screen => anyhow::bail!("screen capture is only supported on Windows hosts"),
            VideoSource::TestPattern { width, height, fps } => {
                Ok(Box::new(test_pattern::TestPattern::new(width, height, fps)))
            }
        }
    }
}

/// One captured image in BGRA, with rows `stride` bytes apart.
pub(crate) struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bgra: &'a [u8],
}

pub(crate) trait Capturer {
    /// Waits up to `timeout` for the screen to change; `Ok(None)` if it didn't.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>>;

    /// How many times a second the source can change, if known.
    fn refresh_hz(&self) -> Option<u32> {
        None
    }

    /// Desktop Duplication, which can also hand frames over on the graphics card.
    #[cfg(windows)]
    fn as_dxgi(&mut self) -> Option<&mut dxgi::DxgiCapturer> {
        None
    }
}
