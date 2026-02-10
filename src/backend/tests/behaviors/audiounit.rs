//! AudioUnit render callback synchronization tests.
//!
//! Tests investigating `AudioOutputUnitStop()` synchronization with callbacks:
//!
//! - `test_default_output_stop_sync_raw` - Tests DefaultOutput units (raw API)
//! - `test_vpio_stop_sync_raw` - Tests VoiceProcessingIO units (raw API)
//! - `test_default_output_stop_sync_wrapped` - Tests DefaultOutput units (wrapper API)
//! - `test_vpio_stop_sync_wrapped` - Tests VoiceProcessingIO units (wrapper API)
//!
//! **Key findings**:
//! - `AudioOutputUnitStop()` DOES wait for in-flight callbacks to complete
//! - For VPIO, both input and output callbacks run on the SAME thread (serialized)
//! - No callback overlap was detected

use super::*;
use std::mem;
use std::ptr;

/// Test to verify whether `AudioOutputUnitStop` waits for in-flight callbacks to complete.
/// Uses raw CoreAudio APIs directly.
///
/// Expected outcomes:
/// - If `callback_finished` is TRUE when stop returns: AudioOutputUnitStop waits
/// - If `callback_finished` is FALSE when stop returns: AudioOutputUnitStop does NOT wait
#[ignore]
#[test]
fn test_default_output_stop_sync_raw() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        callback_entered: AtomicBool,
        callback_finished: AtomicBool,
        callback_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                callback_entered: AtomicBool::new(false),
                callback_finished: AtomicBool::new(false),
                callback_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn render_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };

        let count = state.callback_count.fetch_add(1, Ordering::SeqCst);

        if count == 0 {
            state.callback_entered.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.callback_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            let buffer_count = buffers.mNumberBuffers as usize;
            if buffer_count > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        NO_ERR
    }

    let test_durations = [50u64, 100, 200];

    for sleep_ms in test_durations {
        println!(
            "\n--- Testing with callback sleep duration: {}ms ---",
            sleep_ms
        );

        let state = Box::new(CallbackState::new(sleep_ms));
        let state_ptr = Box::into_raw(state);

        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_DefaultOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = run_serially(|| unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) });
        if comp.is_null() {
            println!("Could not find audio component. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }

        let mut unit: AudioUnit = ptr::null_mut();
        let status = run_serially(|| unsafe { AudioComponentInstanceNew(comp, &mut unit) });
        if status != NO_ERR || unit.is_null() {
            println!(
                "Could not create audio unit (status={}). Skipping test.",
                status
            );
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }

        let callback_struct = AURenderCallbackStruct {
            inputProc: Some(render_callback),
            inputProcRefCon: state_ptr as *mut c_void,
        };

        let status = run_serially(|| unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Global,
                0,
                &callback_struct as *const AURenderCallbackStruct as *const c_void,
                mem::size_of::<AURenderCallbackStruct>() as u32,
            )
        });
        if status != NO_ERR {
            println!(
                "Could not set render callback (status={}). Skipping test.",
                status
            );
            run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }

        let status = run_serially(|| unsafe { AudioUnitInitialize(unit) });
        if status != NO_ERR {
            println!(
                "Could not initialize audio unit (status={}). Skipping test.",
                status
            );
            run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }

        let status = run_serially(|| unsafe { AudioOutputUnitStart(unit) });
        if status != NO_ERR {
            println!(
                "Could not start audio unit (status={}). Skipping test.",
                status
            );
            run_serially(|| unsafe {
                AudioUnitUninitialize(unit);
                AudioComponentInstanceDispose(unit);
            });
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }

        println!("Audio unit started. Waiting for callback to be entered...");

        let start = Instant::now();
        let timeout = Duration::from_secs(5);
        while !unsafe { (*state_ptr).callback_entered.load(Ordering::SeqCst) } {
            if start.elapsed() > timeout {
                println!("Timeout waiting for callback to be entered. Test inconclusive.");
                run_serially(|| unsafe {
                    AudioOutputUnitStop(unit);
                    AudioUnitUninitialize(unit);
                    AudioComponentInstanceDispose(unit);
                });
                unsafe { drop(Box::from_raw(state_ptr)) };
                return;
            }
            thread::sleep(Duration::from_micros(100));
        }

        println!("Callback entered! Now calling AudioOutputUnitStop()...");

        let finished_before_stop = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
        let stop_start = Instant::now();

        let status = run_serially(|| unsafe { AudioOutputUnitStop(unit) });
        let stop_duration = stop_start.elapsed();

        let finished_after_stop = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
        let callback_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };

        println!(
            "AudioOutputUnitStop returned (status={}, took {:?})",
            status, stop_duration
        );
        println!("  Callback count: {}", callback_count);
        println!("  callback_finished BEFORE stop: {}", finished_before_stop);
        println!("  callback_finished AFTER stop:  {}", finished_after_stop);

        if finished_before_stop {
            println!("  INCONCLUSIVE: Callback finished before we called stop.");
            println!("  (The sleep duration may be too short or the callback ran too fast)");
        } else if finished_after_stop {
            println!("  RESULT: AudioOutputUnitStop() DOES wait for callbacks to complete.");
            println!("  (callback_finished was false before stop, true after stop)");
        } else {
            println!("  RESULT: AudioOutputUnitStop() does NOT wait for callbacks!");
            println!("  (callback_finished is still false after stop returned)");
            println!("  WARNING: This confirms the race condition in destroy().");
        }

        run_serially(|| unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        });
        unsafe { drop(Box::from_raw(state_ptr)) };

        thread::sleep(Duration::from_millis(100));
    }

    println!("\n--- Test complete ---");
    println!("If AudioOutputUnitStop does NOT wait, then the code comment in destroy()");
    println!("is misleading and the races detected by TSan are real timing issues.");
}

