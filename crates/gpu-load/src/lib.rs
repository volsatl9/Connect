//! How busy this PC's graphics card is, by kind of work, for the session stats.
//!
//! Windows reports every GPU engine's utilisation through performance counters, whatever the
//! vendor. On Linux only AMD's driver offers a comparable figure without vendor tools, and
//! only for the whole card.

use serde::{Deserialize, Serialize};

/// Percentages, 0–100, of the busiest engine of each kind across the graphics cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuLoad {
    /// Drawing and compute: games, the desktop, colour conversion.
    pub graphics: u8,
    /// The video encoder, where the system reports it separately.
    pub encode: Option<u8>,
    /// The video decoder, where the system reports it separately.
    pub decode: Option<u8>,
}

/// Samples [`GpuLoad`]. Each sample covers the time since the previous one, so take them at a
/// steady interval (around a second).
pub struct Monitor(imp::Monitor);

impl Monitor {
    /// `None` where this system doesn't report GPU load.
    pub fn new() -> Option<Self> {
        imp::Monitor::new().map(Self)
    }

    /// The load since the last sample; `None` for the first one, or if the reading failed.
    pub fn sample(&mut self) -> Option<GpuLoad> {
        self.0.sample()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Engine kind from a Windows "GPU Engine" counter instance, e.g.
/// `pid_1234_luid_0x0000_0x0001_phys_0_eng_3_engtype_VideoEncode`, and the engine it names
/// (instance name without the process) so per-process figures can be added up.
fn engine(instance: &str) -> Option<(&str, &str)> {
    let (_, kind) = instance.rsplit_once("engtype_")?;
    let start = instance.find("luid_")?;
    let end = instance.find("_engtype_")?;
    Some((&instance[start..end], kind))
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Folds per-process engine figures into the busiest engine of each kind.
fn fold<'a>(readings: impl IntoIterator<Item = (&'a str, f64)>) -> GpuLoad {
    let mut engines: std::collections::HashMap<(&str, &str), f64> = Default::default();
    for (instance, value) in readings {
        if let Some(key) = engine(instance) {
            *engines.entry(key).or_default() += value;
        }
    }
    let busiest = |pick: &dyn Fn(&str) -> bool| {
        engines
            .iter()
            .filter(|((_, kind), _)| pick(kind))
            .map(|(_, &v)| v)
            .fold(None, |max: Option<f64>, v| Some(max.map_or(v, |max| max.max(v))))
    };
    let percent = |v: f64| v.round().clamp(0.0, 100.0) as u8;
    GpuLoad {
        graphics: busiest(&|kind| kind == "3D" || kind.starts_with("Compute")).map_or(0, percent),
        encode: busiest(&|kind| kind == "VideoEncode").map(percent),
        decode: busiest(&|kind| kind == "VideoDecode").map(percent),
    }
}

#[cfg(windows)]
mod imp {
    use windows::{
        Win32::System::Performance::{
            PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
            PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        },
        core::{PCWSTR, w},
    };

    use super::GpuLoad;

    pub struct Monitor {
        query: PDH_HQUERY,
        counter: PDH_HCOUNTER,
        primed: bool,
    }

    // SAFETY: the query handle is only used through `&mut self`, one thread at a time.
    unsafe impl Send for Monitor {}

    impl Monitor {
        pub fn new() -> Option<Self> {
            unsafe {
                let mut query = PDH_HQUERY::default();
                if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                    return None;
                }
                let mut counter = PDH_HCOUNTER::default();
                if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                    PdhCloseQuery(query);
                    return None;
                }
                Some(Self { query, counter, primed: false })
            }
        }

        pub fn sample(&mut self) -> Option<GpuLoad> {
            unsafe {
                if PdhCollectQueryData(self.query) != 0 {
                    return None;
                }
                // Utilisation is a rate: the first collection only sets the starting point.
                if !std::mem::replace(&mut self.primed, true) {
                    return None;
                }
                let (mut bytes, mut count) = (0u32, 0u32);
                let status = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None);
                if status != PDH_MORE_DATA || bytes == 0 {
                    return None;
                }
                // The items, then the instance names they point into, in one buffer.
                let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
                let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
                if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, Some(items)) != 0
                {
                    return None;
                }
                let items = std::slice::from_raw_parts(items, count as usize);
                let readings: Vec<(String, f64)> = items
                    .iter()
                    .filter(|item| item.FmtValue.CStatus == 0)
                    .filter_map(|item| Some((item.szName.to_string().ok()?, item.FmtValue.Anonymous.doubleValue)))
                    .collect();
                Some(super::fold(readings.iter().map(|(name, value)| (name.as_str(), *value))))
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.query) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    use super::GpuLoad;

    /// AMD's `gpu_busy_percent`, for each card that has it.
    pub struct Monitor(Vec<PathBuf>);

    impl Monitor {
        pub fn new() -> Option<Self> {
            let cards: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
                .ok()?
                .flatten()
                .map(|entry| entry.path().join("device/gpu_busy_percent"))
                .filter(|path| path.exists())
                .collect();
            (!cards.is_empty()).then_some(Self(cards))
        }

        pub fn sample(&mut self) -> Option<GpuLoad> {
            let busiest =
                self.0.iter().filter_map(|path| std::fs::read_to_string(path).ok()?.trim().parse::<u8>().ok()).max()?;
            Some(GpuLoad { graphics: busiest.min(100), encode: None, decode: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_up_processes_per_engine_and_keeps_the_busiest_engine_of_each_kind() {
        let luid = "luid_0x00000000_0x0000D1A2_phys_0";
        let readings = [
            (format!("pid_10_{luid}_eng_0_engtype_3D"), 30.0),
            (format!("pid_11_{luid}_eng_0_engtype_3D"), 25.4),
            (format!("pid_10_{luid}_eng_1_engtype_Compute_0"), 12.0),
            (format!("pid_12_{luid}_eng_5_engtype_VideoEncode"), 7.6),
            (format!("pid_12_{luid}_eng_6_engtype_Copy"), 90.0),
            ("garbage".to_owned(), 99.0),
        ];
        let load = fold(readings.iter().map(|(name, value)| (name.as_str(), *value)));
        assert_eq!(load, GpuLoad { graphics: 55, encode: Some(8), decode: None });
        assert_eq!(fold([]), GpuLoad::default());
    }
}
