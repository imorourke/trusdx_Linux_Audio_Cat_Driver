use std::io::Write;
use std::process::Command;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use libpulse_binding as pulse;
use libpulse_binding::def::BufferAttr;
use libpulse_simple_binding as psimple;
use serialport;

// Constants
const TX_FALLING_DELAY_MS: u64 = 30;
const STREAMING_TIMEOUT_MS: u64 = 200;
const STREAMING_RETRY_TIMEOUT_MS: u64 = 100;
const POLL_INTERVAL_MS: u64 = 5;
const DRAIN_ATTEMPTS: usize = 10;
const MIN_RMS_THRESHOLD: f32 = 0.05;
const INPUT_LEVEL_SCALE: f32 = 2.1;
const TX_GAIN: f32 = 1.0;
const WAVE_BUF_MAX_SIZE: usize = 512;
const AUDIO_SAMPLE_SIZE: usize = 48;
const I16_TO_F32_SCALE: f32 = 32768.0;
const U8_TO_F32_OFFSET: f32 = 128.0;
const U8_TO_F32_SCALE: f32 = 128.0;

pub fn cleanup_trusdx_audio_interfaces() {
    // Check if pactl command executed successfully
    if let Ok(output) = Command::new("pactl")
        .arg("list")
        .arg("short")
        .arg("modules")
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Check if line contains TRUSDX module identifier
            if line.contains("TRUSDX") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // Check if module ID exists in parsed line
                if let Some(module_id) = parts.first() {
                    let _ = Command::new("pactl")
                        .arg("unload-module")
                        .arg(module_id)
                        .status();
                }
            }
        }
    }
}

