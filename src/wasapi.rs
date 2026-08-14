//! Capturing on Windows, through WASAPI.
//!
//! `cpal` cannot record an output here, and not for want of trying: capturing
//! what a device *plays* is not opening it for input, it is initialising an
//! audio client on the render endpoint with `AUDCLNT_STREAMFLAGS_LOOPBACK`.
//! That flag has no equivalent in a portable audio API, so this module talks to
//! WASAPI directly — the same thing the PipeWire backend does on Linux, and for
//! the same reason.
//!
//! Everything happens on one thread. COM objects belong to the apartment that
//! created them, so the client is built, run and dropped where it lives; only
//! the samples cross over, on a channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::core::PCWSTR;

/// How long to wait for the client to report the shape of its stream.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);

/// The buffer WASAPI is asked for, in hundred-nanosecond units: 200 ms, which
/// is room enough that a busy machine does not drop frames between polls.
const BUFFER_DURATION: i64 = 2_000_000;

/// How long to sleep when a poll finds nothing. A tenth of the buffer keeps the
/// thread cheap without letting the buffer fill.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// An audio endpoint, as WASAPI describes it.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Name as the system shows it, "Speakers (Realtek Audio)" and the like.
    pub name: String,
    /// The endpoint id, which survives renaming and is what reopens it.
    pub id: String,
    /// True for a render endpoint, captured in loopback.
    pub is_output: bool,
    /// True for the endpoint the system plays through by default.
    pub is_default: bool,
}

/// A running capture. Dropping it stops the thread.
pub struct Tap {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A capture in progress, as the shape of its stream plus where samples land.
pub struct Tapped {
    /// Interleaved channel count of the blocks.
    pub channels: u32,
    /// Frames per second of the blocks.
    pub sample_rate: u32,
    /// Blocks of interleaved `f32`.
    pub blocks: Receiver<Vec<f32>>,
    /// Keeps the capture thread alive; dropping it stops the tap.
    pub tap: Tap,
}

/// Every active endpoint, outputs first.
pub fn endpoints() -> Result<Vec<Endpoint>> {
    let _com = Apartment::enter()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

        let mut found = Vec::new();
        for (flow, is_output) in [(eRender, true), (eCapture, false)] {
            let default = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .ok()
                .and_then(|device| device_id(&device).ok());

            let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            for index in 0..collection.GetCount()? {
                let device = collection.Item(index)?;
                let id = device_id(&device)?;
                found.push(Endpoint {
                    name: friendly_name(&device).unwrap_or_else(|_| id.clone()),
                    is_default: default.as_deref() == Some(id.as_str()),
                    id,
                    is_output,
                });
            }
        }
        Ok(found)
    }
}

/// Start capturing an endpoint: an output in loopback, an input as itself.
pub fn open(id: &str, loopback: bool) -> Result<Tapped> {
    let (blocks_tx, blocks) = channel();
    // Rendezvous with the capture thread: it reports the negotiated shape, or
    // why it never got one. Bounded at one, since it is sent exactly once.
    let (shape_tx, shape_rx) = sync_channel::<Result<(u32, u32), String>>(1);
    let stop = Arc::new(AtomicBool::new(false));

    let target = id.to_string();
    let failure = shape_tx.clone();
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("odm-wasapi".to_string())
        .spawn(move || {
            if let Err(e) = run(&target, loopback, blocks_tx, shape_tx, &thread_stop) {
                let _ = failure.try_send(Err(format!("{e:#}")));
            }
        })?;

    let tap = Tap {
        stop,
        thread: Some(thread),
    };

    match shape_rx.recv_timeout(NEGOTIATION_TIMEOUT) {
        Ok(Ok((channels, sample_rate))) => Ok(Tapped {
            channels,
            sample_rate,
            blocks,
            tap,
        }),
        Ok(Err(e)) => Err(anyhow!(e)),
        // Dropping the tap here stops the thread that never got anywhere.
        Err(_) => bail!("WASAPI did not answer within five seconds"),
    }
}

