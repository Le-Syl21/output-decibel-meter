//! Capturing on macOS, through a Core Audio process tap.
//!
//! An output cannot be opened for input here either, and the way round is not a
//! flag but an object: macOS 14.4 added *process taps*, which hand over what a
//! set of programs is playing — or what everything is playing — as a device one
//! can record from. A tap on its own is not readable; it has to be wrapped in a
//! private aggregate device, which is what this module builds and tears down.
//!
//! Two shapes, and they are the two modes of this crate: everything the machine
//! plays, and one program by its process id. The second is what makes a program
//! able to meter itself here as it does on Linux.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Result, bail};
use objc2::rc::Retained;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress, CATapDescription,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyStreamFormat,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeInput, kAudioSubTapUIDKey,
};
use objc2_core_audio_types::AudioBufferList;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

/// What a tap should listen to.
#[derive(Debug, Clone, Copy)]
pub enum What {
    /// Everything the machine plays, mixed — the equivalent of a loopback.
    Everything,
    /// One program, by its process id.
    Process(u32),
}

/// A capture in progress, as the shape of its stream plus where samples land.
pub struct Tapped {
    /// Interleaved channel count of the blocks.
    pub channels: u32,
    /// Frames per second of the blocks.
    pub sample_rate: u32,
    /// Blocks of interleaved `f32`.
    pub blocks: Receiver<Vec<f32>>,
    /// Keeps the tap and its aggregate alive; dropping it takes both down.
    pub tap: Tap,
}

/// A running tap, with everything Core Audio wants back when it stops.
pub struct Tap {
    aggregate: AudioObjectID,
    tap: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    // The channel the callback sends through, freed only after it has stopped.
    sender: *mut Sender<Vec<f32>>,
    stopped: Arc<AtomicBool>,
}

// The pointer is only ever touched by the callback, which stops before the
// pointer is freed; nothing else reads it.
unsafe impl Send for Tap {}

impl Drop for Tap {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        unsafe {
            if self.io_proc.is_some() {
                AudioDeviceStop(self.aggregate, self.io_proc);
                AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc);
            }
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            AudioHardwareDestroyProcessTap(self.tap);
            // Core Audio has stopped calling back by now, so the sender the
            // callback held can go.
            drop(Box::from_raw(self.sender));
        }
    }
}

/// Start a tap on what this asks for.
pub fn open(what: What) -> Result<Tapped> {
    let (blocks_tx, blocks) = channel();

    unsafe {
        // A tap is described in Objective-C: which processes, mixed how.
        let description = match what {
            What::Everything => CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &NSArray::new(),
            ),
            What::Process(pid) => {
                let processes = NSArray::from_retained_slice(&[NSNumber::new_u32(pid)]);
                CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &processes,
                )
            }
        };
        description.setName(&NSString::from_str("output-decibel-meter"));

        let mut tap: AudioObjectID = 0;
        let status = AudioHardwareCreateProcessTap(Some(&description), &mut tap);
        if status != 0 {
            bail!("Core Audio would not create a process tap: status {status}");
        }
        let tap_uid = description.UUID().UUIDString();

        // A tap cannot be recorded from directly: it has to be a sub-device of
        // a private aggregate, which is what an IOProc can then be run on.
        let aggregate = match aggregate_around(&tap_uid) {
            Ok(aggregate) => aggregate,
            Err(e) => {
                AudioHardwareDestroyProcessTap(tap);
                return Err(e);
            }
        };

        let format = match stream_format(aggregate) {
            Ok(format) => format,
            Err(e) => {
                AudioHardwareDestroyAggregateDevice(aggregate);
                AudioHardwareDestroyProcessTap(tap);
                return Err(e);
            }
        };

        let sender = Box::into_raw(Box::new(blocks_tx));
        let mut io_proc: AudioDeviceIOProcID = None;
        let status = AudioDeviceCreateIOProcID(
            aggregate,
            Some(deliver),
            sender as *mut std::ffi::c_void,
            &mut io_proc,
        );
        if status != 0 {
            drop(Box::from_raw(sender));
            AudioHardwareDestroyAggregateDevice(aggregate);
            AudioHardwareDestroyProcessTap(tap);
            bail!("Core Audio would not attach a reader to the tap: status {status}");
        }

        let held = Tap {
            aggregate,
            tap,
            io_proc,
            sender,
            stopped: Arc::new(AtomicBool::new(false)),
        };

        let status = AudioDeviceStart(aggregate, io_proc);
        if status != 0 {
            bail!("Core Audio would not start the tap: status {status}");
        }

        Ok(Tapped {
            channels: format.mChannelsPerFrame,
            sample_rate: format.mSampleRate as u32,
            blocks,
            tap: held,
        })
    }
}

