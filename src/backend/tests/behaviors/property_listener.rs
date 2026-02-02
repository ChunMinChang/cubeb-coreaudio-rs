//! AudioObject property listener synchronization tests.
//!
//! Tests investigating `AudioObjectRemovePropertyListener` synchronization:
//!
//! - `test_audio_object_remove_property_listener_sync` - Tests if removal waits for in-flight callbacks
//! - `test_audio_object_listener_post_removal_callbacks` - Tests if callbacks can fire after removal
//! - `test_audio_object_listener_destroy_simulation` - Simulates destroy() UAF detection
//! - `test_audio_object_listener_rapid_add_remove_stress` - Rapid add/remove stress test
//! - `test_listener_remains_after_failed_unregistration` - Tests failed unregistration scenario
//! - `test_callback_behavior_with_dead_device` - Tests callback behavior when device is removed
//! - `test_multiple_property_listeners_thread_serialization` - Tests if callbacks for different properties run on same thread
//!
//! **Key findings**:
//! - `AudioObjectRemovePropertyListener` DOES wait for in-flight callbacks to complete
//! - If unregistration fails but destruction continues, callbacks WILL fire (UAF vulnerability)
//! - Multiple property callbacks on the same object run serially on the same thread (no locks needed)

use super::super::utils::{test_get_default_device, Scope};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

fn get_hardware_devices_address() -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    }
}

struct TestAggregateDeviceManager {
    plugin_id: AudioObjectID,
}

impl TestAggregateDeviceManager {
    fn new() -> Option<Self> {
        let plugin_id = run_serially(|| match AggregateDevice::get_system_plugin_id() {
            Ok(id) => id,
            Err(_) => kAudioObjectUnknown,
        });
        if plugin_id == kAudioObjectUnknown {
            return None;
        }
        Some(Self { plugin_id })
    }

    fn create_device(
        &self,
        input_id: AudioObjectID,
        output_id: AudioObjectID,
    ) -> Option<AudioObjectID> {
        run_serially(|| match AggregateDevice::new(input_id, output_id) {
            Ok(device) => Some(device.get_device_id()),
            Err(_) => None,
        })
    }

    fn create_blank_device(&self) -> Option<AudioObjectID> {
        run_serially(
            || match AggregateDevice::create_blank_device(self.plugin_id) {
                Ok(device_id) => Some(device_id),
                Err(_) => None,
            },
        )
    }

    fn destroy_device(&self, device_id: AudioObjectID) -> bool {
        run_serially(|| AggregateDevice::destroy_device(self.plugin_id, device_id).is_ok())
    }
}

