#![no_std]
#![no_main]

use core::str::FromStr;
use dotenvy_macro::dotenv;
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use embedded_storage::Storage;
use esp_alloc as _;
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Interface, WifiController};
use esp_storage::FlashStorage;
use heapless::{String, Vec};
use shared::{Ack, OTA_DATA_SIZE, OtaPacket, Packet};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

static OTA_CHANNEL: Channel<CriticalSectionRawMutex, OtaChannelPacket, 1> = Channel::new();

struct OtaChannelPacket {
    data: Vec<u8, 4096>,
    is_last: bool,
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_alloc::heap_allocator!(size: 96 * 1024);

    // println!("STARTED FROM OTA!!");
    // println!("STARTED FROM OTA!!");
    // println!("STARTED FROM OTA!!");

    println!("Starting rtos");
    let software_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    let mut flash = FlashStorage::new(peripherals.FLASH);

    let mut buffer = [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN];
    let pt =
        esp_bootloader_esp_idf::partitions::read_partition_table(&mut flash, &mut buffer).unwrap();

    // List all partitions - this is just FYI
    for part in pt.iter() {
        println!("{:?}", part);
    }

    println!("Currently booted partition {:?}", pt.booted_partition());

    let mut ota =
        esp_bootloader_esp_idf::ota_updater::OtaUpdater::new(&mut flash, &mut buffer).unwrap();

    let current = ota.selected_partition().unwrap();
    println!("current image state {:?}", ota.current_ota_state());
    println!("currently selected partition {:?}", current);

    // Mark the current slot as VALID - this is only needed if the bootloader was
    // built with auto-rollback support. The default pre-compiled bootloader in
    // espflash is NOT.
    if let Ok(state) = ota.current_ota_state() {
        if state == esp_bootloader_esp_idf::ota::OtaImageState::PendingVerify {
            println!("Changed state to VALID");
            ota.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::Valid)
                .unwrap();
        }
    }

    let config = esp_radio::wifi::ControllerConfig::default();
    #[allow(unused_mut)]
    let (mut controller, interfaces) = esp_radio::wifi::new(peripherals.WIFI, config).unwrap();
    #[cfg(feature = "esp32c5")]
    let _ = controller.set_band_mode(esp_radio::wifi::BandMode::_2_4G);

    spawner
        .spawn(wifi_task(spawner, interfaces.station, controller).expect("Failed spawning WIFI"));

    let (mut next_app_partition, part_type) = ota.next_partition().unwrap();

    println!("Flashing OTA image to {:?}", part_type);

    let mut written = 0;
    loop {
        let pkt = OTA_CHANNEL.receive().await;
        if let Err(e) = next_app_partition.write(written as u32, &pkt.data) {
            println!("Failed to write to next app partition: {e:?}");
        }
        written += pkt.data.len();

        if pkt.is_last {
            break;
        }
    }

    println!("Changing OTA slot and setting the state to NEW");
    if let Err(e) = ota.activate_next_partition() {
        println!("Failed to activate next partition: {e}");
    }
    if let Err(e) = ota.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::New) {
        println!("Failed to set current ota state: {e}");
    }
    println!("OTA update succeded. Ready to reboot.");

    loop {
        Timer::after_millis(100).await
    }
}

#[embassy_executor::task]
async fn wifi_task(
    spawner: Spawner,
    wifi_interface: Interface<'static>,
    mut controller: WifiController<'static>,
) -> ! {
    let mut config = embassy_net::DhcpConfig::default();
    config.hostname = Some(String::from_str("esp32-ota").unwrap());

    let config = embassy_net::Config::dhcpv4(config);
    let rng = esp_hal::rng::Rng::new();
    let seed = unsafe { core::mem::transmute::<_, u64>([rng.random(), rng.random()]) };

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );
    let _ = controller.set_power_saving(esp_radio::wifi::PowerSaveMode::None);

    let stack = &*mk_static!(Stack, stack);

    let ssid = dotenv!("SSID");
    let pw = dotenv!("PW");
    let config = esp_radio::wifi::Config::Station(
        StationConfig::default()
            .with_ssid(ssid)
            .with_auth_method(esp_radio::wifi::AuthenticationMethod::Wpa2Personal)
            .with_password(pw.try_into().unwrap()),
    );
    let _ = controller.set_config(&config);

    spawner.spawn(net_task(runner).expect("Failed spawning net_task"));

    let mut recv_bytes = [0u8; 600];
    let mut rx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 10];
    let mut tx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 10];
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 128];
    let mut socket = embassy_net::udp::UdpSocket::new(
        *stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(4242).expect("BIND");

    let mut failed_attempts = 0;
    let mut ota_cnt = 0;
    let mut ip = None;
    let ota_channel = OTA_CHANNEL.sender();
    let mut ota_buffer = Vec::<u8, 4096>::new();
    loop {
        if !controller.is_connected() {
            ip = None;
            match controller.connect_async().await {
                Ok(_) => {
                    failed_attempts = 0;
                }
                Err(e) => {
                    println!("Wifi failed to connect: {e}");
                    failed_attempts += 1;
                    let sleep_time = (failed_attempts * failed_attempts).min(10);
                    Timer::after(Duration::from_secs(sleep_time as u64)).await;
                    continue;
                }
            }
        }

        if ip.is_none() {
            ip = stack.config_v4().map(|a| a.address.address().octets());
            if let Some(ip) = ip {
                println!(
                    "Wifi connected with IP: {}.{}.{}.{}",
                    ip[0], ip[1], ip[2], ip[3]
                );
            }
        }

        let recv_fut = socket.recv_from(&mut recv_bytes);
        let timeout = embassy_time::with_timeout(Duration::from_millis(500), recv_fut).await;
        if let Ok(r) = timeout {
            match r {
                Ok((_len, metadata)) => match postcard::from_bytes::<Packet>(&recv_bytes) {
                    Ok(pkt) => match pkt {
                        Packet::Message(msg) => {
                            println!("Received message {msg}")
                        }
                        Packet::OtaPacket(OtaPacket { num, total, data }) => {
                            if num == 0 {
                                ota_cnt = 0;
                                println!("Reseting OTA counter");
                            } else if num != ota_cnt {
                                println!("OTA packet count mismatch");
                                continue;
                            }
                            ota_cnt += 1;

                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    data.as_ptr(),
                                    ota_buffer.as_mut_ptr().offset(ota_buffer.len() as isize),
                                    data.len(),
                                );
                                ota_buffer.set_len(ota_buffer.len() + data.len());
                            }

                            let free_space = ota_buffer.capacity() - ota_buffer.len();
                            if free_space < OTA_DATA_SIZE || num == total {
                                let mut to_send = Vec::<u8, 4096>::new();
                                core::mem::swap(&mut ota_buffer, &mut to_send);
                                ota_channel
                                    .send(OtaChannelPacket {
                                        data: to_send,
                                        is_last: num == total,
                                    })
                                    .await;
                            }

                            if num % 100 == 0 {
                                println!("Received {num}/{total}");
                            }

                            let from = metadata.endpoint;
                            let response = postcard::to_vec::<_, 2>(&Ack { num })
                                .expect("Failed to serialize ack");

                            let send_fut = socket.send_to(&response, from);
                            if let Err(e) =
                                embassy_time::with_timeout(Duration::from_millis(100), send_fut)
                                    .await
                            {
                                println!("Failed to send ack: {e:?}");
                            }
                        }
                    },
                    Err(_) => {}
                },
                Err(e) => {
                    println!("UDP receive error: {e:?}");
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) {
    runner.run().await
}