/// Build the private aggregate device that makes a tap readable.
unsafe fn aggregate_around(tap_uid: &NSString) -> Result<AudioObjectID> {
    unsafe {
        let sub_tap = NSDictionary::from_retained_objects(
            &[NSString::from_str(
                std::str::from_utf8(kAudioSubTapUIDKey.to_bytes()).unwrap_or("uid"),
            )
            .as_ref()],
            &[Retained::from(tap_uid)],
        );

        let name = key(kAudioAggregateDeviceNameKey);
        let uid = key(kAudioAggregateDeviceUIDKey);
        let private = key(kAudioAggregateDeviceIsPrivateKey);
        let auto_start = key(kAudioAggregateDeviceTapAutoStartKey);
        let tap_list = key(kAudioAggregateDeviceTapListKey);
        let main = key(kAudioAggregateDeviceMainSubDeviceKey);

        let description: Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> =
            NSDictionary::from_retained_objects(
                &[
                    name.as_ref(),
                    uid.as_ref(),
                    private.as_ref(),
                    auto_start.as_ref(),
                    tap_list.as_ref(),
                    main.as_ref(),
                ],
                &[
                    Retained::into_super(NSString::from_str("output-decibel-meter tap")),
                    Retained::into_super(NSString::from_str(
                        &NSUUID::new().UUIDString().to_string(),
                    )),
                    Retained::into_super(NSNumber::new_bool(true)),
                    Retained::into_super(NSNumber::new_bool(true)),
                    Retained::into_super(NSArray::from_retained_slice(&[sub_tap])),
                    Retained::into_super(NSString::from_str("")),
                ],
            );

        let mut aggregate: AudioObjectID = 0;
        let status = AudioHardwareCreateAggregateDevice(
            description.as_ref() as *const _ as *const _,
            &mut aggregate,
        );
        if status != 0 {
            bail!("Core Audio would not wrap the tap in a device: status {status}");
        }
        Ok(aggregate)
    }
}

/// One of Core Audio's string keys, as an `NSString`.
fn key(raw: &'static std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(raw.to_str().unwrap_or_default())
}

/// The format the aggregate hands its input over in.
unsafe fn stream_format(
    device: AudioObjectID,
) -> Result<objc2_core_audio_types::AudioStreamBasicDescription> {
    unsafe {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreamFormat,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut format = objc2_core_audio_types::AudioStreamBasicDescription::default();
        let mut size = std::mem::size_of_val(&format) as u32;
        let status = AudioObjectGetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            &mut format as *mut _ as *mut std::ffi::c_void,
        );
        if status != 0 {
            bail!("the tap would not say what format it delivers: status {status}");
        }
        if format.mChannelsPerFrame == 0 || format.mSampleRate <= 0.0 {
            bail!("the tap delivers nothing: no channels or no rate");
        }
        Ok(format)
    }
}

/// The callback Core Audio drives, on its own real-time thread.
///
/// It does the least it can — a copy and a send — because everything here runs
/// under a deadline that dropping would be heard as a glitch.
unsafe extern "C-unwind" fn deliver(
    _device: AudioObjectID,
    _now: *const objc2_core_audio_types::AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const objc2_core_audio_types::AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const objc2_core_audio_types::AudioTimeStamp,
    context: *mut std::ffi::c_void,
) -> i32 {
    unsafe {
        if input.is_null() || context.is_null() {
            return 0;
        }
        let sender = &*(context as *const Sender<Vec<f32>>);
        let list = &*input;
        let count = list.mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts(list.mBuffers.as_ptr(), count);

        for buffer in buffers {
            if buffer.mData.is_null() {
                continue;
            }
            let samples = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
            let block = std::slice::from_raw_parts(buffer.mData as *const f32, samples).to_vec();
            if !block.is_empty() {
                let _ = sender.send(block);
            }
        }
        0
    }
}

/// How long a caller should be prepared to wait for the first block.
pub const FIRST_BLOCK: Duration = Duration::from_secs(2);