/// Test to verify whether `audio_object_remove_property_listener` waits for in-flight callbacks.
///
/// This test:
/// 1. Creates an aggregate device first (before registering listener)
/// 2. Registers a listener for `kAudioHardwarePropertyDevices` with a sleeping callback
/// 3. Destroys the aggregate device (triggers callback)
/// 4. Waits for callback entry
/// 5. Calls `audio_object_remove_property_listener` and measures timing
/// 6. Checks if callback finished before/after removal returns
///
/// The key insight is that device destruction triggers the callback asynchronously,
/// while device creation via `AggregateDevice::new()` waits synchronously for confirmation.
///
/// **Key result**: If `finished_before=false, finished_after=true` → Remove WAITS
#[ignore]
#[test]
fn test_audio_object_remove_property_listener_sync() {
    struct ListenerState {
        callback_entered: AtomicBool,
        callback_finished: AtomicBool,
        callback_count: AtomicU32,
        callback_thread_id: AtomicU64,
        sleep_duration_ms: u64,
    }

    impl ListenerState {
        fn new(sleep_ms: u64) -> Self {
            Self {
                callback_entered: AtomicBool::new(false),
                callback_finished: AtomicBool::new(false),
                callback_count: AtomicU32::new(0),
                callback_thread_id: AtomicU64::new(0),
                sleep_duration_ms: sleep_ms,
            }
        }
    }

    extern "C" fn property_listener(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };

        state
            .callback_thread_id
            .store(get_thread_id(), Ordering::SeqCst);
        state.callback_entered.store(true, Ordering::SeqCst);
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        let sleep_ms = state.sleep_duration_ms;
        thread::sleep(Duration::from_millis(sleep_ms));

        state.callback_finished.store(true, Ordering::SeqCst);

        NO_ERR
    }

    println!("\n=== AudioObject Property Listener Synchronization Test ===\n");

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let test_durations = [200u64, 500, 1000];

    for sleep_ms in test_durations {
        println!(
            "\n--- Testing with callback sleep duration: {}ms ---",
            sleep_ms
        );

        println!("Creating blank aggregate device BEFORE registering listener...");
        let device_id = match manager.create_blank_device() {
            Some(id) => id,
            None => {
                println!("Could not create blank aggregate device. Skipping.");
                continue;
            }
        };
        println!("Created device: {}", device_id);

        thread::sleep(Duration::from_millis(100));

        let state = Box::new(ListenerState::new(sleep_ms));
        let state_ptr = Box::into_raw(state);
        let address = get_hardware_devices_address();

        let status = run_serially(|| {
            audio_object_add_property_listener(
                kAudioObjectSystemObject,
                &address,
                property_listener,
                state_ptr as *mut c_void,
            )
        });

        if status != NO_ERR {
            println!(
                "Could not add property listener (status={}). Skipping.",
                status
            );
            manager.destroy_device(device_id);
            unsafe { drop(Box::from_raw(state_ptr)) };
            continue;
        }

        println!("Listener registered. Destroying device to trigger callback...");

        let destroy_thread = thread::spawn({
            let plugin_id = manager.plugin_id;
            move || run_serially(|| AggregateDevice::destroy_device(plugin_id, device_id))
        });

        let start = Instant::now();
        let timeout = Duration::from_secs(5);
        let mut timed_out = false;
        while !unsafe { (*state_ptr).callback_entered.load(Ordering::SeqCst) } {
            if start.elapsed() > timeout {
                println!("Timeout waiting for callback entry. Test inconclusive.");
                timed_out = true;
                break;
            }
            thread::sleep(Duration::from_micros(50));
        }

        if timed_out {
            let _ = destroy_thread.join();
            run_serially(|| {
                audio_object_remove_property_listener(
                    kAudioObjectSystemObject,
                    &address,
                    property_listener,
                    state_ptr as *mut c_void,
                )
            });
            unsafe { drop(Box::from_raw(state_ptr)) };
            continue;
        }

        let callback_thread = unsafe { (*state_ptr).callback_thread_id.load(Ordering::SeqCst) };
        println!(
            "Callback entered on thread {}! Calling audio_object_remove_property_listener()...",
            callback_thread
        );

        let finished_before = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
        let remove_start = Instant::now();

        let status = run_serially(|| {
            audio_object_remove_property_listener(
                kAudioObjectSystemObject,
                &address,
                property_listener,
                state_ptr as *mut c_void,
            )
        });
        let remove_duration = remove_start.elapsed();

        let finished_after = unsafe { (*state_ptr).callback_finished.load(Ordering::SeqCst) };
        let callback_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };

        println!(
            "\nState BEFORE remove: callback_finished={}",
            finished_before
        );
        println!(
            "audio_object_remove_property_listener returned (status={}, took {:?})",
            status, remove_duration
        );
        println!("State AFTER remove: callback_finished={}", finished_after);
        println!("Callback count: {}", callback_count);

        println!("\n=== RESULT ===");
        if finished_before {
            println!("INCONCLUSIVE: Callback finished before remove was called.");
            println!("(Try increasing sleep duration)");
        } else if finished_after {
            println!(
                "SYNCHRONIZATION: audio_object_remove_property_listener DOES wait for callbacks."
            );
            println!(
                "  - Remove took {:?} (expected ~{}ms for callback to finish)",
                remove_duration, sleep_ms
            );
            println!("  - IMPLICATION: Current destroy() order is safe for SUCCESSFUL removal.");
        } else {
            println!("WARNING: audio_object_remove_property_listener does NOT wait for callbacks!");
            println!("  - callback_finished is still false after remove returned");
            println!("  - IMPLICATION: UAF vulnerability exists in destroy()");
        }

        let _ = destroy_thread.join();
        unsafe { drop(Box::from_raw(state_ptr)) };

        thread::sleep(Duration::from_millis(100));
    }

    println!("\n=== Test Complete ===\n");
}

