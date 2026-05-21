use anyhow::{Result, anyhow};
use clap::Parser;
use shared::{Ack, OTA_DATA_SIZE, OtaPacket, Packet};
use std::net::UdpSocket;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// IP address of OTA device
    #[arg(short, long)]
    ip: String,

    /// Binary to send over OTA
    #[arg(short, long)]
    binary: String,
}

const READ_TIMEOUT_MS: u64 = 10;

fn main() -> Result<()> {
    let args = Args::parse();

    let path = Path::new(&args.binary);
    if !path.exists() {
        return Err(anyhow!("Binary does not exist"));
    }
    let binary = std::fs::read(path)?;

    let ip = args.ip;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let _ = socket.set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)));
    let addr = format!("{ip}:4242");

    let mut sent_binary = false;
    loop {
        let msg = Packet::Message("ping".try_into().unwrap());
        const MAX_LEN: usize = 600;
        let mut buffer = [0; MAX_LEN];
        let msg = postcard::to_slice(&msg, &mut buffer).unwrap();
        if let Err(e) = socket.send_to(&msg, addr.clone()) {
            println!("Failed sending msg: {e:?}");
        }

        if !sent_binary {
            let start_time = std::time::Instant::now();
            let total = binary.chunks(OTA_DATA_SIZE).count() - 1;
            'send: for (i, data) in binary.chunks(OTA_DATA_SIZE).enumerate() {
                const RETRY_CNT: u32 = 5;

                'retry: for j in 0..RETRY_CNT {
                    match postcard::to_slice(
                        &Packet::OtaPacket(OtaPacket {
                            num: i as u32,
                            total: total as u32,
                            data: data.try_into().unwrap(),
                        }),
                        &mut buffer,
                    ) {
                        Ok(msg) => {
                            if let Err(e) = socket.send_to(msg, addr.clone()) {
                                println!("Failed sending bin: {e:?}");
                            }
                        }
                        Err(e) => {
                            println!("Failed to serialize bin data: {e:?}");
                        }
                    };

                    let timeout = 500;
                    let max_loops = timeout / READ_TIMEOUT_MS;
                    let mut recv_buffer = [0_u8; 4];
                    for _ in 0..max_loops {
                        if let Ok((size, _)) = socket.recv_from(&mut recv_buffer) {
                            let ack: Ack = postcard::from_bytes(&recv_buffer[0..size]).unwrap();
                            if ack.num == i as u32 {
                                break 'retry;
                            } else {
                                println!("Ack failed");
                                if j == RETRY_CNT {
                                    println!("Aborting");
                                    break 'send;
                                }
                            }
                        }
                    }
                }
            }
            let t = start_time.elapsed().as_secs();
            println!("Binary sent in {t}s");
            sent_binary = true;
        }

        sleep(Duration::from_millis(1000));
    }

    #[allow(unreachable_code)]
    Ok(())
}