pub fn create_trusdx_audio_interface(_audio_tx_rate: u32) -> Option<u32> {
    let sink_output = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            "sink_name=TRUSDX",
            "sink_properties=device.description=\"TRUSDX Audio\"",
        ])
        .output();

    sink_output
        .ok()
        // Check if command execution was successful
        .filter(|result| result.status.success())
        .and_then(|result| {
            String::from_utf8_lossy(&result.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
}

#[derive(Clone)]
pub struct AudioHandles {
    pub pa_playback: Arc<psimple::Simple>,
    pub pa_record: Arc<psimple::Simple>,
}

fn create_buffer_attr(rate: u32, sample_size: usize) -> BufferAttr {
    let sample_size_u32 = sample_size as u32;
    BufferAttr {
        maxlength: (rate / 4) * sample_size_u32,
        tlength: (rate / 50) * sample_size_u32,
        prebuf: (rate / 100) * sample_size_u32,
        minreq: (rate / 200) * sample_size_u32,
        fragsize: (rate / 100) * sample_size_u32,
    }
}

pub fn setup_pulseaudio(audio_rx_rate: u32, audio_tx_rate: u32) -> anyhow::Result<AudioHandles> {
    let spec_rx = pulse::sample::Spec {
        format: pulse::sample::Format::F32le,
        channels: 1,
        rate: audio_rx_rate,
    };
    let spec_tx = pulse::sample::Spec {
        format: pulse::sample::Format::S16le,
        channels: 1,
        rate: audio_tx_rate,
    };

    let pb_attr = create_buffer_attr(audio_rx_rate, std::mem::size_of::<f32>());
    let rec_attr = create_buffer_attr(audio_tx_rate, std::mem::size_of::<i16>());

    let pa_playback = psimple::Simple::new(
        None,
        "trusdxAudio",
        pulse::stream::Direction::Playback,
        Some("TRUSDX"),
        "Radio RX Audio",
        &spec_rx,
        None,
        Some(&pb_attr),
    )?;

    let pa_record = psimple::Simple::new(
        None,
        "trusdxAudio",
        pulse::stream::Direction::Record,
        Some("TRUSDX.monitor"),
        "Radio TX Audio",
        &spec_tx,
        None,
        Some(&rec_attr),
    )?;

    Ok(AudioHandles {
        pa_playback: Arc::new(pa_playback),
        pa_record: Arc::new(pa_record),
    })
}

// Helper function to convert u8 wave samples to f32
fn convert_wave_to_f32(wave_buf: &[u8], f32_buf: &mut Vec<f32>) {
    f32_buf.clear();
    f32_buf.reserve(wave_buf.len());
    for &sample in wave_buf {
        f32_buf.push((sample as f32 - U8_TO_F32_OFFSET) / U8_TO_F32_SCALE);
    }
}

// Helper function to calculate peak level from wave buffer
fn calculate_peak_level(wave_buf: &[u8]) -> f32 {
    wave_buf
        .iter()
        .map(|&sample| {
            let normalized_sample = (sample as f32 - U8_TO_F32_OFFSET) / U8_TO_F32_SCALE;
            normalized_sample.abs()
        })
        .fold(0.0f32, f32::max)
}

// Helper function to write audio buffer to pulseaudio (safe conversion)
fn write_audio_buffer(audio: &psimple::Simple, f32_buf: &[f32]) {
    // Safe conversion: f32 to bytes using bytemuck-style approach
    // Since we're writing F32le format, we can safely reinterpret
    let bytes = unsafe {
        std::slice::from_raw_parts(
            f32_buf.as_ptr() as *const u8,
            f32_buf.len() * std::mem::size_of::<f32>(),
        )
    };
    let _ = audio.write(bytes);
}

// Helper function to process and write wave buffer
fn process_and_write_wave(
    wave_buf: &[u8],
    f32_buf: &mut Vec<f32>,
    audio: &psimple::Simple,
    input_level: &Arc<Mutex<f32>>,
) {
    // Check if wave buffer has any data to process
    if wave_buf.is_empty() {
        return;
    }

    let peak = calculate_peak_level(wave_buf);
    {
        let mut lvl = input_level.lock().unwrap();
        *lvl = (peak * INPUT_LEVEL_SCALE).min(1.0);
    }

    convert_wave_to_f32(wave_buf, f32_buf);
    write_audio_buffer(audio, f32_buf);
}

// Helper function to convert bytes to i16 samples (little-endian)
fn bytes_to_i16_le(bytes: &[u8], output: &mut [i16]) {
    for (index, chunk) in bytes.chunks_exact(2).enumerate() {
        // Check if output buffer has space for more samples
        if index >= output.len() {
            break;
        }
        output[index] = i16::from_le_bytes([chunk[0], chunk[1]]);
    }
}

// Helper function to calculate RMS level
fn calculate_rms(samples: &[i16]) -> f32 {
    let sum_sq: f32 = samples
        .iter()
        .map(|&sample_value| {
            let normalized_sample = sample_value as f32 / I16_TO_F32_SCALE;
            normalized_sample * normalized_sample
        })
        .sum();
    (sum_sq / samples.len() as f32).sqrt().min(1.0)
}

// Helper function to wait for streaming to start
fn wait_for_streaming(streaming_started: &Arc<AtomicBool>, timeout_ms: u64) {
    let start = std::time::Instant::now();
    while !streaming_started.load(Ordering::Relaxed)
        && start.elapsed() < Duration::from_millis(timeout_ms)
    {
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
}

pub fn run_audio_bridge(
    ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    audio: AudioHandles,
    stop_flag: Arc<Mutex<bool>>,
    input_level: Arc<Mutex<f32>>,
    output_level: Arc<Mutex<f32>>,
    freq_state: Arc<Mutex<u64>>,
    mode_state: Arc<Mutex<String>>,
    tx_state: Arc<Mutex<bool>>,
    cat_queue: Arc<Mutex<Vec<Vec<u8>>>>,
    streaming_started: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut inbound_serial_audio = false;
        let mut text_buf: Vec<u8> = Vec::with_capacity(1024);
        let mut wave_buf: Vec<u8> = Vec::with_capacity(8192);
        let mut rx_tmp = [0u8; 512];
        let mut f32_buf: Vec<f32> = Vec::with_capacity(1024);

        let mut tx_i16_buf = vec![0i16; AUDIO_SAMPLE_SIZE];
        let mut tx_byte_buf = vec![0u8; tx_i16_buf.len() * 2];
        let mut u8_buf = vec![0u8; AUDIO_SAMPLE_SIZE];

        let drain_cat = || {
            let mut writes: Vec<Vec<u8>> = Vec::new();
            {
                let mut guard = cat_queue.lock().unwrap();
                // Check if CAT queue has pending commands
                if !guard.is_empty() {
                    writes.extend(guard.drain(..));
                }
            }
            // Check if there are commands to write to serial port
            if !writes.is_empty() {
                // Check if serial port lock acquired successfully
                if let Ok(mut serial_port) = ser.lock() {
                    for write_data in writes {
                        let _ = serial_port.write_all(&write_data);
                    }
                    let _ = crate::trusdx::flush_serial_line(&mut **serial_port);
                }
            }
        };

        let mut prev_tx = false;

        loop {
            {
                // Check if stop flag is set to terminate loop
                if *stop_flag.lock().unwrap() {
                    break;
                }
            }

            let is_tx = *tx_state.lock().unwrap();
            let is_rx = !is_tx;
            let starting_tx = is_tx && !prev_tx;
            let starting_rx = !is_tx && prev_tx;

            // Check if transitioning from RX to TX mode
            if starting_tx {
                let mut drain_buf = vec![0u8; 1024];
                for _ in 0..DRAIN_ATTEMPTS {
                    // Check if audio read failed (buffer empty)
                    if audio.pa_record.read(&mut drain_buf).is_err() {
                        break;
                    }
                }
            }

            // Check if transitioning from TX to RX mode
            if starting_rx {
                inbound_serial_audio = false;
                wave_buf.clear();
                text_buf.clear();
                streaming_started.store(false, Ordering::Relaxed);

                thread::sleep(Duration::from_millis(TX_FALLING_DELAY_MS));

                // Check if serial port lock acquired successfully
                if let Ok(mut serial_port) = ser.lock() {
                    let _ = crate::trusdx::enable_streaming_speaker_off(&mut **serial_port);
                }

                wait_for_streaming(&streaming_started, STREAMING_TIMEOUT_MS);

                // Check if streaming didn't start within timeout
                if !streaming_started.load(Ordering::Relaxed) {
                    // Check if serial port lock acquired successfully for retry
                    if let Ok(mut serial_port) = ser.lock() {
                        let _ = crate::trusdx::enable_streaming_speaker_off(&mut **serial_port);
                    }
                    wait_for_streaming(&streaming_started, STREAMING_RETRY_TIMEOUT_MS);
                }
            }

            prev_tx = is_tx;

            // Check if currently in TX mode
            if is_tx {
                {
                    // Check if CAT queue has pending commands
                    if !cat_queue.lock().unwrap().is_empty() {
                        drain_cat();
                    }
                }

                {
                    *input_level.lock().unwrap() = 0.0;
                }

                match audio.pa_record.read(&mut tx_byte_buf) {
                    Ok(_) => {
                        // Convert bytes to i16 samples
                        bytes_to_i16_le(&tx_byte_buf, &mut tx_i16_buf);

                        // Calculate RMS level
                        let rms = calculate_rms(&tx_i16_buf);
                        {
                            *output_level.lock().unwrap() = rms;
                        }

                        // Check if audio level is below minimum threshold
                        if rms < MIN_RMS_THRESHOLD {
                            continue;
                        }

                        // Convert i16 to u8 with gain and offset
                        for (index, &sample_value) in tx_i16_buf.iter().enumerate() {
                            let scaled =
                                (sample_value as f32 * TX_GAIN).clamp(-32768.0, 32767.0) as i16;
                            let byte = 128i16 + (scaled / 256);
                            u8_buf[index] = byte.clamp(0, 255) as u8;
                        }

                        // Escape semicolons
                        for byte in &mut u8_buf {
                            // Check if byte is semicolon (needs escaping)
                            if *byte == b';' {
                                *byte = b':';
                            }
                        }

                        // Check if serial port lock acquired successfully
                        if let Ok(mut serial_port) = ser.lock() {
                            let _ =
                                crate::trusdx::send_audio_stream_raw(&mut **serial_port, &u8_buf);
                        }
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }

            // Check if currently in RX mode
            if is_rx {
                {
                    *output_level.lock().unwrap() = 0.0;
                }

                let bytes_read = {
                    // Check if serial port lock acquired successfully
                    if let Ok(mut serial_port) = ser.lock() {
                        serial_port.read(&mut rx_tmp).unwrap_or(0)
                    } else {
                        0
                    }
                };

                // Process audio
                for &byte in &rx_tmp[..bytes_read] {
                    // Check if currently receiving audio stream data
                    if inbound_serial_audio {
                        // Check if byte is end-of-frame marker
                        if byte == b';' {
                            // End of wave frame
                            process_and_write_wave(
                                &wave_buf,
                                &mut f32_buf,
                                &audio.pa_playback,
                                &input_level,
                            );
                            wave_buf.clear();
                            inbound_serial_audio = false;
                            drain_cat();
                        } else {
                            wave_buf.push(byte);
                            // Check if wave buffer reached maximum size
                            if wave_buf.len() >= WAVE_BUF_MAX_SIZE {
                                // Flush partial buffer
                                process_and_write_wave(
                                    &wave_buf,
                                    &mut f32_buf,
                                    &audio.pa_playback,
                                    &input_level,
                                );
                                wave_buf.clear();
                            }
                        }
                        continue;
                    }

                    // Text mode processing
                    text_buf.push(byte);
                    // Check if received "US" command (start streaming)
                    if text_buf.len() == 2 && text_buf[0] == b'U' && text_buf[1] == b'S' {
                        text_buf.clear();
                        inbound_serial_audio = true;
                        streaming_started.store(true, Ordering::Relaxed);
                        continue;
                    }

                    // Check if byte is command terminator
                    if byte == b';' {
                        // Check if command is frequency update (FA)
                        if text_buf.len() >= 2 && text_buf[0] == b'F' && text_buf[1] == b'A' {
                            // Check if text buffer is valid UTF-8
                            if let Ok(text_string) = std::str::from_utf8(&text_buf) {
                                let digits: String = text_string
                                    .chars()
                                    .skip(2)
                                    .take_while(|character| character.is_ascii_digit())
                                    .collect();
                                // Check if frequency digits parse successfully
                                if let Ok(frequency_value) = digits.parse::<u64>() {
                                    *freq_state.lock().unwrap() = frequency_value;
                                }
                            }
                        }
                        // Check if command is mode update (MD)
                        if text_buf.len() >= 2 && text_buf[0] == b'M' && text_buf[1] == b'D' {
                            let mode_str = match text_buf.get(2).copied().unwrap_or(b'2') {
                                b'1' => "LSB",
                                b'2' => "USB",
                                b'3' => "CW",
                                b'4' => "FM",
                                b'5' => "AM",
                                _ => "USB",
                            };
                            *mode_state.lock().unwrap() = mode_str.to_string();
                        }
                        text_buf.clear();
                        // Check if CAT queue has pending commands
                        if !cat_queue.lock().unwrap().is_empty() {
                            drain_cat();
                        }
                    }
                }
            }
        }
    });
}