/// Test whether property callbacks can fire AFTER removal returns.
///
/// This test:
/// 1. Registers a listener
/// 2. Triggers multiple device changes rapidly
/// 3. Calls remove and immediately sets a `removal_completed` flag
/// 4. Waits and checks if any callbacks fired after removal
///
/// **Key result**: If `callbacks_after_removal > 0` → Callbacks CAN fire after removal
#[ignore]
#[test]
fn test_audio_object_listener_post_removal_callbacks() {
    struct ListenerState {
        callback_count: AtomicU32,
        removal_completed: AtomicBool,
        callbacks_after_removal: AtomicU32,
        callback_thread_ids: Mutex<Vec<u64>>,
    }

    impl ListenerState {
        fn new() -> Self {
            Self {
                callback_count: AtomicU32::new(0),
                removal_completed: AtomicBool::new(false),
                callbacks_after_removal: AtomicU32::new(0),
                callback_thread_ids: Mutex::new(Vec::new()),
            }
        }
    }

    extern "C" fn property_listener(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        if let Ok(mut ids) = state.callback_thread_ids.lock() {
            ids.push(get_thread_id());
        }

        if state.removal_completed.load(Ordering::SeqCst) {
            state.callbacks_after_removal.fetch_add(1, Ordering::SeqCst);
        }

        thread::sleep(Duration::from_millis(50));

        NO_ERR
    }

    println!("\n=== Post-Removal Callback Test ===\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let state = Box::new(ListenerState::new());
    let state_ptr = Box::into_raw(state);
    let address = get_hardware_devices_address();

    let status = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });

    if status != NO_ERR {
        println!(
            "Could not add property listener (status={}). Skipping.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!("Listener registered. Triggering multiple device changes...");

    let mut created_devices = Vec::new();
    for i in 0..3 {
        thread::sleep(Duration::from_millis(50));
        if let Some(device_id) = manager.create_device(input_id, output_id) {
            println!("  Created device {} (iteration {})", device_id, i);
            created_devices.push(device_id);
        }
    }

    thread::sleep(Duration::from_millis(100));

    let count_before_remove = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    println!("\nCallback count before remove: {}", count_before_remove);
    println!("Calling audio_object_remove_property_listener()...");

    let remove_start = Instant::now();
    let status = run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });
    let remove_duration = remove_start.elapsed();

    unsafe { (*state_ptr).removal_completed.store(true, Ordering::SeqCst) };

    println!(
        "Remove returned (status={}, took {:?})",
        status, remove_duration
    );
    println!("Marked removal_completed=true");

    for device_id in &created_devices {
        manager.destroy_device(*device_id);
    }

    thread::sleep(Duration::from_millis(500));

    let final_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    let after_removal = unsafe { (*state_ptr).callbacks_after_removal.load(Ordering::SeqCst) };
    let thread_ids = unsafe { (*state_ptr).callback_thread_ids.lock().unwrap().clone() };

    println!("\n=== RESULTS ===");
    println!("Total callbacks: {}", final_count);
    println!("Callbacks after removal flag set: {}", after_removal);
    println!(
        "Unique callback threads: {:?}",
        thread_ids.iter().collect::<std::collections::HashSet<_>>()
    );

    if after_removal > 0 {
        println!(
            "\nWARNING: {} callback(s) fired AFTER removal returned!",
            after_removal
        );
        println!("IMPLICATION: Critical bug - need explicit barrier or callback protection.");
    } else {
        println!("\nNo callbacks fired after removal.");
        println!("IMPLICATION: Removal appears to provide synchronization.");
    }

    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Simulate the destroy pattern to detect potential UAF.
