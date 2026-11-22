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
    #[cfg(target_os = "android")]
    unsafe {
        // THREAD_PRIORITY_AUDIO (-16): what Android's own audio threads use; allowed for apps.
        extern "C" {
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        setpriority(0, 0, -16);
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
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    // Same device cpal captures from: the default output, or the one whose friendly name matches.
    let device = render_device(&enumerator, device_name.as_deref())?;

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

/// The playback device lagless captures from: the default output, or the one whose friendly
/// name matches (the same name cpal shows).
#[cfg(windows)]
unsafe fn render_device(enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator, name: Option<&str>) -> windows::core::Result<windows::Win32::Media::Audio::IMMDevice> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::*;
    let Some(want) = name else { return enumerator.GetDefaultAudioEndpoint(eRender, eConsole) };
    let all = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    for i in 0..all.GetCount()? {
        let d = all.Item(i)?;
        let n = d
            .OpenPropertyStore(STGM_READ)
            .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
            .and_then(|v| PropVariantToStringAlloc(&v))
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as _));
                s
            })
            .unwrap_or_default();
        if n == want {
            return Ok(d);
        }
    }
    Err(windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))
}

/// The Windows volume of the device being captured, as a linear gain (0 when muted), kept up
/// to date by a small polling thread. Loopback capture hands us the mix *before* Windows applies
/// that volume, so without this the PC's volume dial/keys wouldn't affect what the Mac plays.
pub struct SystemVolume {
    gain: std::sync::Arc<std::sync::atomic::AtomicU32>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SystemVolume {
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(std::sync::atomic::Ordering::Relaxed))
    }
}

impl Drop for SystemVolume {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(windows)]
pub fn follow_system_volume(device_name: Option<String>) -> Option<SystemVolume> {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
    use std::sync::{mpsc, Arc};
    let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let stop = Arc::new(AtomicBool::new(false));
    let (g, s) = (gain.clone(), stop.clone());
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("system-volume".into())
        .spawn(move || unsafe {
            use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
            use windows::Win32::Media::Audio::*;
            use windows::Win32::System::Com::*;
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let vol: windows::core::Result<IAudioEndpointVolume> = (|| {
                let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                render_device(&enumerator, device_name.as_deref())?.Activate(CLSCTX_ALL, None)
            })();
            let Ok(vol) = vol else {
                let _ = tx.send(false);
                return;
            };
            let _ = tx.send(true);
            // Polling (20x a second) is simpler than a COM callback and costs nothing.
            while !s.load(Relaxed) {
                let muted = vol.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                let db = vol.GetMasterVolumeLevel().unwrap_or(0.0);
                let lin = if muted { 0.0 } else { 10f32.powf(db / 20.0).clamp(0.0, 1.0) };
                g.store(lin.to_bits(), Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .ok()?;
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(true) => Some(SystemVolume { gain, stop, handle: Some(handle) }),
        _ => {
            stop.store(true, Relaxed);
            let _ = handle.join();
            None
        }
    }
}

#[cfg(not(windows))]
pub fn follow_system_volume(_device_name: Option<String>) -> Option<SystemVolume> {
    None
}
