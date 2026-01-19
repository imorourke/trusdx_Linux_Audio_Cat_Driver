use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{Builder, Label, ProgressBar, Window};

const GLADE_UI: &str = include_str!("gui.glade");

pub fn load_glade_file() -> Result<Builder, String> {
    Ok(Builder::from_string(GLADE_UI))
}

pub fn setup_gui(
    input_level: Arc<Mutex<f32>>,
    output_level: Arc<Mutex<f32>>,
    shutting_down: Arc<AtomicBool>,
    ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    stop_audio: Arc<Mutex<bool>>,
    freq_state: Arc<Mutex<u64>>,
    mode_state: Arc<Mutex<String>>,
    tx_state: Arc<Mutex<bool>>,
) -> Result<(), String> {
    let display = std::env::var("DISPLAY").ok();
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();

    // Check if neither X11 nor Wayland display is available
    if display.is_none() && wayland.is_none() {
        return Err("Neither DISPLAY nor WAYLAND_DISPLAY environment variables are set. Cannot initialize GUI.".to_string());
    }

    gtk::init().map_err(|e| format!("Failed to initialize GTK: {:?}", e))?;

    let builder = load_glade_file()?;

    let window: Window = builder
        .object("window")
        .ok_or("Could not find window object in glade file")?;

    let prog_tx_level: ProgressBar = builder
        .object("prog_tx_level")
        .ok_or("Could not find prog_tx_level progress bar")?;

    let prog_rx_level: ProgressBar = builder
        .object("prog_rx_level")
        .ok_or("Could not find prog_rx_level progress bar")?;

    let lbl_freq: Label = builder
        .object("lblFreq")
        .ok_or("Could not find lblFreq label in glade file")?;

    let lbl_mode: Label = builder
        .object("lblMode")
        .ok_or("Could not find lblMode label in glade file")?;

    let lbl_state: Label = builder
        .object("lblState")
        .ok_or("Could not find lblState label in glade file")?;

    prog_tx_level.set_show_text(true);
    prog_rx_level.set_show_text(true);

    window.set_resizable(false);

    let shutting_down_clone = shutting_down.clone();
    let stop_audio_clone = stop_audio.clone();
    let ser_clone = ser.clone();
    window.connect_delete_event(move |_, _| {
        crate::shutdown::shutdown(
            shutting_down_clone.clone(),
            ser_clone.clone(),
            stop_audio_clone.clone(),
        );
        gtk::main_quit();
        gtk::glib::Propagation::Stop
    });

    window.show_all();

    let shutting_down_for_timeout = shutting_down.clone();
    let input_level_for_timeout = input_level.clone();
    let output_level_for_timeout = output_level.clone();
    let freq_state_for_timeout = freq_state.clone();
    let mode_state_for_timeout = mode_state.clone();
    let tx_state_for_timeout = tx_state.clone();
    let prog_tx_for_timeout = prog_tx_level.clone();
    let prog_rx_for_timeout = prog_rx_level.clone();
    let lbl_freq_for_timeout = lbl_freq.clone();
    let lbl_mode_for_timeout = lbl_mode.clone();
    let lbl_state_for_timeout = lbl_state.clone();

    let _ = glib::timeout_add_local(Duration::from_millis(50), move || {
        // Check if shutdown flag is set
        if shutting_down_for_timeout.load(Ordering::Relaxed) {
            return glib::ControlFlow::Break;
        }

        let in_lvl = *input_level_for_timeout.lock().unwrap();
        let out_lvl = *output_level_for_timeout.lock().unwrap();

        prog_rx_for_timeout.set_fraction(in_lvl.clamp(0.0, 1.0) as f64);
        let rx_text = format!("{:.1}%", in_lvl * 100.0);
        prog_rx_for_timeout.set_text(Some(&rx_text));

        prog_tx_for_timeout.set_fraction(out_lvl.clamp(0.0, 1.0) as f64);
        let tx_text = format!("{:.1}%", out_lvl * 100.0);
        prog_tx_for_timeout.set_text(Some(&tx_text));

        let freq = *freq_state_for_timeout.lock().unwrap();
        let mode = mode_state_for_timeout.lock().unwrap().clone();
        let tx_now = *tx_state_for_timeout.lock().unwrap();
        let freq_mhz = (freq as f64) / 1_000_000.0f64;
        let freq_text = format!("{:.5} MHz", freq_mhz);
        lbl_freq_for_timeout.set_text(&freq_text);
        lbl_mode_for_timeout.set_text(&mode);
        let state_text = if tx_now { "TX" } else { "RX" };
        lbl_state_for_timeout.set_text(state_text);

        glib::ControlFlow::Continue
    });

    gtk::main();

    Ok(())
}

pub fn spawn_gui(
    input_level: Arc<Mutex<f32>>,
    output_level: Arc<Mutex<f32>>,
    shutting_down: Arc<AtomicBool>,
    ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    stop_audio: Arc<Mutex<bool>>,
    freq_state: Arc<Mutex<u64>>,
    mode_state: Arc<Mutex<String>>,
    tx_state: Arc<Mutex<bool>>,
) {
    thread::spawn(move || {
        // Check if GUI setup failed
        if let Err(e) = setup_gui(
            input_level,
            output_level,
            shutting_down,
            ser,
            stop_audio,
            freq_state,
            mode_state,
            tx_state,
        ) {
            eprintln!("GUI error: {}", e);
        }
    });
}
