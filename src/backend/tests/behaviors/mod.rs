//! Tests for observing and documenting CoreAudio API behaviors.
//!
//! These tests investigate and document how CoreAudio APIs behave in practice.
//! They serve as executable documentation and help verify assumptions about
//! undocumented or poorly documented system behaviors.
//!
//! **Purpose**: These tests help developers understand:
//! - How CoreAudio APIs actually behave (vs. what documentation says or implies)
//! - Threading and synchronization guarantees provided by the system
//! - Edge cases and platform-specific behaviors
//!
//! ## Modules
//!
//! - `audiounit` - AudioUnit render callback synchronization tests
//! - `property_listener` - AudioObject property listener synchronization tests
//!
//! **Running these tests**:
//! ```sh
//! # Run all behavior observation tests
//! cargo test behaviors -- --ignored --nocapture
//!
//! # Run AudioUnit behavior tests
//! cargo test behaviors::audiounit -- --ignored --nocapture
//!
//! # Run property listener behavior tests
//! cargo test behaviors::property_listener -- --ignored --nocapture
//! ```

use super::*;

mod audiounit;
mod property_listener;

fn get_thread_id() -> u64 {
    unsafe {
        let mut thread_id: u64 = 0;
        libc::pthread_threadid_np(libc::pthread_self(), &mut thread_id);
        thread_id
    }
}