///
/// This test:
/// 1. Creates a mock stream structure with a validity sentinel
/// 2. Registers a listener that checks the sentinel and accesses mock fields
/// 3. Triggers device changes
/// 4. Simulates destroy pattern: remove listener, invalidate sentinel, overwrite fields
/// 5. Checks if callback ever saw invalid state
///
/// **Key result**: Detects actual UAF pattern if callback accesses invalid data
#[ignore]
#[test]
fn test_audio_object_listener_destroy_simulation() {
    const VALID_SENTINEL: u64 = 0xDEADBEEF_CAFEBABE;
    const INVALID_SENTINEL: u64 = 0xBADBADBA_DBADBAD0;

    struct MockStream {
        sentinel: AtomicU64,
        data_field_1: AtomicU64,
        data_field_2: AtomicU64,
    }

    struct ListenerState {
        mock_stream: *mut MockStream,
        saw_invalid_sentinel: AtomicBool,
        saw_corrupted_data: AtomicBool,
        callback_count: AtomicU32,
        callbacks_after_invalidation: AtomicU32,
        stream_invalidated: AtomicBool,
    }

    impl ListenerState {
        fn new(mock_stream: *mut MockStream) -> Self {
            Self {
                mock_stream,
                saw_invalid_sentinel: AtomicBool::new(false),
                saw_corrupted_data: AtomicBool::new(false),
                callback_count: AtomicU32::new(0),
                callbacks_after_invalidation: AtomicU32::new(0),
                stream_invalidated: AtomicBool::new(false),
            }
        }
    }

    extern "C" fn property_listener(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        if state.stream_invalidated.load(Ordering::SeqCst) {
            state
                .callbacks_after_invalidation
                .fetch_add(1, Ordering::SeqCst);
        }

        let mock_stream = unsafe { &*state.mock_stream };

        let sentinel = mock_stream.sentinel.load(Ordering::SeqCst);
        if sentinel != VALID_SENTINEL {
            state.saw_invalid_sentinel.store(true, Ordering::SeqCst);
            return NO_ERR;
        }

        let d1 = mock_stream.data_field_1.load(Ordering::SeqCst);
        let d2 = mock_stream.data_field_2.load(Ordering::SeqCst);
        if d1 != 0x1111_1111_1111_1111 || d2 != 0x2222_2222_2222_2222 {
            state.saw_corrupted_data.store(true, Ordering::SeqCst);
        }

        thread::sleep(Duration::from_millis(100));

        NO_ERR
    }

    println!("\n=== Destroy Simulation Test (UAF Detection) ===\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let mock_stream = Box::new(MockStream {
        sentinel: AtomicU64::new(VALID_SENTINEL),
        data_field_1: AtomicU64::new(0x1111_1111_1111_1111),
        data_field_2: AtomicU64::new(0x2222_2222_2222_2222),
    });
    let mock_stream_ptr = Box::into_raw(mock_stream);

    let state = Box::new(ListenerState::new(mock_stream_ptr));
    let state_ptr = Box::into_raw(state);
    let address = get_hardware_devices_address();

    let status = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });

    if status != NO_ERR {
        println!(
            "Could not add property listener (status={}). Skipping.",
            status
        );
        unsafe {
            drop(Box::from_raw(state_ptr));
            drop(Box::from_raw(mock_stream_ptr));
        };
        return;
    }

    println!("Mock stream created with valid sentinel.");
    println!("Listener registered. Triggering device change...");

    let device_id = manager.create_device(input_id, output_id);
    if device_id.is_none() {
        println!("Could not create aggregate device. Cleaning up.");
        run_serially(|| {
            audio_object_remove_property_listener(
                kAudioObjectSystemObject,
                &address,
                property_listener,
                state_ptr as *mut c_void,
            )
        });
        unsafe {
            drop(Box::from_raw(state_ptr));
            drop(Box::from_raw(mock_stream_ptr));
        };
        return;
    }
    let device_id = device_id.unwrap();

    thread::sleep(Duration::from_millis(50));

    println!("\nSimulating destroy() sequence:");
    println!("  1. Removing property listener...");

    let remove_start = Instant::now();
    let status = run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });
    let remove_duration = remove_start.elapsed();
    println!(
        "     Remove returned (status={}, took {:?})",
        status, remove_duration
    );

    println!("  2. Invalidating sentinel...");
    unsafe {
        (*mock_stream_ptr)
            .sentinel
            .store(INVALID_SENTINEL, Ordering::SeqCst)
    };
    unsafe {
        (*state_ptr)
            .stream_invalidated
            .store(true, Ordering::SeqCst)
    };

    println!("  3. Overwriting data fields with poison values...");
    unsafe {
        (*mock_stream_ptr)
            .data_field_1
            .store(0xDEAD_DEAD_DEAD_DEAD, Ordering::SeqCst);
        (*mock_stream_ptr)
            .data_field_2
            .store(0xBEEF_BEEF_BEEF_BEEF, Ordering::SeqCst);
    };

    thread::sleep(Duration::from_millis(200));

    let saw_invalid = unsafe { (*state_ptr).saw_invalid_sentinel.load(Ordering::SeqCst) };
    let saw_corrupted = unsafe { (*state_ptr).saw_corrupted_data.load(Ordering::SeqCst) };
    let callback_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    let after_invalidation = unsafe {
        (*state_ptr)
            .callbacks_after_invalidation
            .load(Ordering::SeqCst)
    };

    println!("\n=== RESULTS ===");
    println!("Total callbacks: {}", callback_count);
    println!("Callbacks after invalidation: {}", after_invalidation);
    println!("Saw invalid sentinel: {}", saw_invalid);
    println!("Saw corrupted data: {}", saw_corrupted);

    if saw_invalid {
        println!("\nCRITICAL: Callback accessed INVALID sentinel!");
        println!("This confirms UAF vulnerability in destroy() pattern.");
    } else if saw_corrupted {
        println!("\nWARNING: Callback saw corrupted data!");
        println!("Race condition detected during destruction.");
    } else if after_invalidation > 0 {
        println!(
            "\nWARNING: {} callback(s) fired after stream invalidation!",
            after_invalidation
        );
        println!("However, they did not detect invalid state (timing-dependent).");
    } else {
        println!("\nNo UAF detected in this run.");
        println!("Remove appears to synchronize properly.");
    }

    manager.destroy_device(device_id);
    unsafe {
        drop(Box::from_raw(state_ptr));
        drop(Box::from_raw(mock_stream_ptr));
    };

    println!("\n=== Test Complete ===\n");
}

