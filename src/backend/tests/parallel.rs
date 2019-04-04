use super::utils::{test_get_default_device, test_ops_context_operation, Scope};
use super::*;
use std::thread;

// Ignore the test by default to avoid overwritting the buffer frame size of the device that is
// currently used by other streams in other tests.
#[ignore]
#[test]
fn test_parallel_ops_init_streams_in_parallel() {
    const THREADS: u32 = 10;

    let default_input = test_get_default_device(Scope::Input);
    let default_output = test_get_default_device(Scope::Output);
    if default_input.is_none() || default_output.is_none() {
        println!("No default input or output device to perform the test of creating duplex streams in parallel");
        return;
    }

    test_ops_context_operation("context: init and destroy", |context_ptr| {
        let context_ptr_value = context_ptr as usize;

        let mut join_handles = vec![];
        for i in 0..THREADS {
            // Latency cannot be changed if another stream is operating in parallel. All the latecy
            // should be set to the same latency value of the first stream that is operating in the
            // context.
            let latency_frames = SAFE_MIN_LATENCY_FRAMES + i;
            assert!(latency_frames < SAFE_MAX_LATENCY_FRAMES);

            // Make sure the parameters meet the requirements of AudioUnitContext::stream_init
            // (in the comments).
            let mut input_params = ffi::cubeb_stream_params::default();
            input_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
            input_params.rate = 48_000;
            input_params.channels = 1;
            input_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
            input_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

            let mut output_params = ffi::cubeb_stream_params::default();
            output_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
            output_params.rate = 44100;
            output_params.channels = 2;
            output_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
            output_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

            // Create many streams within the same context. The order of the stream creation
            // is random (The order of execution of the spawned threads is random.).assert!
            // It's super dangerous to pass `context_ptr_value` across threads and convert it back
            // to a pointer. However, it's a direct way to make sure the inside mutex works.
            let thread_name = format!("stream {} @ context {:?}", i, context_ptr);
            join_handles.push(
                thread::Builder::new()
                    .name(thread_name)
                    .spawn(move || {
                        let context_ptr = context_ptr_value as *mut ffi::cubeb;
                        let mut stream: *mut ffi::cubeb_stream = ptr::null_mut();
                        let stream_name = CString::new(format!("stream {}", i)).unwrap();
                        assert_eq!(
                            unsafe {
                                OPS.stream_init.unwrap()(
                                    context_ptr,
                                    &mut stream,
                                    stream_name.as_ptr(),
                                    ptr::null_mut(), // Use default input device.
                                    &mut input_params,
                                    ptr::null_mut(), // Use default output device.
                                    &mut output_params,
                                    latency_frames,
                                    None,            // No data callback.
                                    None,            // No state callback.
                                    ptr::null_mut(), // No user data pointer.
                                )
                            },
                            ffi::CUBEB_OK
                        );
                        assert!(!stream.is_null());
                        stream as usize
                    })
                    .unwrap(),
            );
        }

        // All the latency frames should be the same value as the first stream's one, since the
        // latency frames cannot be changed if another stream is operating in parallel.
        let mut latency_frames = vec![];

        for handle in join_handles {
            let stream_ptr_value = handle.join().unwrap();
            let stream = unsafe { Box::from_raw(stream_ptr_value as *mut AudioUnitStream) };
            // There is no need to call `stream_destroy`. The `stream` created is leaked from a Box
            // and it's retaken within a Box now. It will be destroyed/dropped automatically.

            latency_frames.push(stream.latency_frames);
        }

        // Make sure all the latency frames are same.
        for i in 0..latency_frames.len() - 1 {
            assert_eq!(latency_frames[i], latency_frames[i + 1]);
        }
    });
}

// Ignore the test by default to avoid overwritting the buffer frame size of the device that is
// currently used by other streams in other tests.
#[ignore]
#[test]
fn test_parallel_init_streams_in_parallel() {
    const THREADS: u32 = 10;

    let default_input = test_get_default_device(Scope::Input);
    let default_output = test_get_default_device(Scope::Output);
    if default_input.is_none() || default_output.is_none() {
        println!("No default input or output device to perform the test of creating duplex streams in parallel");
        return;
    }

    // Initialize the the mutex (whose type is OwnedCriticalSection) within AudioUnitContext,
    // by AudioUnitContext::Init, to make the mutex work.
    let mut context = AudioUnitContext::new();
    context.init();

    let context_ptr_value = &context as *const AudioUnitContext as usize;

    let mut join_handles = vec![];
    for i in 0..THREADS {
        // Latency cannot be changed if another stream is operating in parallel. All the latecy
        // should be set to the same latency value of the first stream that is operating in the
        // context.
        let latency_frames = SAFE_MIN_LATENCY_FRAMES + i;
        assert!(latency_frames < SAFE_MAX_LATENCY_FRAMES);

        // Make sure the parameters meet the requirements of AudioUnitContext::stream_init
        // (in the comments).
        let mut input_params = ffi::cubeb_stream_params::default();
        input_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
        input_params.rate = 48_000;
        input_params.channels = 1;
        input_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
        input_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

        let mut output_params = ffi::cubeb_stream_params::default();
        output_params.format = ffi::CUBEB_SAMPLE_FLOAT32NE;
        output_params.rate = 44100;
        output_params.channels = 2;
        output_params.layout = ffi::CUBEB_LAYOUT_UNDEFINED;
        output_params.prefs = ffi::CUBEB_STREAM_PREF_NONE;

        // Create many streams within the same context. The order of the stream creation
        // is random. (The order of execution of the spawned threads is random.)
        // It's super dangerous to pass `context_ptr_value` across threads and convert it back
        // to a reference. However, it's a direct way to make sure the inside mutex works.
        let thread_name = format!("stream {} @ context {:?}", i, context_ptr_value);
        join_handles.push(
            thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    let context = unsafe { &mut *(context_ptr_value as *mut AudioUnitContext) };
                    let input_params = unsafe { StreamParamsRef::from_ptr(&mut input_params) };
                    let output_params = unsafe { StreamParamsRef::from_ptr(&mut output_params) };
                    let stream = context.stream_init(
                        None,
                        ptr::null_mut(), // Use default input device.
                        Some(input_params),
                        ptr::null_mut(), // Use default output device.
                        Some(output_params),
                        latency_frames,
                        None,            // No data callback.
                        None,            // No state callback.
                        ptr::null_mut(), // No user data pointer.
                    ).unwrap();
                    let stream_ptr_value = stream.as_ptr() as usize;
                    // Prevent the stream from being destroyed by leaking this stream.
                    mem::forget(stream);
                    stream_ptr_value
                })
                .unwrap(),
        );
    }

    // All the latency frames should be the same value as the first stream's one, since the
    // latency frames cannot be changed if another stream is operating in parallel.
    let mut latency_frames = vec![];

    for handle in join_handles {
        let stream_ptr_value = handle.join().unwrap();
        // Retake the leaked stream.
        let stream = unsafe { Box::from_raw(stream_ptr_value as *mut AudioUnitStream) };
        latency_frames.push(stream.latency_frames);
    }

    // Make sure all the latency frames are same.
    for i in 0..latency_frames.len() - 1 {
        assert_eq!(latency_frames[i], latency_frames[i + 1]);
    }
}