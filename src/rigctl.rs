use crate::trusdx;
use serialport;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

fn handle_rigctl_client(
    mut stream: TcpStream,
    _ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    freq_state: Arc<Mutex<u64>>,
    tx_state: Arc<Mutex<bool>>,
    cat_queue: Arc<Mutex<Vec<Vec<u8>>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    let current_vfo = "VFOA";
    let mut vfo_mode = false;

    let mut debug_file = if let Ok(file) = std::env::var("TRUSDX_LOG_FILE") {
        Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
                .unwrap(),
        )
    } else {
        None
    };

    loop {
        line.clear();
        // Check if line read failed (client disconnected)
        if reader.read_line(&mut line).is_err() {
            break;
        }
        // Check if line is empty (end of input)
        if line.is_empty() {
            break;
        }
        let cmd = line.trim();
        // Check if command is empty after trimming
        if cmd.is_empty() {
            continue;
        }

        debug_file.as_mut().map(|f| writeln!(f, "{}", cmd).unwrap());

        fn get_frequency_command<T: AsRef<str>>(s: Option<T>) -> Option<u64> {
            if let Some(val) = s {
                if let Ok(hz_int) = val.as_ref().parse::<u64>() {
                    Some(hz_int)
                } else if let Ok(hz_f) = val.as_ref().parse::<f64>() {
                    Some(hz_f.round() as u64)
                } else {
                    None
                }
            } else {
                None
            }
        }

        fn get_boolean_from_num<T: AsRef<str>>(s: Option<T>) -> bool {
            s.map(|v| v.as_ref().parse::<i32>().map(|x| x != 0).unwrap_or(false))
                .unwrap_or(false)
        }

        fn write_success(stream: &mut TcpStream) {
            let _ = writeln!(stream, "RPRT 0");
        }

        fn write_failure(stream: &mut TcpStream) {
            let _ = writeln!(stream, "RPRT -1");
        }

        fn write_boolean(stream: &mut TcpStream, val: bool) {
            let _ = writeln!(stream, "{}", if val { 1 } else { 0 });
        }

        let cmds = cmd.split_ascii_whitespace().collect::<Vec<_>>();
        debug_file
            .as_mut()
            .map(|f| writeln!(f, "  {}", cmds.len()).unwrap());
        for (i, c) in cmds.iter().enumerate() {
            debug_file
                .as_mut()
                .map(|f| writeln!(f, "    {i} => {c}").unwrap());
        }

        match cmds[0] {
            "\\set_vfo_opt" => {
                vfo_mode = get_boolean_from_num(cmds.get(1));
                write_success(&mut stream);
            }
            "\\chk_vfo" => write_boolean(&mut stream, vfo_mode),
            "\\get_powerstat" => write_boolean(&mut stream, true),
            "\\dump_state" => {
                let lines = [
                    "0",
                    "0",
                    "0",
                    "0 0 0 0 0 0 0",
                    "0 0 0 0 0 0 0",
                    "0 0",
                    "0 0",
                    "0",
                    "0",
                    "0",
                    "0",
                    "0 0 0 0 0 0 0",
                    "0 0 0 0 0 0 0",
                    "0",
                    "0",
                    "0",
                    "0",
                    "0",
                    "0",
                ];
                for l in lines {
                    let _ = writeln!(stream, "{}", l);
                }
            }
            "\\get_freq" | "f" => {
                if (cmds.len() == 1 && !vfo_mode)
                    || (cmds.len() == 2 && vfo_mode && cmds[1] == current_vfo)
                {
                    let hz = *freq_state.lock().unwrap();
                    let _ = writeln!(stream, "{}", hz);
                } else {
                    write_failure(&mut stream);
                }
            }
            "\\set_freq" | "F" => {
                let parsed_hz = if cmds.len() == 2 && !vfo_mode {
                    get_frequency_command(cmds.get(1))
                } else if cmds.len() == 3 && vfo_mode && cmds[1] == current_vfo {
                    get_frequency_command(cmds.get(2))
                } else {
                    write_failure(&mut stream);
                    continue;
                };

                if let Some(hz) = parsed_hz {
                    *freq_state.lock().unwrap() = hz;
                    {
                        let mut q = cat_queue.lock().unwrap();
                        q.push(format!("FA{:011};", hz).into_bytes());
                    }
                    write_success(&mut stream);
                }
            }
            "\\dump_caps" => {
                write_success(&mut stream);
            }
            "m" => {
                let _ = writeln!(stream, "USB");
                let _ = writeln!(stream, "2400");
            }
            "M" => {
                {
                    let mut q = cat_queue.lock().unwrap();
                    q.push(b"MD2;".to_vec());
                }
                write_success(&mut stream);
            }
            "v" => {
                let _ = writeln!(stream, "{current_vfo}");
            }
            "V" => {
                write_success(&mut stream);
            }
            "t" | "\\get_ptt" => write_boolean(&mut stream, *tx_state.lock().unwrap()),
            "T" | "\\set_ptt" => {
                // Check if command has TX state parameter
                if cmds.len() >= 2 {
                    let val = get_boolean_from_num(cmds.get(1));
                    // Check if serial port lock acquired successfully
                    if let Ok(mut s) = _ser.lock() {
                        // Check if TX should be enabled
                        if val {
                            let _ = trusdx::start_transmit_baseband(&mut **s);
                        } else {
                            let _ = trusdx::enable_streaming_speaker_off(&mut **s);
                        }
                    }
                    *tx_state.lock().unwrap() = val;
                    write_success(&mut stream);
                } else {
                    write_failure(&mut stream);
                }
            }
            "q" => {
                write_success(&mut stream);
                break;
            }
            _ => {
                write_failure(&mut stream);
            }
        };

        debug_file.as_mut().map(|f| f.flush().unwrap());
    }
}

pub fn spawn_rigctl_server(
    ser: Arc<Mutex<Box<dyn serialport::SerialPort + Send>>>,
    freq_state: Arc<Mutex<u64>>,
    tx_state: Arc<Mutex<bool>>,
    cat_queue: Arc<Mutex<Vec<Vec<u8>>>>,
) {
    let _ = std::process::Command::new("pkill")
        .args(["-f", "rigctl"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            let _ = child.try_wait();
            Ok(())
        });

    let _ = std::process::Command::new("fuser")
        .args(["-k", "4532/tcp"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            let _ = child.try_wait();
            Ok(())
        });

    // Check if lsof command executed successfully
    if let Ok(output) = std::process::Command::new("lsof")
        .args(["-ti:4532"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
    {
        // Check if port 4532 is in use
        if !output.stdout.is_empty() {
            let pid_str = String::from_utf8_lossy(&output.stdout);
            let pid = pid_str.trim();
            let current_pid = std::process::id().to_string();
            // Check if process using port is not current process
            if pid != current_pid {
                let _ = std::process::Command::new("kill")
                    .args(["-9", pid])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .and_then(|mut child| {
                        let _ = child.try_wait();
                        Ok(())
                    });
            }
        }
    }

    std::thread::spawn(move || {
        let addr = ("127.0.0.1", 4532);
        // Check if TCP listener bound successfully
        if let Ok(listener) = TcpListener::bind(addr) {
            for stream in listener.incoming() {
                // Check if client connection accepted successfully
                if let Ok(stream) = stream {
                    handle_rigctl_client(
                        stream,
                        ser.clone(),
                        freq_state.clone(),
                        tx_state.clone(),
                        cat_queue.clone(),
                    );
                }
            }
        } else {
            eprintln!("rigctl: failed to bind 127.0.0.1:4532");
        }
    });
}
