//! cpal glue: device lookup and sample-format-agnostic streams that speak f32.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

pub fn find_device(name: Option<&str>, output: bool) -> Result<Device> {
    let host = cpal::default_host();
    match name {
        None => if output { host.default_output_device() } else { host.default_input_device() }
            .ok_or_else(|| anyhow!("no default {} device", if output { "output" } else { "input" })),
        Some(want) => {
            let want = want.to_lowercase();
            let devices = if output { host.output_devices()? } else { host.input_devices()? };
            for d in devices {
                if d.name().map(|n| n.to_lowercase().contains(&want)).unwrap_or(false) {
                    return Ok(d);
                }
            }
            Err(anyhow!("no {} device matching {want:?} (try `lagless devices`)", if output { "output" } else { "input" }))
        }
    }
}

pub fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let def_out = host.default_output_device().and_then(|d| d.name().ok());
    let def_in = host.default_input_device().and_then(|d| d.name().ok());
    println!("Audio host: {}", host.id().name());
    println!("\nOutput devices (playback on receiver, loopback source on sender):");
    for d in host.output_devices()? {
        let n = d.name().unwrap_or_default();
        let cfg = d.default_output_config().map(|c| format!("{} Hz, {} ch", c.sample_rate().0, c.channels())).unwrap_or_default();
        println!("  {} {n}  [{cfg}]", if Some(&n) == def_out.as_ref() { "*" } else { " " });
    }
    println!("\nInput devices (for --source input):");
    for d in host.input_devices()? {
        let n = d.name().unwrap_or_default();
        println!("  {} {n}", if Some(&n) == def_in.as_ref() { "*" } else { " " });
    }
    Ok(())
}

fn err_fn(e: cpal::StreamError) {
    eprintln!("\naudio stream error: {e}");
}

pub fn build_input(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    on_data: impl FnMut(&[f32]) + Send + 'static,
) -> Result<Stream> {
    fn build<T: SizedSample>(
        device: &Device,
        config: &StreamConfig,
        mut on_data: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Stream, cpal::BuildStreamError>
    where
        f32: FromSample<T>,
    {
        let mut buf: Vec<f32> = Vec::with_capacity(16384);
        device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                buf.clear();
                buf.extend(data.iter().map(|&s| f32::from_sample(s)));
                on_data(&buf);
            },
            err_fn,
            None,
        )
    }
    Ok(match format {
        SampleFormat::F32 => build::<f32>(device, config, on_data),
        SampleFormat::I16 => build::<i16>(device, config, on_data),
        SampleFormat::I32 => build::<i32>(device, config, on_data),
        SampleFormat::U16 => build::<u16>(device, config, on_data),
        SampleFormat::F64 => build::<f64>(device, config, on_data),
        f => return Err(anyhow!("unsupported capture sample format {f:?}")),
    }
    .context("could not open capture stream")?)
}

/// `render(out, device_latency_secs)` fills interleaved f32 samples.
pub fn build_output(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    render: impl FnMut(&mut [f32], Option<f64>) + Send + 'static,
) -> Result<Stream, cpal::BuildStreamError> {
    fn build<T: SizedSample + FromSample<f32>>(
        device: &Device,
        config: &StreamConfig,
        mut render: impl FnMut(&mut [f32], Option<f64>) + Send + 'static,
    ) -> Result<Stream, cpal::BuildStreamError> {
        let mut scratch = vec![0f32; 1 << 15];
        device.build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let lat = ts.playback.duration_since(&ts.callback).map(|d| d.as_secs_f64());
                if scratch.len() < data.len() {
                    scratch.resize(data.len(), 0.0);
                }
                let s = &mut scratch[..data.len()];
                render(s, lat);
                for (o, &i) in data.iter_mut().zip(s.iter()) {
                    *o = T::from_sample(i);
                }
            },
            err_fn,
            None,
        )
    }
    match format {
        SampleFormat::F32 => build::<f32>(device, config, render),
        SampleFormat::I16 => build::<i16>(device, config, render),
        SampleFormat::I32 => build::<i32>(device, config, render),
        SampleFormat::U16 => build::<u16>(device, config, render),
        SampleFormat::F64 => build::<f64>(device, config, render),
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}
