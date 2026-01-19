use serialport;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub fn shutdown(
    shutting_down: Arc<AtomicBool>,
    ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    stop_audio: Arc<Mutex<bool>>,
) {
    shutting_down.store(true, Ordering::Relaxed);

    *stop_audio.lock().unwrap() = true;

    // Check if serial port lock acquired successfully
    if let Ok(mut s) = ser.lock() {
        let _ = crate::trusdx::enable_streaming_speaker_on(&mut **s);
        let _ = s.flush();
    }
}