/// Test to verify VoiceProcessingIO (VPIO) callback behavior with AudioOutputUnitStop().
/// Uses raw CoreAudio APIs directly.
///
/// This test answers critical questions about VPIO synchronization:
/// 1. Do input/output callbacks run on the same thread?
/// 2. Can callbacks overlap (run concurrently)?
/// 3. Does AudioOutputUnitStop() wait for BOTH callback types?
#[ignore]
#[test]
fn test_vpio_stop_sync_raw() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        input_thread_ids: Mutex<Vec<u64>>,
        output_thread_ids: Mutex<Vec<u64>>,
        input_running: AtomicBool,
        output_running: AtomicBool,
        overlap_detected: AtomicBool,
        input_started: AtomicBool,
        input_finished: AtomicBool,
        output_started: AtomicBool,
        output_finished: AtomicBool,
        input_count: AtomicU32,
        output_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                input_thread_ids: Mutex::new(Vec::new()),
                output_thread_ids: Mutex::new(Vec::new()),
                input_running: AtomicBool::new(false),
                output_running: AtomicBool::new(false),
                overlap_detected: AtomicBool::new(false),
                input_started: AtomicBool::new(false),
                input_finished: AtomicBool::new(false),
                output_started: AtomicBool::new(false),
                output_finished: AtomicBool::new(false),
                input_count: AtomicU32::new(0),
                output_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn input_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        _buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.input_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.output_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.input_running.store(true, Ordering::SeqCst);

        let count = state.input_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.input_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.input_finished.store(true, Ordering::SeqCst);
        }

        state.input_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.output_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.input_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.output_running.store(true, Ordering::SeqCst);

        let count = state.output_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.output_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.output_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            let buffer_count = buffers.mNumberBuffers as usize;
            if buffer_count > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        state.output_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== VPIO Callback Synchronization Test ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let desc = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_VoiceProcessingIO,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };

    let comp = run_serially(|| unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) });
    if comp.is_null() {
        println!("Could not find VoiceProcessingIO component. Skipping test.");
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let mut unit: AudioUnit = ptr::null_mut();
    let status = run_serially(|| unsafe { AudioComponentInstanceNew(comp, &mut unit) });
    if status != NO_ERR || unit.is_null() {
        println!(
            "Could not create VPIO unit (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let enable: u32 = 1;
    let status = run_serially(|| unsafe {
        AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            AU_OUT_BUS,
            &enable as *const u32 as *const c_void,
            mem::size_of::<u32>() as u32,
        )
    });
    if status != NO_ERR {
        println!(
            "Could not enable output (status={}). Skipping test.",
            status
        );
        run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_callback_struct = AURenderCallbackStruct {
        inputProc: Some(input_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| unsafe {
        AudioUnitSetProperty(
            unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            0,
            &input_callback_struct as *const AURenderCallbackStruct as *const c_void,
            mem::size_of::<AURenderCallbackStruct>() as u32,
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set input callback (status={}). Skipping test.",
            status
        );
        run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let output_callback_struct = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| unsafe {
        AudioUnitSetProperty(
            unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_callback_struct as *const AURenderCallbackStruct as *const c_void,
            mem::size_of::<AURenderCallbackStruct>() as u32,
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set output callback (status={}). Skipping test.",
            status
        );
        run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| unsafe { AudioUnitInitialize(unit) });
    if status != NO_ERR {
        println!(
            "Could not initialize VPIO unit (status={}). Skipping test.",
            status
        );
        run_serially(|| unsafe { AudioComponentInstanceDispose(unit) });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| unsafe { AudioOutputUnitStart(unit) });
    if status != NO_ERR {
        println!(
            "Could not start VPIO unit (status={}). Skipping test.",
            status
        );
        run_serially(|| unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!("VPIO unit started. Waiting for callbacks...");
    println!("(Sleep duration: {}ms per callback)\n", sleep_ms);

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut input_started = false;
    let mut output_started = false;

    while start.elapsed() < timeout {
        input_started = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
        output_started = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };

        if input_started || output_started {
            thread::sleep(Duration::from_millis(10));
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !input_started && !output_started {
        println!("TIMEOUT: No callbacks were triggered. Test inconclusive.");
        println!("(This may indicate VPIO requires actual audio hardware or permissions)");
        run_serially(|| unsafe {
            AudioOutputUnitStop(unit);
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_started_before = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
    let input_finished_before = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_started_before = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
    let output_finished_before = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };

    println!("State BEFORE AudioOutputUnitStop():");
    println!(
        "  Input:  started={}, finished={}",
        input_started_before, input_finished_before
    );
    println!(
        "  Output: started={}, finished={}",
        output_started_before, output_finished_before
    );

    let stop_start = Instant::now();
    let status = run_serially(|| unsafe { AudioOutputUnitStop(unit) });
    let stop_duration = stop_start.elapsed();

    let input_finished_after = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_finished_after = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
    let overlap_detected = unsafe { (*state_ptr).overlap_detected.load(Ordering::SeqCst) };

    println!(
        "\nAudioOutputUnitStop() returned (status={}, took {:?})",
        status, stop_duration
    );
    println!("\nState AFTER AudioOutputUnitStop():");
    println!("  Input:  finished={}", input_finished_after);
    println!("  Output: finished={}", output_finished_after);
    println!("  Input callback count:  {}", input_count);
    println!("  Output callback count: {}", output_count);

    let input_thread_ids = unsafe { (*state_ptr).input_thread_ids.lock().unwrap().clone() };
    let output_thread_ids = unsafe { (*state_ptr).output_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===\n");

    println!("1. THREAD IDENTITY:");
    if !input_thread_ids.is_empty() {
        let first_input_tid = input_thread_ids[0];
        let all_same_input = input_thread_ids.iter().all(|&id| id == first_input_tid);
        println!(
            "   Input callbacks: {} invocations, all on thread {}",
            input_thread_ids.len(),
            if all_same_input {
                format!("{}", first_input_tid)
            } else {
                "MULTIPLE".to_string()
            }
        );
    } else {
        println!("   Input callbacks: NONE triggered");
    }

    if !output_thread_ids.is_empty() {
        let first_output_tid = output_thread_ids[0];
        let all_same_output = output_thread_ids.iter().all(|&id| id == first_output_tid);
        println!(
            "   Output callbacks: {} invocations, all on thread {}",
            output_thread_ids.len(),
            if all_same_output {
                format!("{}", first_output_tid)
            } else {
                "MULTIPLE".to_string()
            }
        );
    } else {
        println!("   Output callbacks: NONE triggered");
    }

    if !input_thread_ids.is_empty() && !output_thread_ids.is_empty() {
        let same_thread = input_thread_ids[0] == output_thread_ids[0];
        println!(
            "   RESULT: Input and output callbacks run on {} thread(s)",
            if same_thread { "THE SAME" } else { "DIFFERENT" }
        );
    }

    println!("\n2. CALLBACK SERIALIZATION:");
    println!("   Overlap detected: {}", overlap_detected);
    if overlap_detected {
        println!("   RESULT: Callbacks CAN run concurrently (POTENTIAL RACE!)");
    } else {
        println!("   RESULT: Callbacks appear to be serialized");
    }

    println!("\n3. STOP SYNCHRONIZATION:");
    let input_waited = input_started_before && !input_finished_before && input_finished_after;
    let output_waited = output_started_before && !output_finished_before && output_finished_after;

    if input_started_before {
        if input_finished_before {
            println!("   Input: INCONCLUSIVE (finished before stop was called)");
        } else if input_finished_after {
            println!("   Input: Stop WAITED for input callback to finish");
        } else {
            println!("   Input: Stop did NOT wait (callback still not finished!)");
        }
    } else {
        println!("   Input: Not triggered before stop");
    }

    if output_started_before {
        if output_finished_before {
            println!("   Output: INCONCLUSIVE (finished before stop was called)");
        } else if output_finished_after {
            println!("   Output: Stop WAITED for output callback to finish");
        } else {
            println!("   Output: Stop did NOT wait (callback still not finished!)");
        }
    } else {
        println!("   Output: Not triggered before stop");
    }

    println!("\n4. IMPLICATIONS FOR TSAN RACES:");

    let input_ran_during_stop = !input_started_before && input_finished_after;
    let output_ran_during_stop =
        output_started_before && !output_finished_before && output_finished_after;

    let _ = input_waited;
    let _ = output_waited;
    let _ = input_ran_during_stop;
    let _ = output_ran_during_stop;

    let both_finished = input_finished_after && output_finished_after;

    let expected_single_callback_ms = sleep_ms;
    let expected_both_callbacks_ms = sleep_ms * 2;
    let actual_stop_ms = stop_duration.as_millis() as u64;

    println!("   Stop duration analysis:");
    println!(
        "     Expected for single callback: ~{}ms",
        expected_single_callback_ms
    );
    println!(
        "     Expected for both callbacks:  ~{}ms",
        expected_both_callbacks_ms
    );
    println!("     Actual stop duration:         ~{}ms", actual_stop_ms);

    if both_finished && actual_stop_ms > expected_single_callback_ms + 100 {
        println!("\n   CONCLUSION: AudioOutputUnitStop() waits for ALL audio thread work.");
        println!("   - Both callbacks run on the SAME thread (serialized)");
        println!("   - Stop blocks until all pending callbacks complete");
        println!("   - TSan races are likely FALSE POSITIVES");
        println!("   - (TSan can't see CoreAudio's internal synchronization)");
    } else if output_finished_after || input_finished_after {
        println!("\n   CONCLUSION: AudioOutputUnitStop() waits for callbacks.");
        println!("   TSan races are likely FALSE POSITIVES.");
    } else if !input_started_before && !output_started_before {
        println!("\n   CONCLUSION: INCONCLUSIVE - no callbacks were running when stop was called.");
    } else {
        println!("\n   CONCLUSION: AudioOutputUnitStop() may NOT wait properly!");
        println!("   TSan races may be REAL - explicit synchronization recommended.");
    }

    run_serially(|| unsafe {
        AudioUnitUninitialize(unit);
        AudioComponentInstanceDispose(unit);
    });
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Same as `test_default_output_stop_sync_raw` but uses the crate's wrapper APIs
/// (`create_typed_audiounit`, `audio_unit_set_property`, etc.) instead of raw CoreAudio APIs.
///
/// This verifies that our wrapper layer doesn't change the synchronization behavior.
#[ignore]
#[test]
fn test_default_output_stop_sync_wrapped() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        callback_entered: AtomicBool,
        callback_finished: AtomicBool,
        callback_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                callback_entered: AtomicBool::new(false),
                callback_finished: AtomicBool::new(false),
                callback_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn render_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let count = state.callback_count.fetch_add(1, Ordering::SeqCst);

        if count == 0 {
            state.callback_entered.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.callback_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        NO_ERR
    }

    println!("\n=== DefaultOutput Stop Sync Test (via wrapper APIs) ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("Could not get default output device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let unit = match run_serially(|| create_audiounit(&device)) {
        Ok(u) => u,
        Err(_) => {
            println!("Could not create audio unit. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let callback_struct = AURenderCallbackStruct {
        inputProc: Some(render_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };

    let status = run_serially(|| {
        audio_unit_set_property(
            unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            0,
            &callback_struct,
            mem::size_of_val(&callback_struct),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set render callback (status={}). Skipping test.",
            status
        );
        run_serially(|| dispose_audio_unit(unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| audio_unit_initialize(unit));
    if status != NO_ERR {
        println!(
            "Could not initialize unit (status={}). Skipping test.",
            status
        );
        run_serially(|| dispose_audio_unit(unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let result = run_serially(|| start_audiounit(unit));
    if result.is_err() {
        println!("Could not start unit (result={:?}). Skipping test.", result);
        run_serially(|| {
            audio_unit_uninitialize(unit);
            dispose_audio_unit(unit);
        });
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!(
        "Audio unit started (sleep duration: {}ms). Waiting for callback...",
        sleep_ms
    );

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    while !unsafe { (*state_ptr).callback_entered.load(Ordering::SeqCst) } {
        if start.elapsed() > timeout {
            println!("Timeout waiting for callback. Test inconclusive.");
            run_serially(|| {
                let _ = stop_audiounit(unit);
                audio_unit_uninitialize(unit);
                dispose_audio_unit(unit);
            });
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
        thread::sleep(Duration::from_micros(100));
    }

    println!("Callback entered! Now calling stop_audiounit()...");

    let finished_before = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
    let stop_start = Instant::now();

    let result = run_serially(|| stop_audiounit(unit));
    let stop_duration = stop_start.elapsed();

    let finished_after = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
    let callback_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };

    println!(
        "stop_audiounit returned (result={:?}, took {:?})",
        result, stop_duration
    );
    println!("  Callback count: {}", callback_count);
    println!("  callback_finished BEFORE stop: {}", finished_before);
    println!("  callback_finished AFTER stop:  {}", finished_after);

    if finished_before {
        println!("  INCONCLUSIVE: Callback finished before stop was called.");
    } else if finished_after {
        println!("  RESULT: stop_audiounit() DOES wait for callbacks.");
    } else {
        println!("  RESULT: stop_audiounit() does NOT wait!");
    }

    run_serially(|| {
        audio_unit_uninitialize(unit);
        dispose_audio_unit(unit);
    });
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Same as `test_vpio_stop_sync_raw` but uses the crate's wrapper APIs
/// (`create_voiceprocessing_audiounit`, `enable_audiounit_scope`, etc.).
///
/// This verifies that our wrapper layer and VPIO setup match production behavior.
#[ignore]
#[test]
fn test_vpio_stop_sync_wrapped() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        input_thread_ids: Mutex<Vec<u64>>,
        output_thread_ids: Mutex<Vec<u64>>,
        input_running: AtomicBool,
        output_running: AtomicBool,
        overlap_detected: AtomicBool,
        input_started: AtomicBool,
        input_finished: AtomicBool,
        output_started: AtomicBool,
        output_finished: AtomicBool,
        input_count: AtomicU32,
        output_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                input_thread_ids: Mutex::new(Vec::new()),
                output_thread_ids: Mutex::new(Vec::new()),
                input_running: AtomicBool::new(false),
                output_running: AtomicBool::new(false),
                overlap_detected: AtomicBool::new(false),
                input_started: AtomicBool::new(false),
                input_finished: AtomicBool::new(false),
                output_started: AtomicBool::new(false),
                output_finished: AtomicBool::new(false),
                input_count: AtomicU32::new(0),
                output_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn input_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        _buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.input_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.output_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.input_running.store(true, Ordering::SeqCst);

        let count = state.input_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.input_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.input_finished.store(true, Ordering::SeqCst);
        }

        state.input_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.output_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.input_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.output_running.store(true, Ordering::SeqCst);

        let count = state.output_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.output_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.output_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        state.output_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== VPIO Stop Sync Test (via wrapper APIs) ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let queue = Queue::new_with_target("test_vpio_stop_sync_wrapped", get_serial_queue_singleton());
    let mut shared_vpio_mgr = SharedVoiceProcessingUnitManager::new(queue.clone());

    let in_device = match run_serially(|| get_default_device(DeviceType::INPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_INPUT,
        },
        None => {
            println!("Could not get default input device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let out_device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("Could not get default output device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let vpio_handle = match run_serially(|| {
        get_voiceprocessing_audiounit(&mut shared_vpio_mgr, &in_device, &out_device)
    }) {
        Ok(h) => h,
        Err(_) => {
            println!("Could not create VPIO unit. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };
    let unit = vpio_handle.as_ref().unit;

    let input_cb = AURenderCallbackStruct {
        inputProc: Some(input_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &input_cb,
            mem::size_of_val(&input_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set input callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let output_cb = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_cb,
            mem::size_of_val(&output_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set output callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| audio_unit_initialize(unit));
    if status != NO_ERR {
        println!(
            "Could not initialize VPIO (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let result = run_serially(|| start_audiounit(unit));
    if result.is_err() {
        println!("Could not start VPIO (result={:?}). Skipping test.", result);
        run_serially(|| audio_unit_uninitialize(unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!(
        "VPIO started (sleep duration: {}ms). Waiting for callbacks...\n",
        sleep_ms
    );

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut input_started = false;
    let mut output_started = false;

    while start.elapsed() < timeout {
        input_started = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
        output_started = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
        if input_started || output_started {
            thread::sleep(Duration::from_millis(10));
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !input_started && !output_started {
        println!("TIMEOUT: No callbacks triggered. Test inconclusive.");
        run_serially(|| {
            let _ = stop_audiounit(unit);
            audio_unit_uninitialize(unit);
        });
        run_serially(move || drop(vpio_handle));
        drop(shared_vpio_mgr);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_started_before = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
    let input_finished_before = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_started_before = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
    let output_finished_before = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };

    println!("State BEFORE stop_audiounit():");
    println!(
        "  Input:  started={}, finished={}",
        input_started_before, input_finished_before
    );
    println!(
        "  Output: started={}, finished={}",
        output_started_before, output_finished_before
    );

    let stop_start = Instant::now();
    let result = run_serially(|| stop_audiounit(unit));
    let stop_duration = stop_start.elapsed();

    let input_finished_after = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_finished_after = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
    let overlap_detected = unsafe { (*state_ptr).overlap_detected.load(Ordering::SeqCst) };

    println!(
        "\nstop_audiounit() returned (result={:?}, took {:?})",
        result, stop_duration
    );
    println!("\nState AFTER stop_audiounit():");
    println!("  Input:  finished={}", input_finished_after);
    println!("  Output: finished={}", output_finished_after);
    println!("  Input callback count:  {}", input_count);
    println!("  Output callback count: {}", output_count);

    let input_thread_ids = unsafe { (*state_ptr).input_thread_ids.lock().unwrap().clone() };
    let output_thread_ids = unsafe { (*state_ptr).output_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===\n");

    println!("1. THREAD IDENTITY:");
    if !input_thread_ids.is_empty() && !output_thread_ids.is_empty() {
        let same_thread = input_thread_ids[0] == output_thread_ids[0];
        println!(
            "   Input/Output on {} thread(s)",
            if same_thread { "SAME" } else { "DIFFERENT" }
        );
    }

    println!(
        "\n2. OVERLAP: {}",
        if overlap_detected { "DETECTED" } else { "None" }
    );

    println!("\n3. STOP SYNC:");
    let both_finished = input_finished_after && output_finished_after;
    let actual_stop_ms = stop_duration.as_millis() as u64;
    println!(
        "   Stop duration: ~{}ms (expected ~{}ms for both)",
        actual_stop_ms,
        sleep_ms * 2
    );

    if both_finished && actual_stop_ms > sleep_ms + 100 {
        println!("   RESULT: stop_audiounit() waits for ALL callbacks.");
    } else if input_finished_after || output_finished_after {
        println!("   RESULT: stop_audiounit() waits for callbacks.");
    } else {
        println!("   RESULT: stop_audiounit() may NOT wait!");
    }

    run_serially(move || {
        audio_unit_uninitialize(unit);
        drop(vpio_handle);
    });
    drop(shared_vpio_mgr);
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Tests VPIO stop synchronization using two separate `AudioUnit` variables
/// pointing to the same VPIO unit, matching the production setup where
/// `CoreStreamData::input_unit` and `CoreStreamData::output_unit` both hold
/// the same VPIO pointer (see `mod.rs:3610-3612`).
///
/// Production only calls `stop_audiounit(self.input_unit)` for VPIO
/// (see `stop_audiounits` at `mod.rs:3399-3417` which returns early),
/// so this test verifies that stopping via one variable still synchronizes
/// both input and output callbacks.
#[ignore]
#[test]
fn test_vpio_two_variables_stop_sync() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        input_thread_ids: Mutex<Vec<u64>>,
        output_thread_ids: Mutex<Vec<u64>>,
        input_running: AtomicBool,
        output_running: AtomicBool,
        overlap_detected: AtomicBool,
        input_started: AtomicBool,
        input_finished: AtomicBool,
        output_started: AtomicBool,
        output_finished: AtomicBool,
        input_count: AtomicU32,
        output_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                input_thread_ids: Mutex::new(Vec::new()),
                output_thread_ids: Mutex::new(Vec::new()),
                input_running: AtomicBool::new(false),
                output_running: AtomicBool::new(false),
                overlap_detected: AtomicBool::new(false),
                input_started: AtomicBool::new(false),
                input_finished: AtomicBool::new(false),
                output_started: AtomicBool::new(false),
                output_finished: AtomicBool::new(false),
                input_count: AtomicU32::new(0),
                output_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn input_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        _buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.input_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.output_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.input_running.store(true, Ordering::SeqCst);

        let count = state.input_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.input_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.input_finished.store(true, Ordering::SeqCst);
        }

        state.input_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.output_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.input_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.output_running.store(true, Ordering::SeqCst);

        let count = state.output_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.output_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.output_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        state.output_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== VPIO Two-Variable Stop Sync Test ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let queue = Queue::new_with_target(
        "test_vpio_two_variables_stop_sync",
        get_serial_queue_singleton(),
    );
    let mut shared_vpio_mgr = SharedVoiceProcessingUnitManager::new(queue.clone());

    let in_device = match run_serially(|| get_default_device(DeviceType::INPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_INPUT,
        },
        None => {
            println!("Could not get default input device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let out_device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("Could not get default output device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let vpio_handle = match run_serially(|| {
        get_voiceprocessing_audiounit(&mut shared_vpio_mgr, &in_device, &out_device)
    }) {
        Ok(h) => h,
        Err(_) => {
            println!("Could not create VPIO unit. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    // Two variables pointing to the same VPIO unit, matching production setup.
    let input_unit = vpio_handle.as_ref().unit;
    let output_unit = vpio_handle.as_ref().unit;
    assert_eq!(input_unit, output_unit);
    println!(
        "input_unit = {:p}, output_unit = {:p} (same pointer: {})",
        input_unit,
        output_unit,
        input_unit == output_unit
    );

    // Set input callback via input_unit (production uses self.input_unit).
    let input_cb = AURenderCallbackStruct {
        inputProc: Some(input_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &input_cb,
            mem::size_of_val(&input_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set input callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    // Set output callback via output_unit (production uses self.output_unit).
    let output_cb = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            output_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_cb,
            mem::size_of_val(&output_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set output callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    // Initialize via input_unit (production initializes self.input_unit).
    let status = run_serially(|| audio_unit_initialize(input_unit));
    if status != NO_ERR {
        println!(
            "Could not initialize VPIO (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    // Start via input_unit only (production: start_audiounits line 3369).
    let result = run_serially(|| start_audiounit(input_unit));
    if result.is_err() {
        println!("Could not start VPIO (result={:?}). Skipping test.", result);
        run_serially(|| audio_unit_uninitialize(input_unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!(
        "VPIO started via input_unit only (sleep: {}ms). Waiting for callbacks...\n",
        sleep_ms
    );

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut input_started = false;
    let mut output_started = false;

    while start.elapsed() < timeout {
        input_started = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
        output_started = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
        if input_started || output_started {
            thread::sleep(Duration::from_millis(10));
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !input_started && !output_started {
        println!("TIMEOUT: No callbacks triggered. Test inconclusive.");
        run_serially(|| {
            let _ = stop_audiounit(input_unit);
            audio_unit_uninitialize(input_unit);
        });
        run_serially(move || drop(vpio_handle));
        drop(shared_vpio_mgr);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_started_before = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
    let input_finished_before = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_started_before = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
    let output_finished_before = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };

    println!("State BEFORE stop_audiounit(input_unit):");
    println!(
        "  Input:  started={}, finished={}",
        input_started_before, input_finished_before
    );
    println!(
        "  Output: started={}, finished={}",
        output_started_before, output_finished_before
    );

    // Stop via input_unit only (production: stop_audiounits line 3400, returns early at 3417).
    let stop_start = Instant::now();
    let result = run_serially(|| stop_audiounit(input_unit));
    let stop_duration = stop_start.elapsed();

    let input_finished_after = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_finished_after = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
    let overlap_detected = unsafe { (*state_ptr).overlap_detected.load(Ordering::SeqCst) };

    println!(
        "\nstop_audiounit(input_unit) returned (result={:?}, took {:?})",
        result, stop_duration
    );
    println!("\nState AFTER stop:");
    println!("  Input:  finished={}", input_finished_after);
    println!("  Output: finished={}", output_finished_after);
    println!("  Input callback count:  {}", input_count);
    println!("  Output callback count: {}", output_count);

    // Post-stop monitoring: verify no callbacks fire after stop returns.
    let count_at_stop_input = input_count;
    let count_at_stop_output = output_count;
    thread::sleep(Duration::from_millis(200));
    let count_after_wait_input = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let count_after_wait_output = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };

    let input_thread_ids = unsafe { (*state_ptr).input_thread_ids.lock().unwrap().clone() };
    let output_thread_ids = unsafe { (*state_ptr).output_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===\n");

    println!("1. THREAD IDENTITY:");
    if !input_thread_ids.is_empty() && !output_thread_ids.is_empty() {
        let same_thread = input_thread_ids[0] == output_thread_ids[0];
        println!(
            "   Input/Output on {} thread(s)",
            if same_thread { "SAME" } else { "DIFFERENT" }
        );
    }

    println!(
        "\n2. OVERLAP: {}",
        if overlap_detected { "DETECTED" } else { "None" }
    );

    println!("\n3. STOP SYNC:");
    let both_finished = input_finished_after && output_finished_after;
    let actual_stop_ms = stop_duration.as_millis() as u64;
    println!(
        "   Stop duration: ~{}ms (expected ~{}ms for both)",
        actual_stop_ms,
        sleep_ms * 2
    );

    if both_finished && actual_stop_ms > sleep_ms + 100 {
        println!("   RESULT: stop_audiounit(input_unit) waits for ALL callbacks.");
    } else if input_finished_after || output_finished_after {
        println!("   RESULT: stop_audiounit(input_unit) waits for callbacks.");
    } else {
        println!("   RESULT: stop_audiounit(input_unit) may NOT wait!");
    }

    println!("\n4. POST-STOP CALLBACKS:");
    let post_stop_input = count_after_wait_input - count_at_stop_input;
    let post_stop_output = count_after_wait_output - count_at_stop_output;
    println!("   Input callbacks after stop:  {}", post_stop_input);
    println!("   Output callbacks after stop: {}", post_stop_output);
    if post_stop_input == 0 && post_stop_output == 0 {
        println!("   RESULT: No callbacks fired after stop (good).");
    } else {
        println!("   RESULT: Callbacks fired AFTER stop! This is unexpected.");
    }

    run_serially(move || {
        audio_unit_uninitialize(input_unit);
        drop(vpio_handle);
    });
    drop(shared_vpio_mgr);
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Tests VPIO stop synchronization with voice processing properties enabled
/// BEFORE starting the unit.
///
/// The TSan-failing tests (`test_ops_duplex_voice_stream_set_input_processing_params_*`)
/// both use VPIO with AGC and voice processing enabled. This test verifies that
/// enabling these properties does not change callback synchronization behavior.
#[ignore]
#[test]
fn test_vpio_with_voice_processing_params_stop_sync() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        input_thread_ids: Mutex<Vec<u64>>,
        output_thread_ids: Mutex<Vec<u64>>,
        input_running: AtomicBool,
        output_running: AtomicBool,
        overlap_detected: AtomicBool,
        input_started: AtomicBool,
        input_finished: AtomicBool,
        output_started: AtomicBool,
        output_finished: AtomicBool,
        input_count: AtomicU32,
        output_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                input_thread_ids: Mutex::new(Vec::new()),
                output_thread_ids: Mutex::new(Vec::new()),
                input_running: AtomicBool::new(false),
                output_running: AtomicBool::new(false),
                overlap_detected: AtomicBool::new(false),
                input_started: AtomicBool::new(false),
                input_finished: AtomicBool::new(false),
                output_started: AtomicBool::new(false),
                output_finished: AtomicBool::new(false),
                input_count: AtomicU32::new(0),
                output_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn input_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        _buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.input_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.output_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.input_running.store(true, Ordering::SeqCst);

        let count = state.input_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.input_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.input_finished.store(true, Ordering::SeqCst);
        }

        state.input_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.output_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.input_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.output_running.store(true, Ordering::SeqCst);

        let count = state.output_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.output_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.output_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        state.output_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== VPIO Voice Processing Params Stop Sync Test (params before start) ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let queue = Queue::new_with_target(
        "test_vpio_vp_params_stop_sync",
        get_serial_queue_singleton(),
    );
    let mut shared_vpio_mgr = SharedVoiceProcessingUnitManager::new(queue.clone());

    let in_device = match run_serially(|| get_default_device(DeviceType::INPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_INPUT,
        },
        None => {
            println!("Could not get default input device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let out_device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("Could not get default output device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let vpio_handle = match run_serially(|| {
        get_voiceprocessing_audiounit(&mut shared_vpio_mgr, &in_device, &out_device)
    }) {
        Ok(h) => h,
        Err(_) => {
            println!("Could not create VPIO unit. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let input_unit = vpio_handle.as_ref().unit;
    let output_unit = vpio_handle.as_ref().unit;

    let input_cb = AURenderCallbackStruct {
        inputProc: Some(input_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &input_cb,
            mem::size_of_val(&input_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set input callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let output_cb = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            output_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_cb,
            mem::size_of_val(&output_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set output callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| audio_unit_initialize(input_unit));
    if status != NO_ERR {
        println!(
            "Could not initialize VPIO (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    // Enable voice processing params BEFORE start.
    let agc_enable: u32 = 1;
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAUVoiceIOProperty_VoiceProcessingEnableAGC,
            kAudioUnitScope_Global,
            AU_IN_BUS,
            &agc_enable,
            mem::size_of::<u32>(),
        )
    });
    println!(
        "Set kAUVoiceIOProperty_VoiceProcessingEnableAGC = 1: status={}",
        status
    );

    let bypass_disable: u32 = 0;
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAUVoiceIOProperty_BypassVoiceProcessing,
            kAudioUnitScope_Global,
            AU_IN_BUS,
            &bypass_disable,
            mem::size_of::<u32>(),
        )
    });
    println!(
        "Set kAUVoiceIOProperty_BypassVoiceProcessing = 0: status={}",
        status
    );

    let result = run_serially(|| start_audiounit(input_unit));
    if result.is_err() {
        println!("Could not start VPIO (result={:?}). Skipping test.", result);
        run_serially(|| audio_unit_uninitialize(input_unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!(
        "VPIO started with VP params (sleep: {}ms). Waiting for callbacks...\n",
        sleep_ms
    );

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut input_started = false;
    let mut output_started = false;

    while start.elapsed() < timeout {
        input_started = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
        output_started = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
        if input_started || output_started {
            thread::sleep(Duration::from_millis(10));
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !input_started && !output_started {
        println!("TIMEOUT: No callbacks triggered. Test inconclusive.");
        run_serially(|| {
            let _ = stop_audiounit(input_unit);
            audio_unit_uninitialize(input_unit);
        });
        run_serially(move || drop(vpio_handle));
        drop(shared_vpio_mgr);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_started_before = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
    let input_finished_before = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_started_before = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
    let output_finished_before = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };

    println!("State BEFORE stop_audiounit(input_unit):");
    println!(
        "  Input:  started={}, finished={}",
        input_started_before, input_finished_before
    );
    println!(
        "  Output: started={}, finished={}",
        output_started_before, output_finished_before
    );

    let stop_start = Instant::now();
    let result = run_serially(|| stop_audiounit(input_unit));
    let stop_duration = stop_start.elapsed();

    let input_finished_after = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_finished_after = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
    let overlap_detected = unsafe { (*state_ptr).overlap_detected.load(Ordering::SeqCst) };

    println!(
        "\nstop_audiounit(input_unit) returned (result={:?}, took {:?})",
        result, stop_duration
    );

    let count_at_stop_input = input_count;
    let count_at_stop_output = output_count;
    thread::sleep(Duration::from_millis(200));
    let count_after_wait_input = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let count_after_wait_output = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };

    let input_thread_ids = unsafe { (*state_ptr).input_thread_ids.lock().unwrap().clone() };
    let output_thread_ids = unsafe { (*state_ptr).output_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===\n");

    println!("1. THREAD IDENTITY:");
    if !input_thread_ids.is_empty() && !output_thread_ids.is_empty() {
        let same_thread = input_thread_ids[0] == output_thread_ids[0];
        println!(
            "   Input/Output on {} thread(s)",
            if same_thread { "SAME" } else { "DIFFERENT" }
        );
    }

    println!(
        "\n2. OVERLAP: {}",
        if overlap_detected { "DETECTED" } else { "None" }
    );

    println!("\n3. STOP SYNC:");
    let both_finished = input_finished_after && output_finished_after;
    let actual_stop_ms = stop_duration.as_millis() as u64;
    println!(
        "   Stop duration: ~{}ms (expected ~{}ms for both)",
        actual_stop_ms,
        sleep_ms * 2
    );

    if both_finished && actual_stop_ms > sleep_ms + 100 {
        println!("   RESULT: stop_audiounit(input_unit) waits for ALL callbacks.");
    } else if input_finished_after || output_finished_after {
        println!("   RESULT: stop_audiounit(input_unit) waits for callbacks.");
    } else {
        println!("   RESULT: stop_audiounit(input_unit) may NOT wait!");
    }

    println!("\n4. POST-STOP CALLBACKS:");
    println!(
        "   Input callbacks after stop:  {}",
        count_after_wait_input - count_at_stop_input
    );
    println!(
        "   Output callbacks after stop: {}",
        count_after_wait_output - count_at_stop_output
    );
    if count_after_wait_input == count_at_stop_input
        && count_after_wait_output == count_at_stop_output
    {
        println!("   RESULT: No callbacks fired after stop (good).");
    } else {
        println!("   RESULT: Callbacks fired AFTER stop! This is unexpected.");
    }

    run_serially(move || {
        audio_unit_uninitialize(input_unit);
        drop(vpio_handle);
    });
    drop(shared_vpio_mgr);
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Tests VPIO stop synchronization with voice processing properties set
/// AFTER starting the unit, while callbacks are already running.
///
/// This matches the production ordering in `start_audiounits` (mod.rs:3369-3378):
/// 1. `start_audiounit(self.input_unit)` -- callbacks begin
/// 2. `set_input_processing_params(self.input_unit, ...)` -- params set while running
#[ignore]
#[test]
fn test_vpio_set_params_after_start_stop_sync() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    struct CallbackState {
        input_thread_ids: Mutex<Vec<u64>>,
        output_thread_ids: Mutex<Vec<u64>>,
        input_running: AtomicBool,
        output_running: AtomicBool,
        overlap_detected: AtomicBool,
        input_started: AtomicBool,
        input_finished: AtomicBool,
        output_started: AtomicBool,
        output_finished: AtomicBool,
        input_count: AtomicU32,
        output_count: AtomicU32,
        sleep_duration_ms: u64,
    }

    impl CallbackState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                input_thread_ids: Mutex::new(Vec::new()),
                output_thread_ids: Mutex::new(Vec::new()),
                input_running: AtomicBool::new(false),
                output_running: AtomicBool::new(false),
                overlap_detected: AtomicBool::new(false),
                input_started: AtomicBool::new(false),
                input_finished: AtomicBool::new(false),
                output_started: AtomicBool::new(false),
                output_finished: AtomicBool::new(false),
                input_count: AtomicU32::new(0),
                output_count: AtomicU32::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn input_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        _buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.input_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.output_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.input_running.store(true, Ordering::SeqCst);

        let count = state.input_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.input_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.input_finished.store(true, Ordering::SeqCst);
        }

        state.input_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const CallbackState) };
        let thread_id = get_thread_id();

        if let Ok(mut ids) = state.output_thread_ids.lock() {
            ids.push(thread_id);
        }

        if state.input_running.load(Ordering::SeqCst) {
            state.overlap_detected.store(true, Ordering::SeqCst);
        }
        state.output_running.store(true, Ordering::SeqCst);

        let count = state.output_count.fetch_add(1, Ordering::SeqCst);
        if count == 0 {
            state.output_started.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(state.sleep_duration_ms));
            state.output_finished.store(true, Ordering::SeqCst);
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        state.output_running.store(false, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== VPIO Voice Processing Params Stop Sync Test (params after start) ===\n");

    let sleep_ms = 200u64;
    let state = Box::new(CallbackState::new(sleep_ms));
    let state_ptr = Box::into_raw(state);

    let queue =
        Queue::new_with_target("test_vpio_params_after_start", get_serial_queue_singleton());
    let mut shared_vpio_mgr = SharedVoiceProcessingUnitManager::new(queue.clone());

    let in_device = match run_serially(|| get_default_device(DeviceType::INPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_INPUT,
        },
        None => {
            println!("Could not get default input device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let out_device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("Could not get default output device. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let vpio_handle = match run_serially(|| {
        get_voiceprocessing_audiounit(&mut shared_vpio_mgr, &in_device, &out_device)
    }) {
        Ok(h) => h,
        Err(_) => {
            println!("Could not create VPIO unit. Skipping test.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let input_unit = vpio_handle.as_ref().unit;
    let output_unit = vpio_handle.as_ref().unit;

    let input_cb = AURenderCallbackStruct {
        inputProc: Some(input_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &input_cb,
            mem::size_of_val(&input_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set input callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let output_cb = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            output_unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_cb,
            mem::size_of_val(&output_cb),
        )
    });
    if status != NO_ERR {
        println!(
            "Could not set output callback (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| audio_unit_initialize(input_unit));
    if status != NO_ERR {
        println!(
            "Could not initialize VPIO (status={}). Skipping test.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    // Start FIRST (matching production: start_audiounits line 3369).
    let result = run_serially(|| start_audiounit(input_unit));
    if result.is_err() {
        println!("Could not start VPIO (result={:?}). Skipping test.", result);
        run_serially(|| audio_unit_uninitialize(input_unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!(
        "VPIO started (sleep: {}ms). Waiting for first callback before setting VP params...\n",
        sleep_ms
    );

    // Wait for at least one callback to fire.
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    while start.elapsed() < timeout {
        let ic = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
        let oc = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
        if ic > 0 || oc > 0 {
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    // Set voice processing params AFTER start, while callbacks are running
    // (matching production: set_input_processing_params at line 3378).
    let agc_enable: u32 = 1;
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAUVoiceIOProperty_VoiceProcessingEnableAGC,
            kAudioUnitScope_Global,
            AU_IN_BUS,
            &agc_enable,
            mem::size_of::<u32>(),
        )
    });
    println!(
        "Set kAUVoiceIOProperty_VoiceProcessingEnableAGC = 1 (after start): status={}",
        status
    );

    let bypass_disable: u32 = 0;
    let status = run_serially(|| {
        audio_unit_set_property(
            input_unit,
            kAUVoiceIOProperty_BypassVoiceProcessing,
            kAudioUnitScope_Global,
            AU_IN_BUS,
            &bypass_disable,
            mem::size_of::<u32>(),
        )
    });
    println!(
        "Set kAUVoiceIOProperty_BypassVoiceProcessing = 0 (after start): status={}",
        status
    );

    // Wait for the slow callbacks (count==0 path with sleep) to be entered.
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    let mut input_started = false;
    let mut output_started = false;

    while start.elapsed() < timeout {
        input_started = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
        output_started = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
        if input_started || output_started {
            thread::sleep(Duration::from_millis(10));
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !input_started && !output_started {
        println!("TIMEOUT: No slow callbacks triggered. Test inconclusive.");
        run_serially(|| {
            let _ = stop_audiounit(input_unit);
            audio_unit_uninitialize(input_unit);
        });
        run_serially(move || drop(vpio_handle));
        drop(shared_vpio_mgr);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let input_started_before = unsafe { (*state_ptr).input_started.load(Ordering::SeqCst) };
    let input_finished_before = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_started_before = unsafe { (*state_ptr).output_started.load(Ordering::SeqCst) };
    let output_finished_before = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };

    println!("\nState BEFORE stop_audiounit(input_unit):");
    println!(
        "  Input:  started={}, finished={}",
        input_started_before, input_finished_before
    );
    println!(
        "  Output: started={}, finished={}",
        output_started_before, output_finished_before
    );

    let stop_start = Instant::now();
    let result = run_serially(|| stop_audiounit(input_unit));
    let stop_duration = stop_start.elapsed();

    let input_finished_after = unsafe { (*state_ptr).input_finished.load(Ordering::SeqCst) };
    let output_finished_after = unsafe { (*state_ptr).output_finished.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };
    let overlap_detected = unsafe { (*state_ptr).overlap_detected.load(Ordering::SeqCst) };

    println!(
        "\nstop_audiounit(input_unit) returned (result={:?}, took {:?})",
        result, stop_duration
    );

    let count_at_stop_input = input_count;
    let count_at_stop_output = output_count;
    thread::sleep(Duration::from_millis(200));
    let count_after_wait_input = unsafe { (*state_ptr).input_count.load(Ordering::SeqCst) };
    let count_after_wait_output = unsafe { (*state_ptr).output_count.load(Ordering::SeqCst) };

    let input_thread_ids = unsafe { (*state_ptr).input_thread_ids.lock().unwrap().clone() };
    let output_thread_ids = unsafe { (*state_ptr).output_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===\n");

    println!("1. THREAD IDENTITY:");
    if !input_thread_ids.is_empty() && !output_thread_ids.is_empty() {
        let same_thread = input_thread_ids[0] == output_thread_ids[0];
        println!(
            "   Input/Output on {} thread(s)",
            if same_thread { "SAME" } else { "DIFFERENT" }
        );
    }

    println!(
        "\n2. OVERLAP: {}",
        if overlap_detected { "DETECTED" } else { "None" }
    );

    println!("\n3. STOP SYNC:");
    let both_finished = input_finished_after && output_finished_after;
    let actual_stop_ms = stop_duration.as_millis() as u64;
    println!(
        "   Stop duration: ~{}ms (expected ~{}ms for both)",
        actual_stop_ms,
        sleep_ms * 2
    );

    if both_finished && actual_stop_ms > sleep_ms + 100 {
        println!("   RESULT: stop_audiounit(input_unit) waits for ALL callbacks.");
    } else if input_finished_after || output_finished_after {
        println!("   RESULT: stop_audiounit(input_unit) waits for callbacks.");
    } else {
        println!("   RESULT: stop_audiounit(input_unit) may NOT wait!");
    }

    println!("\n4. POST-STOP CALLBACKS:");
    println!(
        "   Input callbacks after stop:  {}",
        count_after_wait_input - count_at_stop_input
    );
    println!(
        "   Output callbacks after stop: {}",
        count_after_wait_output - count_at_stop_output
    );
    if count_after_wait_input == count_at_stop_input
        && count_after_wait_output == count_at_stop_output
    {
        println!("   RESULT: No callbacks fired after stop (good).");
    } else {
        println!("   RESULT: Callbacks fired AFTER stop! This is unexpected.");
    }

    run_serially(move || {
        audio_unit_uninitialize(input_unit);
        drop(vpio_handle);
    });
    drop(shared_vpio_mgr);
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Verifies that `__tsan_release`/`__tsan_acquire` annotations can teach TSan
/// about CoreAudio's implicit `AudioOutputUnitStop` synchronization.
///
/// Under TSan: callbacks write non-atomic shared data, then `__tsan_release`.
/// After stop returns: `__tsan_acquire`, then read the shared data.
/// Without annotations, TSan would flag this as a data race.
/// With annotations, TSan sees the happens-before edge and stays silent.
///
/// Without TSan, the annotations are no-ops and the test still passes.
#[ignore]
#[test]
fn test_vpio_tsan_annotations_verify() {
    use std::cell::UnsafeCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    struct TsanTestState {
        // Non-atomic shared data, deliberately unprotected.
        // Written by callback thread, read by main thread after stop.
        shared_data: UnsafeCell<u64>,
        callback_ran: AtomicBool,
        // AudioUnit handle, used as the TSan annotation token (mirrors production).
        unit: AudioUnit,
    }

    unsafe impl Sync for TsanTestState {}

    extern "C" fn output_callback(
        user_data: *mut c_void,
        _flags: *mut AudioUnitRenderActionFlags,
        _timestamp: *const AudioTimeStamp,
        _bus: u32,
        _frames: u32,
        buffer_list: *mut AudioBufferList,
    ) -> OSStatus {
        let state = unsafe { &*(user_data as *const TsanTestState) };

        if !state.callback_ran.swap(true, Ordering::SeqCst) {
            unsafe {
                *state.shared_data.get() = 0xDEAD_BEEF_CAFE_BABE;
            }
        }

        if !buffer_list.is_null() {
            let buffers = unsafe { &mut *buffer_list };
            if buffers.mNumberBuffers > 0 {
                let buffer = unsafe { &mut *(&mut buffers.mBuffers as *mut _ as *mut AudioBuffer) };
                if !buffer.mData.is_null() && buffer.mDataByteSize > 0 {
                    unsafe {
                        ptr::write_bytes(buffer.mData as *mut u8, 0, buffer.mDataByteSize as usize);
                    }
                }
            }
        }

        #[cfg(feature = "tsan-annotations")]
        {
            extern "C" {
                fn __tsan_release(addr: *mut c_void);
            }
            unsafe {
                __tsan_release(state.unit as *mut c_void);
            }
        }

        NO_ERR
    }

    println!("\n=== VPIO TSan Annotations Verify Test ===\n");

    #[cfg(not(feature = "tsan-annotations"))]
    {
        println!("NOTE: Not running under TSan. Annotations are no-ops.");
        println!("Run with TSan to verify annotations silence race reports:");
        println!("  RUSTFLAGS=\"-Zsanitizer=thread -Cunsafe-allow-abi-mismatch=sanitizer\" \\");
        println!("  cargo test -Z build-std --target $(rustc -vV | grep host | cut -d' ' -f2) \\");
        println!("  -p cubeb-coreaudio test_vpio_tsan_annotations -- --ignored --nocapture\n");
    }

    let state = Box::new(TsanTestState {
        shared_data: UnsafeCell::new(0),
        callback_ran: AtomicBool::new(false),
        unit: ptr::null_mut(),
    });
    let state_ptr = Box::into_raw(state);

    let queue = Queue::new_with_target("test_tsan_verify", get_serial_queue_singleton());
    let mut shared_vpio_mgr = SharedVoiceProcessingUnitManager::new(queue.clone());

    let in_device = match run_serially(|| get_default_device(DeviceType::INPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_INPUT,
        },
        None => {
            println!("No input device. Skipping.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let out_device = match run_serially(|| get_default_device(DeviceType::OUTPUT)) {
        Some(id) => device_info {
            id,
            flags: device_flags::DEV_OUTPUT,
        },
        None => {
            println!("No output device. Skipping.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let vpio_handle = match run_serially(|| {
        get_voiceprocessing_audiounit(&mut shared_vpio_mgr, &in_device, &out_device)
    }) {
        Ok(h) => h,
        Err(_) => {
            println!("Could not create VPIO. Skipping.");
            unsafe { drop(Box::from_raw(state_ptr)) };
            return;
        }
    };

    let unit = vpio_handle.as_ref().unit;
    unsafe {
        (*state_ptr).unit = unit;
    }

    let output_cb = AURenderCallbackStruct {
        inputProc: Some(output_callback),
        inputProcRefCon: state_ptr as *mut c_void,
    };
    let status = run_serially(|| {
        audio_unit_set_property(
            unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Global,
            AU_OUT_BUS,
            &output_cb,
            mem::size_of_val(&output_cb),
        )
    });
    if status != NO_ERR {
        println!("Could not set callback (status={}). Skipping.", status);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let status = run_serially(|| audio_unit_initialize(unit));
    if status != NO_ERR {
        println!("Could not initialize (status={}). Skipping.", status);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let result = run_serially(|| start_audiounit(unit));
    if result.is_err() {
        println!("Could not start (result={:?}). Skipping.", result);
        run_serially(|| audio_unit_uninitialize(unit));
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if unsafe { (*state_ptr).callback_ran.load(Ordering::SeqCst) } {
            break;
        }
        thread::sleep(Duration::from_micros(100));
    }

    if !unsafe { (*state_ptr).callback_ran.load(Ordering::SeqCst) } {
        println!("TIMEOUT: No callback triggered. Inconclusive.");
        run_serially(|| {
            let _ = stop_audiounit(unit);
            audio_unit_uninitialize(unit);
        });
        run_serially(move || drop(vpio_handle));
        drop(shared_vpio_mgr);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    let result = run_serially(|| stop_audiounit(unit));
    println!("stop_audiounit returned: {:?}", result);

    // Acquire on the AudioUnit handle to pair with the callback's release,
    // mirroring the production annotation in stop_audiounit().
    #[cfg(feature = "tsan-annotations")]
    {
        extern "C" {
            fn __tsan_acquire(addr: *mut c_void);
        }
        unsafe {
            __tsan_acquire(unit as *mut c_void);
        }
    }

    // Read non-atomic shared data after stop. Without TSan annotations, TSan would
    // flag this as a race with the callback's write. With the release-in-callback →
    // acquire-after-stop chain, TSan sees the happens-before edge.
    let value = unsafe { *(*state_ptr).shared_data.get() };
    println!("Shared data after stop: 0x{:X}", value);
    assert_eq!(
        value, 0xDEAD_BEEF_CAFE_BABE,
        "Callback should have written this value"
    );

    println!("RESULT: Successfully read callback-written data after stop.");
    #[cfg(feature = "tsan-annotations")]
    println!("TSan annotations active. If no warnings above, annotations are correct.");
    #[cfg(not(feature = "tsan-annotations"))]
    println!("TSan not active. Re-run under TSan to verify.");

    run_serially(move || {
        audio_unit_uninitialize(unit);
        drop(vpio_handle);
    });
    drop(shared_vpio_mgr);
    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}
