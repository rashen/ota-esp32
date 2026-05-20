//! OTA Update Example
//!
//! This shows the basics of dealing with partitions and changing the active
//! partition. For simplicity it will flash an application image embedded into
//! the binary. In a real world application you can get the image via HTTP(S),
//! UART or from an sd-card etc.
//!
//! Adjust the target and the chip in the following commands according to the
//! chip used!
//!
//! ```ignore,bash
//! cargo xtask build examples gpio --chip=esp32
//! espflash save-image --chip=esp32 target/xtensa-esp32-none-elf/release/gpio_interrupt examples/target/ota_image
//! cargo xtask build examples update --chip=esp32
//! espflash save-image --chip=esp32 target/xtensa-esp32-none-elf/release/ota_update examples/target/ota_image
//! cargo xtask build examples update --chip=esp32
//! espflash save-image --chip=esp32 target/xtensa-esp32-none-elf/release/ota_update examples/target/ota_image
//! espflash erase-flash
//! cargo xtask run example update --chip=esp32
//! ```
//!
//! On first boot notice the firmware partition gets booted ("Loaded app from
//! partition at offset 0x10000"). Press the BOOT button, once finished press
//! the RESET button.
//!
//! Notice OTA0 gets booted ("Loaded app from partition at offset 0x110000").
//!
//! Once again press BOOT, when finished press RESET.
//! You will see the `gpio_interrupt` example gets booted from OTA1 ("Loaded app
//! from partition at offset 0x210000")
//!
//! See <https://docs.espressif.com/projects/esp-idf/en/latest/esp32/api-reference/system/ota.html>

#![no_std]
#![no_main]

use core::str::FromStr;
use dotenvy_macro::dotenv;
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use embedded_storage::Storage;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_println::println;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Interface, WifiController};
use esp_storage::FlashStorage;
use heapless::{String, Vec};
use shared::{OTA_DATA_SIZE, OtaPacket, Packet};

esp_bootloader_esp_idf::esp_app_desc!();

// static OTA_IMAGE: &[u8] = include_bytes!("../../../target/ota_image");
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

type OtaBuffer = Mutex<
    CriticalSectionRawMutex,
    Vec<u8, { esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN }>,
>;

static OTA_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static OTA_BUFFER: OtaBuffer = Mutex::new(Vec::new());

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_alloc::heap_allocator!(size: 96 * 1024);

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
    println!(
        "current image state {:?} (only relevant if the bootloader was built with auto-rollback support)",
        ota.current_ota_state()
    );
    println!("currently selected partition {:?}", current);

    // Mark the current slot as VALID - this is only needed if the bootloader was
    // built with auto-rollback support. The default pre-compiled bootloader in
    // espflash is NOT.
    if let Ok(state) = ota.current_ota_state() {
        if state == esp_bootloader_esp_idf::ota::OtaImageState::New
            || state == esp_bootloader_esp_idf::ota::OtaImageState::PendingVerify
        {
            println!("Changed state to VALID");
            ota.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::Valid)
                .unwrap();
        }
    }

    cfg_if::cfg_if! {
         if #[cfg(feature = "esp32c5")] {
            let button = peripherals.GPIO28;
        } else {
            let button = peripherals.GPIO9;
        }
    }

    let config = esp_radio::wifi::ControllerConfig::default();
    #[allow(unused_mut)]
    let (mut controller, interfaces) = esp_radio::wifi::new(peripherals.WIFI, config).unwrap();
    #[cfg(feature = "esp32c5")]
    let _ = controller.set_band_mode(esp_radio::wifi::BandMode::_2_4G);

    spawner
        .spawn(wifi_task(spawner, interfaces.station, controller).expect("Failed spawning WIFI"));

    let boot_button = Input::new(button, InputConfig::default().with_pull(Pull::Up));

    let mut done = false;
    loop {
        let _ = OTA_READY.wait().await;
        println!("OTA update ready");
        println!("Press boot button to flash and switch to the next OTA slot");
        // Lock OtaBuffer so it can't be written to by the wifi task
        let ota_buffer = OTA_BUFFER.lock().await;

        if boot_button.is_low() && !done {
            done = true;

            let (mut next_app_partition, part_type) = ota.next_partition().unwrap();

            println!("Flashing image to {:?}", part_type);

            // write to the app partition
            for (sector, chunk) in ota_buffer.chunks(4096).enumerate() {
                println!("Writing sector {sector}...");

                next_app_partition
                    .write((sector * 4096) as u32, chunk)
                    .unwrap();
            }

            println!("Changing OTA slot and setting the state to NEW");

            ota.activate_next_partition().unwrap();
            ota.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::New)
                .unwrap();
        }
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
    let mut tx_meta = [embassy_net::udp::PacketMetadata::EMPTY; 0];
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 0];
    let mut s = embassy_net::udp::UdpSocket::new(
        *stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    s.bind(4242).expect("BIND");

    let mut failed_attempts = 0;
    let mut ota_cnt = 0;
    let mut ip = None;
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

        let recv_fut = s.recv_from(&mut recv_bytes);
        let timeout = embassy_time::with_timeout(Duration::from_millis(500), recv_fut).await;
        if let Ok(r) = timeout {
            match r {
                Ok(_) => match postcard::from_bytes::<Packet>(&recv_bytes) {
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

                            if let Ok(mut buffer) = OTA_BUFFER.try_lock() {
                                // TODO: This is very inefficient
                                for b in data {
                                    let _ = buffer.push(b);
                                }
                                println!("Received {num}/{total}");
                                // let start = OTA_DATA_SIZE * num as usize;
                                // unsafe {
                                //     let src = data.as_ptr();
                                //     let dst = core::ptr::addr_of_mut!(buffer[start]);
                                //     core::ptr::copy_nonoverlapping(src, dst, OTA_DATA_SIZE);
                                // }
                                if num == total {
                                    OTA_READY.signal(());
                                }
                            } else {
                                println!("Failed to lock buffer");
                            }
                        }
                    },
                    Err(e) => {
                        println!("Postcard decode error: {e:?}");
                    }
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