/// The capture thread: initialise the client, then pump it until told to stop.
fn run(
    id: &str,
    loopback: bool,
    blocks: Sender<Vec<f32>>,
    shape: SyncSender<Result<(u32, u32), String>>,
    stop: &AtomicBool,
) -> Result<()> {
    let _com = Apartment::enter()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        let device = enumerator.GetDevice(PCWSTR(wide.as_ptr()))?;

        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let shaped = Shape::read(format)?;

        // The loopback flag is the whole point of going native: it turns a
        // render endpoint into something that can be recorded, which no
        // portable API asks for.
        let flags = if loopback {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION,
            0,
            format,
            None,
        )?;
        CoTaskMemFree(Some(format as *const _));

        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let _ = shape.try_send(Ok((shaped.channels, shaped.sample_rate)));

        while !stop.load(Ordering::Relaxed) {
            let waiting = capture.GetNextPacketSize()?;
            if waiting == 0 {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut buffer_flags = 0u32;
            capture.GetBuffer(&mut data, &mut frames, &mut buffer_flags, None, None)?;

            let samples = frames as usize * shaped.channels as usize;
            let block = if buffer_flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                // WASAPI is allowed to hand over a silent packet without
                // writing it out; the buffer's contents are then meaningless.
                vec![0.0; samples]
            } else {
                shaped.to_f32(data, samples)
            };
            capture.ReleaseBuffer(frames)?;

            if !block.is_empty() && blocks.send(block).is_err() {
                break;
            }
        }

        client.Stop()?;
        Ok(())
    }
}

/// The sample layout a client negotiated, and how to read it.
struct Shape {
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
}

/// What the samples are, among the formats a shared-mode mix can take.
enum SampleFormat {
    F32,
    I16,
    I32,
}

impl Shape {
    /// Read a `WAVEFORMATEX`, following it into its extensible form when the
    /// tag says to — which a modern mix format always does.
    unsafe fn read(format: *const WAVEFORMATEX) -> Result<Self> {
        const WAVE_FORMAT_PCM: u16 = 1;
        const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
        const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
        // The two subformats a shared-mode mix ever uses, as raw GUID data.
        const SUBTYPE_PCM: u128 = 0x00000001_0000_0010_8000_00aa00389b71;
        const SUBTYPE_FLOAT: u128 = 0x00000003_0000_0010_8000_00aa00389b71;

        let base = unsafe { &*format };
        let (tag, bits) = match base.wFormatTag {
            WAVE_FORMAT_EXTENSIBLE => {
                let extended = unsafe { &*(format as *const WAVEFORMATEXTENSIBLE) };
                let sub = extended.SubFormat;
                let as_u128 = (sub.data1 as u128) << 96
                    | (sub.data2 as u128) << 80
                    | (sub.data3 as u128) << 64
                    | u128::from_be_bytes({
                        let mut bytes = [0u8; 16];
                        bytes[8..].copy_from_slice(&sub.data4);
                        bytes
                    });
                let tag = match as_u128 {
                    SUBTYPE_FLOAT => WAVE_FORMAT_IEEE_FLOAT,
                    SUBTYPE_PCM => WAVE_FORMAT_PCM,
                    _ => bail!("this endpoint mixes in a format this meter cannot read"),
                };
                (tag, base.wBitsPerSample)
            }
            tag => (tag, base.wBitsPerSample),
        };

        let format = match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFormat::F32,
            (WAVE_FORMAT_PCM, 16) => SampleFormat::I16,
            (WAVE_FORMAT_PCM, 32) => SampleFormat::I32,
            (tag, bits) => bail!("unsupported mix format: tag {tag}, {bits} bits per sample"),
        };

        Ok(Self {
            channels: base.nChannels as u32,
            sample_rate: base.nSamplesPerSec,
            format,
        })
    }

    /// Turn one WASAPI packet into interleaved `f32`.
    unsafe fn to_f32(&self, data: *const u8, samples: usize) -> Vec<f32> {
        match self.format {
            SampleFormat::F32 => {
                unsafe { std::slice::from_raw_parts(data as *const f32, samples) }.to_vec()
            }
            SampleFormat::I16 => unsafe { std::slice::from_raw_parts(data as *const i16, samples) }
                .iter()
                .map(|s| *s as f32 / 32768.0)
                .collect(),
            SampleFormat::I32 => unsafe { std::slice::from_raw_parts(data as *const i32, samples) }
                .iter()
                .map(|s| *s as f32 / 2_147_483_648.0)
                .collect(),
        }
    }
}

/// A COM apartment held for as long as the work needs it.
///
/// Every thread touching these objects has to enter one, and leave it after the
/// objects are gone — hence a guard rather than a pair of calls to remember.
struct Apartment;

impl Apartment {
    fn enter() -> Result<Self> {
        // A thread already in an apartment answers RPC_E_CHANGED_MODE, which is
        // not a failure: it is already in one, which is all that was wanted.
        let entered = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if entered.is_err() && entered != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            bail!("COM would not start on this thread: {entered:?}");
        }
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// The endpoint id, which is what reopens a device later.
unsafe fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let id = device.GetId()?;
        let text = id.to_string()?;
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(text)
    }
}

/// The name the system shows for a device.
unsafe fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let value = store.GetValue(&PKEY_Device_FriendlyName)?;
        Ok(value.to_string())
    }
}
