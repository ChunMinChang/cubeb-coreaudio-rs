#!/usr/sbin/dtrace -s
/*
 * Trace HALB_Mutex contention during CoreAudio sync operations.
 *
 * Measures actual Lock() duration — long durations indicate the thread was
 * blocked waiting for another thread to release the mutex.
 *
 * Usage: sudo dtrace -s trace_contention.d -c "<test_binary> <test_name> ..."
 *    or: sudo ./trace_contention.sh <test_name>
 *
 * Requires sudo (dtrace needs root) AND SIP disabled for dtrace:
 *   csrutil enable --without dtrace   (from Recovery Mode)
 * With SIP enabled, pid provider cannot probe Apple framework symbols
 * (CoreAudio, HALB_Mutex) and dtrace will fail to compile.
 * For SIP-enabled machines, use trace_mutex.sh (LLDB-based) instead.
 */

#pragma D option quiet
#pragma D option switchrate=10hz

dtrace:::BEGIN
{
    printf("TRACE: Contention tracer started\n");
    in_sync = 0;
}

/* --- Sync operations: entry/return with duration --- */

pid$target::AudioOutputUnitStop:entry
{
    self->sync_start = timestamp;
    self->sync_name = "AudioOutputUnitStop";
    in_sync = 1;
    printf("TRACE: %8d.%06d  T%-4d  AudioOutputUnitStop ENTER\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid);
}

pid$target::AudioOutputUnitStop:return
/self->sync_start/
{
    this->dur_us = (timestamp - self->sync_start) / 1000;
    printf("TRACE: %8d.%06d  T%-4d  AudioOutputUnitStop RETURN  (%d us)\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid, this->dur_us);
    self->sync_start = 0;
    in_sync = 0;
}

pid$target::AudioObjectRemovePropertyListener:entry
{
    self->sync_start = timestamp;
    self->sync_name = "AudioObjectRemovePropertyListener";
    in_sync = 1;
    printf("TRACE: %8d.%06d  T%-4d  AudioObjectRemovePropertyListener ENTER\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid);
}

pid$target::AudioObjectRemovePropertyListener:return
/self->sync_start/
{
    this->dur_us = (timestamp - self->sync_start) / 1000;
    printf("TRACE: %8d.%06d  T%-4d  AudioObjectRemovePropertyListener RETURN  (%d us)\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid, this->dur_us);
    self->sync_start = 0;
    in_sync = 0;
}

pid$target::AudioComponentInstanceDispose:entry
{
    self->sync_start = timestamp;
    self->sync_name = "AudioComponentInstanceDispose";
    in_sync = 1;
    printf("TRACE: %8d.%06d  T%-4d  AudioComponentInstanceDispose ENTER\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid);
}

pid$target::AudioComponentInstanceDispose:return
/self->sync_start/
{
    this->dur_us = (timestamp - self->sync_start) / 1000;
    printf("TRACE: %8d.%06d  T%-4d  AudioComponentInstanceDispose RETURN  (%d us)\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid, this->dur_us);
    self->sync_start = 0;
    in_sync = 0;
}

pid$target::AudioUnitUninitialize:entry
{
    self->sync_start = timestamp;
    self->sync_name = "AudioUnitUninitialize";
    in_sync = 1;
    printf("TRACE: %8d.%06d  T%-4d  AudioUnitUninitialize ENTER\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid);
}

pid$target::AudioUnitUninitialize:return
/self->sync_start/
{
    this->dur_us = (timestamp - self->sync_start) / 1000;
    printf("TRACE: %8d.%06d  T%-4d  AudioUnitUninitialize RETURN  (%d us)\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid, this->dur_us);
    self->sync_start = 0;
    in_sync = 0;
}

/* --- HALB_Mutex::Lock contention measurement --- */
/* Long Lock() duration = thread was blocked waiting for the mutex. */

pid$target:CoreAudio:_ZN10HALB_Mutex4LockEv:entry
{
    self->lock_start = timestamp;
    self->lock_addr = arg0;
}

pid$target:CoreAudio:_ZN10HALB_Mutex4LockEv:return
/self->lock_start/
{
    this->dur_us = (timestamp - self->lock_start) / 1000;
    printf("TRACE: %8d.%06d  T%-4d  HALB_Mutex::Lock  0x%p  (%d us)%s\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid,
        self->lock_addr, this->dur_us,
        this->dur_us > 100 ? "  <-- CONTENTION" : "");
    self->lock_start = 0;
}

pid$target:CoreAudio:_ZN10HALB_Mutex6UnlockEv:entry
{
    printf("TRACE: %8d.%06d  T%-4d  HALB_Mutex::Unlock  0x%p\n",
        walltimestamp / 1000000000, (walltimestamp / 1000) % 1000000, tid, arg0);
}

dtrace:::END
{
    printf("\nTRACE: Contention tracer finished\n");
}
