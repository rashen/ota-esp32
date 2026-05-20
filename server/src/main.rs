use anyhow::{Result, anyhow};
use clap::Parser;
use shared::{OTA_DATA_SIZE, OtaPacket, Packet};
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

fn main() -> Result<()> {
    let args = Args::parse();

    let path = Path::new(&args.binary);
    if !path.exists() {
        return Err(anyhow!("Binary does not exist"));
    }
    let binary = std::fs::read(path)?;

    let ip = args.ip;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
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

        sleep(Duration::from_millis(500));

        if !sent_binary {
            let total = binary.len() / OTA_DATA_SIZE;
            for (i, data) in binary.chunks(OTA_DATA_SIZE).enumerate() {
                if let Err(e) = postcard::to_slice(
                    &Packet::OtaPacket(OtaPacket {
                        num: i as u32,
                        total: total as u32,
                        data: data.try_into().unwrap(),
                    }),
                    &mut buffer,
                ) {
                    println!("Failed to serialize bin data: {e:?}");
                };

                if let Err(e) = socket.send_to(&buffer, addr.clone()) {
                    println!("Failed sending bin: {e:?}");
                }
                sleep(Duration::from_millis(10));
            }
            println!("Binary sent");
            sent_binary = true;
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}
