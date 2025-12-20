use crate::trusdx;
use serialport;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

enum CommandOptions {
    GetFrequency,
    SetFrequency(u64),
    GetVFO,
    GetMode,
    SetMode,
    SetTransmit(bool),
    SetVfoOpt(bool),
    Boolean(bool),
    DumpState,
    Exit,
    Success,
    Error,
}

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

        fn get_frequency_command<T: AsRef<str>>(s: Option<T>) -> CommandOptions {
            if let Some(val) = s {
                let parsed_hz = if let Ok(hz_int) = val.as_ref().parse::<u64>() {
                    Some(hz_int)
                } else if let Ok(hz_f) = val.as_ref().parse::<f64>() {
                    Some(hz_f.round() as u64)
                } else {
                    None
                };

                // Check if frequency parsed successfully
                if let Some(hz) = parsed_hz {
                    CommandOptions::SetFrequency(hz)
                } else {
                    CommandOptions::Error
                }
            } else {
                CommandOptions::Error
            }
        }

        fn get_boolean_from_num<T: AsRef<str>>(s: Option<T>) -> bool {
            s.map(|v| v.as_ref().parse::<i32>().map(|x| x != 0).unwrap_or(false))
                .unwrap_or(false)
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

        let command_response = match cmds[0] {
            "\\set_vfo_opt" => CommandOptions::SetVfoOpt(get_boolean_from_num(cmds.get(1))),
            "\\chk_vfo" => CommandOptions::Boolean(vfo_mode),
            "\\get_powerstat" => CommandOptions::Boolean(true),
            "\\dump_state" => CommandOptions::DumpState,
            "\\get_freq" | "f" => {
                if (cmds.len() == 1 && !vfo_mode)
                    || (cmds.len() == 2 && vfo_mode && cmds[1] == current_vfo)
                {
                    CommandOptions::GetFrequency
                } else {
                    CommandOptions::Error
                }
            }
            "\\set_freq" | "F" => {
                if cmds.len() == 2 && !vfo_mode {
                    get_frequency_command(cmds.get(1))
                } else if cmds.len() == 3 && vfo_mode && cmds[1] == current_vfo {
                    get_frequency_command(cmds.get(2))
                } else {
                    CommandOptions::Error
                }
            }
            "\\dump_caps" => CommandOptions::Success,
            "m" => CommandOptions::GetMode,
            "M" => CommandOptions::SetMode,
            "v" => CommandOptions::GetVFO,
            "V" => CommandOptions::Success,
            "t" | "\\get_ptt" => CommandOptions::Boolean(*tx_state.lock().unwrap()),
            "T" | "\\set_ptt" => {
                // Check if command has TX state parameter
                if cmds.len() >= 2 {
                    CommandOptions::SetTransmit(get_boolean_from_num(cmds.get(1)))
                } else {
                    CommandOptions::Error
                }
            }
            "q" => CommandOptions::Exit,
            _ => CommandOptions::Error,
        };

        match command_response {
            CommandOptions::Boolean(val) => {
                let _ = writeln!(stream, "{}", if val { "1" } else { "0" });
            }
            CommandOptions::GetFrequency => {
                let hz = *freq_state.lock().unwrap();
                let _ = writeln!(stream, "{}", hz);
            }
            CommandOptions::SetVfoOpt(val) => {
                vfo_mode = val;
                let _ = writeln!(stream, "RPRT 0");
            }
            CommandOptions::SetFrequency(hz) => {
                *freq_state.lock().unwrap() = hz;
                {
                    let mut q = cat_queue.lock().unwrap();
                    q.push(format!("FA{:011};", hz).into_bytes());
                }
                let _ = writeln!(stream, "RPRT 0");
            }
            CommandOptions::GetVFO => {
                let _ = writeln!(stream, "{current_vfo}");
            }
            CommandOptions::GetMode => {
                let _ = writeln!(stream, "USB");
                let _ = writeln!(stream, "2400");
            }
            CommandOptions::SetMode => {
                {
                    let mut q = cat_queue.lock().unwrap();
                    q.push(b"MD2;".to_vec());
                }
                let _ = writeln!(stream, "RPRT 0");
            }
            CommandOptions::SetTransmit(val) => {
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
                let _ = writeln!(stream, "RPRT 0");
            }
            CommandOptions::Exit => {
                let _ = writeln!(stream, "RPRT 0");
                break;
            }
            CommandOptions::Success => {
                let _ = writeln!(stream, "RPRT 0");
            }
            CommandOptions::Error => {
                let _ = writeln!(stream, "RPRT -1");
            }
            CommandOptions::DumpState => {
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
        }

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
