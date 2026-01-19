use anyhow::Result;
use serialport;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

mod audio;
mod cli;
mod gui;
mod rigctl;
mod shutdown;
mod trusdx;

fn main() -> Result<()> {
    audio::cleanup_trusdx_audio_interfaces();

    let _mid = audio::create_trusdx_audio_interface(11520);

    let mut port = trusdx::open_trusdx_serial()?;

    let _ = trusdx::control_rts(&mut *port, false);
    let _ = trusdx::control_dtr(&mut *port, true);
    let ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>> = Arc::new(Mutex::new(port));

    {
        let mut s = ser.lock().unwrap();
        let _ = trusdx::enable_streaming_speaker_off(&mut **s);
        let _ = trusdx::set_mode(&mut **s, 2);
    }
    let streaming_started = Arc::new(AtomicBool::new(false));
    {
        let mut attempts = 0;
        loop {
            let start = std::time::Instant::now();
            while !streaming_started.load(Ordering::Relaxed)
                && start.elapsed() < Duration::from_millis(250)
            {
                thread::sleep(Duration::from_millis(10));
            }
            // Check if streaming started successfully
            if streaming_started.load(Ordering::Relaxed) {
                break;
            }
            // Check if maximum retry attempts reached
            if attempts >= 2 {
                break;
            }
            attempts += 1;
            // Check if serial port lock acquired successfully
            if let Ok(mut s) = ser.lock() {
                let _ = trusdx::enable_streaming_speaker_off(&mut **s);
            }
        }
    }

    let freq_state = Arc::new(Mutex::new(0u64));
    let tx_state = Arc::new(Mutex::new(false));
    let mode_state = Arc::new(Mutex::new(String::from("USB")));
    let audio = audio::setup_pulseaudio(7812, 11520)?;
    let stop_audio = Arc::new(Mutex::new(false));
    let input_level = Arc::new(Mutex::new(0.0f32));
    let output_level = Arc::new(Mutex::new(0.0f32));
    let cat_queue: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    audio::run_audio_bridge(
        ser.clone(),
        audio,
        stop_audio.clone(),
        input_level.clone(),
        output_level.clone(),
        freq_state.clone(),
        mode_state.clone(),
        tx_state.clone(),
        cat_queue.clone(),
        streaming_started.clone(),
    );

    rigctl::spawn_rigctl_server(
        ser.clone(),
        freq_state.clone(),
        tx_state.clone(),
        cat_queue.clone(),
    );

    let shutting_down = Arc::new(AtomicBool::new(false));
    cli::spawn_esc_handler(shutting_down.clone(), ser.clone(), stop_audio.clone());

    gui::spawn_gui(
        input_level.clone(),
        output_level.clone(),
        shutting_down.clone(),
        ser.clone(),
        stop_audio.clone(),
        freq_state.clone(),
        mode_state.clone(),
        tx_state.clone(),
    );

    cli::print_console_header();
    let mut last_poll = std::time::Instant::now();
    let mut prev_tx_state = false;
    let mut last_tx_end = std::time::Instant::now() - std::time::Duration::from_secs(5);

    loop {
        // Check if shutdown flag is set
        if shutting_down.load(Ordering::Relaxed) {
            break;
        }
        let in_lvl = *input_level.lock().unwrap();
        let out_lvl = *output_level.lock().unwrap();
        let freq = *freq_state.lock().unwrap();
        let mode = mode_state.lock().unwrap().clone();
        let tx_now = *tx_state.lock().unwrap();
        // Check if TX just ended (transition from TX to RX)
        if prev_tx_state && !tx_now {
            last_tx_end = std::time::Instant::now();
        }
        prev_tx_state = tx_now;
        cli::render_levels(in_lvl, out_lvl, freq, &mode, tx_now);
        thread::sleep(Duration::from_millis(10));
        // Check if in RX mode and poll interval elapsed
        if !tx_now && last_poll.elapsed() >= Duration::from_secs(2) {
            // Check if enough time passed since last TX ended
            if std::time::Instant::now().duration_since(last_tx_end) >= Duration::from_millis(500) {
                // Check if serial port lock acquired successfully
                if let Ok(mut s) = ser.lock() {
                    let _ = trusdx::query_vfo_a(&mut **s);
                }
            }
            last_poll = std::time::Instant::now();
        }
    }

    print!("\x1B[2J\x1B[H");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    audio::cleanup_trusdx_audio_interfaces();
    Ok(())
}
