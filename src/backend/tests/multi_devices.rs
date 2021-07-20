use super::utils::{test_get_devices_in_scope, Scope};
use super::*;
use std::collections::HashSet;

#[ignore]
#[test]
fn test_create_one_duplex_and_one_input_with_three_different_devices() {
    let input_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Input)
        .iter()
        .cloned()
        .collect();
    let output_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Output)
        .iter()
        .cloned()
        .collect();
    let input_only_devices: HashSet<AudioObjectID> =
        input_devices.difference(&output_devices).cloned().collect();
    let output_only_devices: HashSet<AudioObjectID> =
        output_devices.difference(&input_devices).cloned().collect();

    // Duplex stream whose input runs on X, output on Y. Anpother input stream runs on Z
    if input_only_devices.len() >= 2 && output_only_devices.len() >= 1 {
        let duplex_input = input_only_devices.iter().nth(0).unwrap();
        let input = input_only_devices.iter().nth(1).unwrap();
        let duplex_output = output_only_devices.iter().nth(0).unwrap();
        assert_ne!(duplex_input, duplex_output);
        assert_ne!(duplex_input, input);
        assert_ne!(input, duplex_output);
        create_one_duplex_and_one_input(duplex_input.clone(), duplex_output.clone(), input.clone());
    }
}

#[ignore]
#[test]
fn test_create_one_duplex_and_one_input_with_duplex_on_one_input_on_another() {
    let input_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Input)
        .iter()
        .cloned()
        .collect();
    let output_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Output)
        .iter()
        .cloned()
        .collect();
    let inout_devices: HashSet<AudioObjectID> = input_devices
        .intersection(&output_devices)
        .cloned()
        .collect();

    // Duplex stream runs on one device. Input stream runs on another
    if inout_devices.len() >= 1 && input_devices.len() >= 2 {
        let duplex = inout_devices.iter().nth(0).unwrap();
        let mut inputs = input_devices.clone();
        inputs.remove(&duplex);
        let input = inputs.iter().nth(0).unwrap();
        assert_ne!(duplex, input);
        create_one_duplex_and_one_input(duplex.clone(), duplex.clone(), input.clone());
    }
}

#[ignore]
#[test]
fn test_create_one_duplex_and_one_input_with_same_input_device_but_different_output() {
    let input_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Input)
        .iter()
        .cloned()
        .collect();
    let output_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Output)
        .iter()
        .cloned()
        .collect();

    // Duplex input and output is X and Y. Another input stream runs on X
    if input_devices.len() >= 1 && output_devices.len() >= 1 {
        let mut outputs = output_devices.clone();
        for input in input_devices.iter() {
            let removed = outputs.remove(input);
            if outputs.len() >= 1 {
                let output = outputs.iter().nth(0).unwrap();
                create_one_duplex_and_one_input(input.clone(), output.clone(), input.clone());
                break;
            }
            if removed {
                outputs.insert(input.clone());
            }
        }
    }
}

#[ignore]
#[test]
fn test_create_one_duplex_and_one_input_with_same_output_device_but_different_input() {
    let input_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Input)
        .iter()
        .cloned()
        .collect();
    let output_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Output)
        .iter()
        .cloned()
        .collect();
    let inout_devices: HashSet<AudioObjectID> = input_devices
        .intersection(&output_devices)
        .cloned()
        .collect();

    // Duplex input and output is X and Y. Another input stream runs on Y
    if inout_devices.len() >= 1 && input_devices.len() >= 2 {
        let duplex_output = inout_devices.iter().nth(0).unwrap();
        let input = duplex_output;
        let mut inputs = input_devices.clone();
        inputs.remove(&input);
        let duplex_input = inputs.iter().nth(0).unwrap();
        assert_ne!(duplex_input, duplex_output);
        create_one_duplex_and_one_input(duplex_input.clone(), duplex_output.clone(), input.clone());
    }
}

