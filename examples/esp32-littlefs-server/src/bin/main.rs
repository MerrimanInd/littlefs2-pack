#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println as _;

use esp32_littlefs_server as lib;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);

    // Start WiFi FIRST — it needs internal SRAM heap for task stacks
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Embassy initialized!");

    let radio_init = &*lib::mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );
    let rng = Rng::new();
    let stack = lib::wifi::start_wifi(radio_init, peripherals.WIFI, rng, &spawner).await;
    info!("WiFi started!");

    Timer::after(Duration::from_secs(2)).await;

    // Mount filesystem — just mounts the LittleFS partition, no files
    // are read yet. Files are read on-demand per HTTP request.
    lib::fs::mount_fs(peripherals.FLASH);

    Timer::after(Duration::from_secs(2)).await;

    // Start web server — no upfront file loading, no PSRAM leaks.
    // Each request reads from flash into a temporary buffer, serves
    // it, then frees the buffer.
    let web_app = lib::web::WebApp::default();
    for id in 0..lib::web::WEB_TASK_POOL_SIZE {
        spawner.must_spawn(lib::web::web_task(
            id,
            stack,
            web_app.router,
            web_app.config,
        ));
    }
    info!("Web server running!");

    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}