/// Stress test with rapid add/remove cycles.
///
/// This test:
/// 1. Performs rapid add/remove cycles (configurable iterations)
/// 2. Tracks callback counts, timing, and errors
/// 3. Verifies no callbacks fire after final removal
#[ignore]
#[test]
fn test_audio_object_listener_rapid_add_remove_stress() {
    struct ListenerState {
        callback_count: AtomicU32,
        listener_active: AtomicBool,
        callbacks_when_inactive: AtomicU32,
    }

    impl ListenerState {
        fn new() -> Self {
            Self {
                callback_count: AtomicU32::new(0),
                listener_active: AtomicBool::new(false),
                callbacks_when_inactive: AtomicU32::new(0),
            }
        }
    }

    extern "C" fn property_listener(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        if !state.listener_active.load(Ordering::SeqCst) {
            state.callbacks_when_inactive.fetch_add(1, Ordering::SeqCst);
        }

        NO_ERR
    }

    println!("\n=== Rapid Add/Remove Stress Test ===\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let iterations = 50;
    let state = Box::new(ListenerState::new());
    let state_ptr = Box::into_raw(state);
    let address = get_hardware_devices_address();

    let mut add_errors = 0u32;
    let mut remove_errors = 0u32;
    let mut device_create_errors = 0u32;

    println!("Running {} add/remove cycles...", iterations);
    let test_start = Instant::now();

    for i in 0..iterations {
        let status = run_serially(|| {
            audio_object_add_property_listener(
                kAudioObjectSystemObject,
                &address,
                property_listener,
                state_ptr as *mut c_void,
            )
        });

        if status != NO_ERR {
            add_errors += 1;
            continue;
        }

        unsafe { (*state_ptr).listener_active.store(true, Ordering::SeqCst) };

        if let Some(device_id) = manager.create_device(input_id, output_id) {
            thread::sleep(Duration::from_millis(10));
            manager.destroy_device(device_id);
        } else {
            device_create_errors += 1;
        }

        unsafe { (*state_ptr).listener_active.store(false, Ordering::SeqCst) };

        let status = run_serially(|| {
            audio_object_remove_property_listener(
                kAudioObjectSystemObject,
                &address,
                property_listener,
                state_ptr as *mut c_void,
            )
        });

        if status != NO_ERR {
            remove_errors += 1;
        }

        if (i + 1) % 10 == 0 {
            println!("  Completed {} iterations...", i + 1);
        }
    }

    let test_duration = test_start.elapsed();

    thread::sleep(Duration::from_millis(500));

    let final_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    let when_inactive = unsafe { (*state_ptr).callbacks_when_inactive.load(Ordering::SeqCst) };

    println!("\n=== RESULTS ===");
    println!("Test duration: {:?}", test_duration);
    println!("Iterations: {}", iterations);
    println!("Add errors: {}", add_errors);
    println!("Remove errors: {}", remove_errors);
    println!("Device create errors: {}", device_create_errors);
    println!("Total callbacks: {}", final_count);
    println!("Callbacks when listener marked inactive: {}", when_inactive);

    if when_inactive > 0 {
        println!(
            "\nWARNING: {} callback(s) fired when listener was marked inactive!",
            when_inactive
        );
        println!("This may indicate a race between remove and callback delivery.");
    } else {
        println!("\nNo callbacks fired when listener was inactive.");
        println!("Add/remove operations appear to be properly synchronized.");
    }

    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Test what happens when listener unregistration fails but destruction continues.
///
/// This simulates the failure path in `destroy()` where `uninstall_system_changed_callback()`
/// fails but the function continues to destroy the stream.
#[ignore]
#[test]
fn test_listener_remains_after_failed_unregistration() {
    struct ListenerState {
        callback_count: AtomicU32,
        stream_destroyed: AtomicBool,
        callbacks_after_destroy: AtomicU32,
    }

    impl ListenerState {
        fn new() -> Self {
            Self {
                callback_count: AtomicU32::new(0),
                stream_destroyed: AtomicBool::new(false),
                callbacks_after_destroy: AtomicU32::new(0),
            }
        }
    }

    extern "C" fn property_listener(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        if state.stream_destroyed.load(Ordering::SeqCst) {
            state.callbacks_after_destroy.fetch_add(1, Ordering::SeqCst);
        }

        thread::sleep(Duration::from_millis(20));

        NO_ERR
    }

    println!("\n=== Failed Unregistration Simulation Test ===\n");
    println!("This test simulates what happens if listener removal fails");
    println!("but destroy() continues anyway (current code behavior).\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let state = Box::new(ListenerState::new());
    let state_ptr = Box::into_raw(state);
    let address = get_hardware_devices_address();

    let status = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });

    if status != NO_ERR {
        println!(
            "Could not add property listener (status={}). Skipping.",
            status
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!("Listener registered.");
    println!("Simulating: uninstall_system_changed_callback() FAILS");
    println!("Simulating: destroy() continues anyway, marks stream as destroyed\n");

    unsafe { (*state_ptr).stream_destroyed.store(true, Ordering::SeqCst) };

    println!("Triggering device changes after 'destruction'...");

    let mut created_devices = Vec::new();
    for i in 0..3 {
        if let Some(device_id) = manager.create_device(input_id, output_id) {
            println!("  Created device {} (iteration {})", device_id, i);
            created_devices.push(device_id);
            thread::sleep(Duration::from_millis(100));
        }
    }

    thread::sleep(Duration::from_millis(500));

    let total_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    let after_destroy = unsafe { (*state_ptr).callbacks_after_destroy.load(Ordering::SeqCst) };

    println!("\n=== RESULTS ===");
    println!("Total callbacks: {}", total_count);
    println!("Callbacks after 'destruction': {}", after_destroy);

    if after_destroy > 0 {
        println!(
            "\nCRITICAL: {} callback(s) fired after simulated destruction!",
            after_destroy
        );
        println!("IMPLICATION: If uninstall fails, listener continues to fire.");
        println!("             This WILL cause UAF if memory is actually freed.");
        println!("\nIn the current code (mod.rs:5016-5021):");
        println!("  - uninstall_system_changed_callback() failure is logged but ignored");
        println!("  - Stream destruction continues regardless");
        println!("  - Listener remains registered pointing to freed memory");
    } else {
        println!("\nNo callbacks detected after destruction.");
        println!("(This test may need adjustment to properly trigger callbacks)");
    }

    for device_id in &created_devices {
        manager.destroy_device(*device_id);
    }

    println!("\nNow properly removing the listener to clean up...");
    let status = run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });
    println!("Remove status: {}", status);

    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Test whether callbacks for different properties run on the same thread.
///
/// This simulates the production pattern where multiple property listeners are
/// registered on the same object (e.g., DeviceSource + DeviceIsAlive on a device,
/// or DefaultInputDevice + DefaultOutputDevice on system object).
///
/// This test:
/// 1. Registers listeners for multiple different properties on system object
/// 2. Triggers device changes that should fire multiple callbacks
/// 3. Tracks which thread each callback runs on
/// 4. Checks if all callbacks run on the same thread (serialized)
///
/// **Key question**: Does CoreAudio serialize all property callbacks on the same thread?
/// If YES: No additional synchronization needed between property callbacks
/// If NO: Need locks when callbacks access shared AudioUnitStream state
#[ignore]
#[test]
fn test_multiple_property_listeners_thread_serialization() {
    use std::collections::{HashMap, HashSet};

    struct ListenerState {
        // Track which thread each property's callbacks run on
        devices_property_threads: Mutex<Vec<u64>>,
        default_input_threads: Mutex<Vec<u64>>,
        default_output_threads: Mutex<Vec<u64>>,
        // Track callback counts
        devices_callback_count: AtomicU32,
        default_input_count: AtomicU32,
        default_output_count: AtomicU32,
        // Track if callbacks are running concurrently
        callbacks_running: AtomicU32, // Incremented on entry, decremented on exit
        max_concurrent_callbacks: AtomicU32,
    }

    impl ListenerState {
        fn new() -> Self {
            Self {
                devices_property_threads: Mutex::new(Vec::new()),
                default_input_threads: Mutex::new(Vec::new()),
                default_output_threads: Mutex::new(Vec::new()),
                devices_callback_count: AtomicU32::new(0),
                default_input_count: AtomicU32::new(0),
                default_output_count: AtomicU32::new(0),
                callbacks_running: AtomicU32::new(0),
                max_concurrent_callbacks: AtomicU32::new(0),
            }
        }
    }

    extern "C" fn devices_property_callback(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        let thread_id = get_thread_id();

        // Track concurrency
        let running = state.callbacks_running.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = state.max_concurrent_callbacks.load(Ordering::SeqCst);
        while running > max {
            match state.max_concurrent_callbacks.compare_exchange(
                max,
                running,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(x) => max = x,
            }
        }

        state.devices_callback_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut threads) = state.devices_property_threads.lock() {
            threads.push(thread_id);
        }

        thread::sleep(Duration::from_millis(10)); // Increase chance of overlap

        state.callbacks_running.fetch_sub(1, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn default_input_callback(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        let thread_id = get_thread_id();

        let running = state.callbacks_running.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = state.max_concurrent_callbacks.load(Ordering::SeqCst);
        while running > max {
            match state.max_concurrent_callbacks.compare_exchange(
                max,
                running,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(x) => max = x,
            }
        }

        state.default_input_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut threads) = state.default_input_threads.lock() {
            threads.push(thread_id);
        }

        thread::sleep(Duration::from_millis(10));

        state.callbacks_running.fetch_sub(1, Ordering::SeqCst);
        NO_ERR
    }

    extern "C" fn default_output_callback(
        _id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        let thread_id = get_thread_id();

        let running = state.callbacks_running.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = state.max_concurrent_callbacks.load(Ordering::SeqCst);
        while running > max {
            match state.max_concurrent_callbacks.compare_exchange(
                max,
                running,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(x) => max = x,
            }
        }

        state.default_output_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut threads) = state.default_output_threads.lock() {
            threads.push(thread_id);
        }

        thread::sleep(Duration::from_millis(10));

        state.callbacks_running.fetch_sub(1, Ordering::SeqCst);
        NO_ERR
    }

    println!("\n=== Multiple Property Listeners Thread Serialization Test ===\n");
    println!("This test simulates production code that monitors multiple properties:");
    println!("  - kAudioHardwarePropertyDevices");
    println!("  - kAudioHardwarePropertyDefaultInputDevice");
    println!("  - kAudioHardwarePropertyDefaultOutputDevice\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let state = Box::new(ListenerState::new());
    let state_ptr = Box::into_raw(state);

    // Register listeners for three different properties on system object
    let devices_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let default_input_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let default_output_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    println!("Registering listeners for three properties...");

    let status1 = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &devices_address,
            devices_property_callback,
            state_ptr as *mut c_void,
        )
    });

    let status2 = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &default_input_address,
            default_input_callback,
            state_ptr as *mut c_void,
        )
    });

    let status3 = run_serially(|| {
        audio_object_add_property_listener(
            kAudioObjectSystemObject,
            &default_output_address,
            default_output_callback,
            state_ptr as *mut c_void,
        )
    });

    if status1 != NO_ERR || status2 != NO_ERR || status3 != NO_ERR {
        println!(
            "Failed to register listeners (statuses: {}, {}, {}). Skipping test.",
            status1, status2, status3
        );
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!("All listeners registered successfully.\n");

    // Trigger device changes that should fire multiple callbacks
    println!("Triggering device changes by creating/destroying aggregate devices...");
    let mut created_devices = Vec::new();

    for i in 0..5 {
        if let Some(device_id) = manager.create_device(input_id, output_id) {
            println!("  Created aggregate device {} (iteration {})", device_id, i);
            created_devices.push(device_id);
            thread::sleep(Duration::from_millis(100));
        }
    }

    thread::sleep(Duration::from_millis(200));

    for device_id in &created_devices {
        manager.destroy_device(*device_id);
        thread::sleep(Duration::from_millis(100));
    }

    thread::sleep(Duration::from_millis(500));

    // Gather results
    let devices_count = unsafe { (*state_ptr).devices_callback_count.load(Ordering::SeqCst) };
    let input_count = unsafe { (*state_ptr).default_input_count.load(Ordering::SeqCst) };
    let output_count = unsafe { (*state_ptr).default_output_count.load(Ordering::SeqCst) };
    let max_concurrent = unsafe { (*state_ptr).max_concurrent_callbacks.load(Ordering::SeqCst) };

    let devices_threads = unsafe {
        (*state_ptr)
            .devices_property_threads
            .lock()
            .unwrap()
            .clone()
    };
    let input_threads = unsafe { (*state_ptr).default_input_threads.lock().unwrap().clone() };
    let output_threads = unsafe { (*state_ptr).default_output_threads.lock().unwrap().clone() };

    // Clean up listeners
    run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &devices_address,
            devices_property_callback,
            state_ptr as *mut c_void,
        )
    });

    run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &default_input_address,
            default_input_callback,
            state_ptr as *mut c_void,
        )
    });

    run_serially(|| {
        audio_object_remove_property_listener(
            kAudioObjectSystemObject,
            &default_output_address,
            default_output_callback,
            state_ptr as *mut c_void,
        )
    });

    println!("\n=== RESULTS ===\n");
    println!("Callback counts:");
    println!("  Devices property:        {}", devices_count);
    println!("  Default input property:  {}", input_count);
    println!("  Default output property: {}", output_count);
    println!(
        "  Total callbacks:         {}",
        devices_count + input_count + output_count
    );

    println!("\nConcurrency analysis:");
    println!("  Max concurrent callbacks: {}", max_concurrent);
    if max_concurrent > 1 {
        println!("  WARNING: Multiple callbacks ran concurrently!");
    } else {
        println!("  All callbacks ran serially (one at a time)");
    }

    // Analyze thread usage
    let mut all_threads = HashSet::new();
    let mut property_to_threads: HashMap<&str, HashSet<u64>> = HashMap::new();

    for tid in &devices_threads {
        all_threads.insert(*tid);
        property_to_threads
            .entry("Devices")
            .or_insert_with(HashSet::new)
            .insert(*tid);
    }
    for tid in &input_threads {
        all_threads.insert(*tid);
        property_to_threads
            .entry("DefaultInput")
            .or_insert_with(HashSet::new)
            .insert(*tid);
    }
    for tid in &output_threads {
        all_threads.insert(*tid);
        property_to_threads
            .entry("DefaultOutput")
            .or_insert_with(HashSet::new)
            .insert(*tid);
    }

    println!("\nThread analysis:");
    println!(
        "  Unique threads across all callbacks: {}",
        all_threads.len()
    );
    for (prop, threads) in &property_to_threads {
        println!(
            "  {} property used {} thread(s): {:?}",
            prop,
            threads.len(),
            threads
        );
    }

    println!("\n=== CONCLUSION ===\n");

    if all_threads.len() == 1 {
        println!("RESULT: All property callbacks run on THE SAME thread.");
        println!("Thread ID: {}", all_threads.iter().next().unwrap());
    } else {
        println!("RESULT: Property callbacks run on DIFFERENT threads!");
        println!("This is unexpected and may require synchronization.");
    }

    if max_concurrent == 0 {
        println!("\nSERIALIZATION: All callbacks ran serially (no overlap detected).");
        println!("IMPLICATION: CoreAudio appears to serialize property callbacks.");
        println!("             No additional locking needed between property callbacks.");
    } else if max_concurrent == 1 {
        println!("\nSERIALIZATION: Callbacks ran one at a time (serialized).");
        println!("IMPLICATION: Safe to access shared state without locks.");
    } else {
        println!(
            "\nCONCURRENCY: Up to {} callbacks ran concurrently!",
            max_concurrent
        );
        println!(
            "IMPLICATION: CRITICAL - Must use locks when accessing shared AudioUnitStream state!"
        );
    }

    println!("\nRELEVANCE TO PRODUCTION CODE:");
    println!("  - AudioUnitStream registers multiple listeners per device");
    println!(
        "  - If serialized: audiounit_property_listener_callback can safely access stream state"
    );
    println!("  - If concurrent: Need locks around shared state access in callbacks");

    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}