#[ignore]
#[test]
fn test_create_one_duplex_and_one_input_same_device() {
    let input_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Input)
        .iter()
        .cloned()
        .collect();
    let output_devices: HashSet<AudioObjectID> = test_get_devices_in_scope(Scope::Output)
        .iter()
        .cloned()
        .collect();
    let inout_devices: HashSet<AudioObjectID> = input_devices
        .intersection(&output_devices)
        .cloned()
        .collect();

    // Duplex stream and input stream all run on the same device
    if inout_devices.len() >= 1 {
        let dev = inout_devices.iter().nth(0).unwrap();
        create_one_duplex_and_one_input(dev.clone(), dev.clone(), dev.clone());
    }
}

fn create_one_duplex_and_one_input(
    duplex_input_dev: AudioObjectID,
    duplex_output_dev: AudioObjectID,
    input_dev: AudioObjectID,
) {
    println!(
        "create_one_duplex_and_one_input: duplex-input {}, duplex-output {}, input {}",
        duplex_input_dev, duplex_output_dev, input_dev
    );
    // Make sure the parameters meet the requirements of AudioUnitContext::stream_init (in the comments).
    let mut stream_input_params = ffi::cubeb_stream_params::default();
    stream_input_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
    stream_input_params.rate = 48_000;
    stream_input_params.channels = 1;
    stream_input_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
    stream_input_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

    let mut stream_output_params = ffi::cubeb_stream_params::default();
    stream_output_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
    stream_output_params.rate = 44100;
    stream_output_params.channels = 2;
    stream_output_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
    stream_output_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

    let input_params = unsafe { StreamParamsRef::from_ptr(&mut stream_input_params) };
    let output_params = unsafe { StreamParamsRef::from_ptr(&mut stream_output_params) };

    let mut context = AudioUnitContext::new();
    let _duplex_stream = context
        .stream_init(
            None,
            duplex_input_dev as DeviceId,
            Some(input_params),
            duplex_output_dev as DeviceId,
            Some(output_params),
            SAFE_MIN_LATENCY_FRAMES,
            Some(duplex_data_callback),
            Some(state_callback),
            ptr::null_mut(),
        )
        .unwrap();

    let _input_stream = context
        .stream_init(
            None,
            input_dev as DeviceId,
            Some(input_params),
            ptr::null_mut(),
            None,
            SAFE_MIN_LATENCY_FRAMES,
            Some(input_data_callback),
            Some(state_callback),
            ptr::null_mut(),
        )
        .unwrap();

    extern "C" fn state_callback(
        stream: *mut ffi::cubeb_stream,
        _user_ptr: *mut c_void,
        state: ffi::cubeb_state,
    ) {
        assert!(!stream.is_null());
        assert_ne!(state, ffi::CUBEB_STATE_ERROR);
    }

    extern "C" fn duplex_data_callback(
        stream: *mut ffi::cubeb_stream,
        _user_ptr: *mut c_void,
        input_buffer: *const c_void,
        output_buffer: *mut c_void,
        nframes: i64,
    ) -> i64 {
        assert!(!stream.is_null());
        assert!(!input_buffer.is_null());
        assert!(!output_buffer.is_null());

        // Feed silence data to output buffer
        if !output_buffer.is_null() {
            let stm = unsafe { &mut *(stream as *mut AudioUnitStream) };
            let channels = stm.core_stream_data.output_stream_params.channels();
            let samples = nframes as usize * channels as usize;
            let sample_size = cubeb_sample_size(stm.core_stream_data.output_stream_params.format());
            unsafe {
                ptr::write_bytes(output_buffer, 0, samples * sample_size);
            }
        }

        nframes
    }

    extern "C" fn input_data_callback(
        stream: *mut ffi::cubeb_stream,
        _user_ptr: *mut c_void,
        input_buffer: *const c_void,
        output_buffer: *mut c_void,
        nframes: i64,
    ) -> i64 {
        assert!(!stream.is_null());
        assert!(!input_buffer.is_null());
        assert!(output_buffer.is_null());
        nframes
    }
}
