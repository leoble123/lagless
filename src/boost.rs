//! OS-level latency tweaks: scheduler priority, timer resolution, and on Windows the
//! low-latency audio engine trick.

/// Process-wide: on Windows, 1 ms timer resolution (default is 15.6 ms, which makes every
/// short sleep/timeout coarse) and high priority class.
pub fn process() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Media::timeBeginPeriod;
        use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
        timeBeginPeriod(1);
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}

/// For the current thread: Windows MMCSS "Pro Audio" class; macOS user-interactive QoS.
/// Applied to the threads that move audio between the sound card and the network.
pub fn thread() {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut idx = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx);
    }
    #[cfg(target_os = "macos")]
    unsafe {
        // <pthread/qos.h>; QOS_CLASS_USER_INTERACTIVE = 0x21.
        extern "C" {
            fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
        }
        pthread_set_qos_class_self_np(0x21, 0);
    }
}

/// Holds the Windows audio engine at its minimum period while alive. See `low_latency_engine`.
pub struct EngineBoost {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EngineBoost {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// What the engine trick achieved, in frames at `rate`.
#[derive(Clone, Copy, Debug)]
pub struct EnginePeriod {
    pub default_frames: u32,
    pub active_frames: u32,
    pub rate: u32,
}

impl EnginePeriod {
    pub fn ms(frames: u32, rate: u32) -> f32 {
        frames as f32 * 1000.0 / rate.max(1) as f32
    }
}

/// The big one. Windows hands loopback audio to us once per audio-engine period, 10 ms by
/// default. But the shared-mode engine runs each endpoint at the *smallest* period any client
/// asked for through IAudioClient3 (Windows 10+, if the driver allows it, e.g. the inbox HD
/// Audio driver offers 128 frames = 2.67 ms at 48 kHz). So we open a silent render stream at the
/// minimum period on the device we capture: the whole engine speeds up, and loopback packets
/// arrive 3-4x more often. Returns None (and changes nothing) where the driver can't go lower.
#[cfg(windows)]
pub fn low_latency_engine(device_name: Option<String>) -> Option<(EngineBoost, EnginePeriod)> {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::{mpsc, Arc};

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let s = stop.clone();
    let handle = std::thread::Builder::new()
        .name("engine-boost".into())
        .spawn(move || {
            thread();
            if let Err(e) = unsafe { hold_min_period(device_name, &s, &tx) } {
                let _ = tx.send(Err(e.to_string()));
            }
        })
        .ok()?;
    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok(p)) => Some((EngineBoost { stop, handle: Some(handle) }, p)),
        _ => {
            stop.store(true, Relaxed);
            let _ = handle.join();
            None
        }
    }
}

#[cfg(not(windows))]
pub fn low_latency_engine(_device_name: Option<String>) -> Option<(EngineBoost, EnginePeriod)> {
    None
}

#[cfg(windows)]
unsafe fn hold_min_period(
    device_name: Option<String>,
    stop: &std::sync::atomic::AtomicBool,
    tx: &std::sync::mpsc::Sender<Result<EnginePeriod, String>>,
) -> windows::core::Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::core::PCWSTR;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    // Same device cpal captures from: the default output, or the one whose friendly name matches.
    let device = match device_name {
        None => enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?,
        Some(want) => {
            let all = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut found = None;
            for i in 0..all.GetCount()? {
                let d = all.Item(i)?;
                let name = d
                    .OpenPropertyStore(STGM_READ)
                    .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                    .and_then(|v| PropVariantToStringAlloc(&v))
                    .map(|p| {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as _));
                        s
                    })
                    .unwrap_or_default();
                if name == want {
                    found = Some(d);
                    break;
                }
            }
            match found {
                Some(d) => d,
                None => return Err(windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG)),
            }
        }
    };

    let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
    let fmt = client.GetMixFormat()?;
    let rate = (*fmt).nSamplesPerSec;
    let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    client.GetSharedModeEnginePeriod(fmt, &mut def, &mut fund, &mut min, &mut max)?;
    if min == 0 || min >= def {
        CoTaskMemFree(Some(fmt as _));
        let _ = tx.send(Err("driver offers no lower period".into()));
        return Ok(());
    }
    client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, fmt, None)?;
    CoTaskMemFree(Some(fmt as _));
    let event = CreateEventW(None, false, false, PCWSTR::null())?;
    client.SetEventHandle(event)?;
    let render: IAudioRenderClient = client.GetService()?;
    let size = client.GetBufferSize()?;
    client.Start()?;
    let _ = tx.send(Ok(EnginePeriod { default_frames: def, active_frames: min, rate }));

    // Keep the stream fed with silence (flagged silent, so it's free) for as long as we stream.
    while !stop.load(Relaxed) {
        WaitForSingleObject(event, 100);
        let pad = client.GetCurrentPadding().unwrap_or(size);
        let n = size.saturating_sub(pad);
        if n > 0 && render.GetBuffer(n).is_ok() {
            let _ = render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
        }
    }
    let _ = client.Stop();
    let _ = CloseHandle(event);
    Ok(())
}