/// Test callback behavior when the device associated with the listener is removed.
#[ignore]
#[test]
fn test_callback_behavior_with_dead_device() {
    struct ListenerState {
        callback_count: AtomicU32,
        device_removed: AtomicBool,
        callbacks_after_device_removal: AtomicU32,
    }

    impl ListenerState {
        fn new() -> Self {
            Self {
                callback_count: AtomicU32::new(0),
                device_removed: AtomicBool::new(false),
                callbacks_after_device_removal: AtomicU32::new(0),
            }
        }
    }

    extern "C" fn property_listener(
        id: AudioObjectID,
        _number_of_addresses: u32,
        _addresses: *const AudioObjectPropertyAddress,
        data: *mut c_void,
    ) -> OSStatus {
        let state = unsafe { &*(data as *const ListenerState) };
        state.callback_count.fetch_add(1, Ordering::SeqCst);

        println!("  Callback fired for device {}", id);

        if state.device_removed.load(Ordering::SeqCst) {
            state
                .callbacks_after_device_removal
                .fetch_add(1, Ordering::SeqCst);
        }

        NO_ERR
    }

    println!("\n=== Dead Device Callback Behavior Test ===\n");

    let input_device = test_get_default_device(Scope::Input);
    let output_device = test_get_default_device(Scope::Output);

    if input_device.is_none() || output_device.is_none() {
        println!("No input or output device available. Skipping test.");
        return;
    }

    let input_id = input_device.unwrap();
    let output_id = output_device.unwrap();

    let manager = match TestAggregateDeviceManager::new() {
        Some(m) => m,
        None => {
            println!("Could not get system plugin. Skipping test.");
            return;
        }
    };

    let device_id = match manager.create_device(input_id, output_id) {
        Some(id) => id,
        None => {
            println!("Could not create aggregate device. Skipping test.");
            return;
        }
    };

    println!("Created aggregate device: {}", device_id);

    let state = Box::new(ListenerState::new());
    let state_ptr = Box::into_raw(state);

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceIsAlive,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };

    let status = run_serially(|| {
        audio_object_add_property_listener(
            device_id,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });

    if status != NO_ERR {
        println!(
            "Could not add property listener to device (status={}). Skipping.",
            status
        );
        manager.destroy_device(device_id);
        unsafe { drop(Box::from_raw(state_ptr)) };
        return;
    }

    println!("Listener registered on device {}.", device_id);
    println!("Destroying the device...\n");

    unsafe { (*state_ptr).device_removed.store(true, Ordering::SeqCst) };
    manager.destroy_device(device_id);

    thread::sleep(Duration::from_millis(500));

    let total_count = unsafe { (*state_ptr).callback_count.load(Ordering::SeqCst) };
    let after_removal = unsafe {
        (*state_ptr)
            .callbacks_after_device_removal
            .load(Ordering::SeqCst)
    };

    println!("\n=== RESULTS ===");
    println!("Total callbacks: {}", total_count);
    println!("Callbacks after device removal: {}", after_removal);

    if total_count > 0 {
        println!("\nDevice destruction triggered callback (expected for kAudioDevicePropertyDeviceIsAlive).");
    }

    println!("\nAttempting to remove listener from dead device...");
    let status = run_serially(|| {
        audio_object_remove_property_listener(
            device_id,
            &address,
            property_listener,
            state_ptr as *mut c_void,
        )
    });

    println!("Remove status: {} (expected error for dead device)", status);

    if status != NO_ERR {
        println!("\nIMPLICATION: Removing listener from dead device fails.");
        println!("If the device dies before listener removal, the removal will fail,");
        println!("but the listener is effectively orphaned (device no longer exists).");
    }

    unsafe { drop(Box::from_raw(state_ptr)) };

    println!("\n=== Test Complete ===\n");
}
