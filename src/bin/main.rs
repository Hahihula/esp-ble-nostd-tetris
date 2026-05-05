#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use bt_hci::controller::ExternalController;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output};
use esp_hal::rmt::TxChannelCreator;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use log::info;
use no_std_tetris::{RandomGenerator, Tetris, Color};
use static_cell::StaticCell;
use trouble_host::prelude::*;
use esp_hal::rmt::PulseCode;
use embassy_sync::channel::Channel;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

// Tetris game constants (matching no_std_tetris)
const BOARD_WIDTH: usize = 10;
const BOARD_HEIGHT: usize = 20;
const FALL_INTERVAL_MS: u64 = 500;
const BRIGHTNESS: u8 = 12;

// NeoPixel timing constants
const T0H: u16 = 35;
const T0L: u16 = 90;
const T1H: u16 = 70;
const T1L: u16 = 55;

// Game state for rendering - sent from game task to render task
// Uses plain array to avoid heap allocation
struct GameState {
    // Board state: None = empty, Some(color) = filled cell
    board: [[Option<Color>; BOARD_WIDTH]; BOARD_HEIGHT],
    current_color: Color,
    current_piece: [(i8, i8); 4],
    piece_x: i8,
    piece_y: i8,
    score: u32,
    game_over: bool,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            board: [[None; BOARD_WIDTH]; BOARD_HEIGHT],
            current_color: Color::Red,
            current_piece: [(0, 0), (1, 0), (0, 1), (1, 1)],
            piece_x: 4,
            piece_y: 0,
            score: 0,
            game_over: true,
        }
    }
}

// Convert no_std_tetris game to renderable state
fn extract_game_state(game: &Tetris<TetrisRng>) -> GameState {
    let mut state = GameState::default();
    state.score = game.score;
    state.game_over = game.is_game_over();
    state.current_color = game.current_piece.color;

    // Copy piece shape
    for (i, &(dx, dy)) in game.current_piece.shape.iter().enumerate() {
        if i < 4 {
            state.current_piece[i] = (dx as i8, dy as i8);
        }
    }
    state.piece_x = game.piece_pos.0;
    state.piece_y = game.piece_pos.1;

    // Copy board
    for y in 0..BOARD_HEIGHT {
        for x in 0..BOARD_WIDTH {
            state.board[y][x] = game.board[y][x];
        }
    }

    state
}

// Compare two game states to see if rendering is needed
fn state_changed(old: &GameState, new: &GameState) -> bool {
    if old.game_over != new.game_over {
        return true;
    }
    // Compare colors by their discriminant (Color is an enum)
    if core::mem::discriminant(&old.current_color) != core::mem::discriminant(&new.current_color) {
        return true;
    }
    if old.piece_x != new.piece_x || old.piece_y != new.piece_y {
        return true;
    }
    for i in 0..4 {
        if old.current_piece[i] != new.current_piece[i] {
            return true;
        }
    }
    for y in 0..BOARD_HEIGHT {
        for x in 0..BOARD_WIDTH {
            if core::mem::discriminant(&old.board[y][x]) != core::mem::discriminant(&new.board[y][x]) {
                return true;
            }
        }
    }
    false
}

#[esp_rtos::main]
async fn main(_spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    log::info!("ESP BLE Tetris starting");

    let bluetooth = peripherals.BT;
    let connector = BleConnector::new(bluetooth, Default::default()).unwrap();
    let controller: ExternalController<_, 1> = ExternalController::new(connector);

    // GPIO8 for connection status LED
    static STATUS_LED: StaticCell<Output<'static>> = StaticCell::new();
    let status_led = STATUS_LED.init(Output::new(peripherals.GPIO8, Level::Low, Default::default()));

    // GPIO4 for NeoPixel LED strip
    let rmt = esp_hal::rmt::Rmt::new(peripherals.RMT, esp_hal::time::Rate::from_mhz(80))
        .unwrap()
        .into_async();
    log::info!("Configuring RMT for NeoPixel control on GPIO4");
    let tx_config = esp_hal::rmt::TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(false);
    let channel = rmt
        .channel0
        .configure_tx(&tx_config)
        .unwrap()
        .with_pin(peripherals.GPIO4);

    static RMT_CHANNEL: StaticCell<esp_hal::rmt::Channel<'static, esp_hal::Async, esp_hal::rmt::Tx>> = StaticCell::new();
    let rmt_channel = RMT_CHANNEL.init(channel);

    // Create channel for game state to renderer
    // Using embassy-sync channel with capacity 1 (latest state only)
    static RENDER_CHANNEL: StaticCell<Channel<CriticalSectionRawMutex, GameState, 1>> = StaticCell::new();
    let game_to_render = RENDER_CHANNEL.init(Channel::new());
    log::info!("Starting BLE Tetris peripheral task");

    ble_tetris_run(controller, status_led, rmt_channel, game_to_render).await;
    log::info!("BLE Tetris peripheral task exited");
}

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

// Tetris control commands (matches web UI)
const CMD_NONE: u8 = 0;
const CMD_LEFT: u8 = 1;
const CMD_RIGHT: u8 = 2;
const CMD_ROTATE: u8 = 3;
const CMD_DOWN: u8 = 4;
const CMD_START: u8 = 5;

#[gatt_server]
struct Server {
    tetris_service: TetrisService,
}

#[gatt_service(uuid = "12345678-1234-5678-1234-56789abcdef0")]
struct TetrisService {
    #[characteristic(uuid = "12345678-1234-5678-1234-56789abcdef1", write)]
    control: u8,
}

struct TetrisRng;

impl RandomGenerator for TetrisRng {
    fn next_random(&mut self) -> usize {
        // Simple LFSR-like pseudo-random for embedded
        static mut STATE: u32 = 0x12345678;
        unsafe {
            let state = STATE;
            let new_state = state.wrapping_mul(1103515245).wrapping_add(12345);
            STATE = new_state;
            (new_state >> 16) as usize
        }
    }
}

fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Red => (BRIGHTNESS, 0, 0),
        Color::Green => (0, BRIGHTNESS, 0),
        Color::Blue => (0, 0, BRIGHTNESS),
        Color::Yellow => (BRIGHTNESS / 2, BRIGHTNESS / 2, 0),
        Color::Cyan => (0, BRIGHTNESS / 2, BRIGHTNESS / 2),
        Color::Magenta => (BRIGHTNESS / 2, 0, BRIGHTNESS / 2),
        Color::White => (BRIGHTNESS / 3, BRIGHTNESS / 3, BRIGHTNESS / 3),
        _ => (0, 0, 0),
    }
}

fn board_to_led_index(x: usize, y: usize, flip_y: bool) -> usize {
    let y_mapped = if flip_y {
        BOARD_HEIGHT - 1 - y
    } else {
        y
    };
    let col_start = x * BOARD_HEIGHT;
    if x % 2 == 0 {
        col_start + y_mapped
    } else {
        col_start + (BOARD_HEIGHT - 1 - y_mapped)
    }
}

async fn ble_tetris_run<C>(
    controller: C,
    status_led: &'static mut Output<'static>,
    rmt_channel: &'static esp_hal::rmt::Channel<'static, esp_hal::Async, esp_hal::rmt::Tx>,
    game_to_render: &'static Channel<CriticalSectionRawMutex, GameState, 1>,
) where
    C: Controller,
{
    let address: Address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);
    info!("Our address = {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let Host {
        mut peripheral,
        runner,
        ..
    } = stack.build();

    info!("Starting advertising and GATT Tetris service");
    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: "ESP-TETRIS",
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    }))
    .unwrap();

    // Spawn render task with exclusive access to RMT channel
    let rmt_owned = unsafe { core::ptr::read(rmt_channel) };

    // Wrap in a static RefCell for interior mutability
    static RMT_MUT: StaticCell<core::cell::RefCell<esp_hal::rmt::Channel<'static, esp_hal::Async, esp_hal::rmt::Tx>>> = StaticCell::new();
    let rmt_ref: &'static _ = RMT_MUT.init(core::cell::RefCell::new(rmt_owned));

    let render_spawner = unsafe { embassy_executor::Spawner::for_current_executor() }.await;
    let token = render_task(rmt_ref, game_to_render);
    if let Ok(t) = token {
        render_spawner.spawn(t);
    } else {
        info!("Failed to create render task token");
    }

    let _ = join(ble_task(runner), async {
        loop {
            match advertise(&mut peripheral, &server).await {
                Ok(conn) => {
                    status_led.set_high();
                    let a = gatt_game_task(&server, &conn, status_led, game_to_render);
                    let b = connection_task();
                    select(a, b).await;
                    status_led.set_low();
                }
                Err(e) => {
                    info!("[adv] error: {:?}", e);
                }
            }
        }
    })
    .await;
}

async fn ble_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if let Err(e) = runner.run().await {
            info!("[ble_task] error: {:?}", e);
        }
    }
}

async fn connection_task() {
    loop {
        embassy_time::Timer::after_secs(30).await;
    }
}

async fn gatt_game_task<P: PacketPool>(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, P>,
    status_led: &mut Output<'static>,
    game_to_render: &'static Channel<CriticalSectionRawMutex, GameState, 1>,
) -> Result<(), Error> {
    let control_char = server.tetris_service.control;

    let mut game = Tetris::new(TetrisRng);
    let mut last_update = embassy_time::Instant::now();
    let fall_interval = embassy_time::Duration::from_millis(FALL_INTERVAL_MS);
    let mut pending_cmd = CMD_NONE;
    let mut cmd_processed = true;

    loop {
        // Handle timing - auto fall (always check)
        let now = embassy_time::Instant::now();
        if now - last_update >= fall_interval {
            game.move_down();
            last_update = now;
        }

        // Handle pending BLE command
        if !cmd_processed {
            match pending_cmd {
                CMD_LEFT => { game.move_left(); }
                CMD_RIGHT => { game.move_right(); }
                CMD_ROTATE => { game.rotate(); }
                CMD_DOWN => {
                    game.move_down();
                    last_update = now;
                }
                CMD_START => {
                    if game.is_game_over() {
                        game = Tetris::new(TetrisRng);
                    }
                }
                _ => {}
            }
            cmd_processed = true;
        }

        // Send game state to render task (non-blocking, overwrite if full)
        let state = extract_game_state(&game);
        game_to_render.try_send(state).ok();

        // Wait for either a BLE event or timeout, then process
        // This ensures we check auto-fall at least every 50ms
        let event = select(conn.next(), embassy_time::Timer::after_millis(50)).await;

        match event {
            Either::First(conn_event) => {
                match conn_event {
                    GattConnectionEvent::Disconnected { .. } => break,
                    GattConnectionEvent::Gatt { event } => {
                        match &event {
                            GattEvent::Write(event) => {
                                if event.handle() == control_char.handle {
                                    let data = event.data();
                                    if !data.is_empty() {
                                        pending_cmd = data[0];
                                        cmd_processed = false;
                                        info!("[gatt] Tetris command: {}", pending_cmd);
                                    }
                                }
                            }
                            _ => {}
                        };
                        match event.accept() {
                            Ok(reply) => reply.send().await,
                            Err(e) => info!("[gatt] error sending response: {:?}", e),
                        };
                    }
                    _ => {}
                }
            }
            Either::Second(_) => {
                // Timeout - continue loop to check auto-fall
            }
        }
    }
    info!("[gatt] disconnected");
    Ok(())
}

// Render task - receives game state and updates LED strip
#[embassy_executor::task]
async fn render_task(
    rmt_cell: &'static core::cell::RefCell<esp_hal::rmt::Channel<'static, esp_hal::Async, esp_hal::rmt::Tx>>,
    game_from_gatt: &'static Channel<CriticalSectionRawMutex, GameState, 1>,
) {
    let mut last_state = GameState::default();

    loop {
        // Wait for new game state
        let state = game_from_gatt.receive().await;

        // Only render when state actually changes to avoid flickering
        if state_changed(&last_state, &state) {
            // Render with exclusive access to RMT channel
            let mut rmt = rmt_cell.borrow_mut();
            render_board(&state, &mut rmt).await;
            last_state = state;
        }
    }
}

// Render game state to LED strip
async fn render_board(
    state: &GameState,
    channel: &mut esp_hal::rmt::Channel<'static, esp_hal::Async, esp_hal::rmt::Tx>,
) {
    let mut led_colors = [(0u8, 0u8, 0u8); 200];

    // Render board state (locked cells)
    for y in 0..BOARD_HEIGHT {
        for x in 0..BOARD_WIDTH {
            if let Some(color) = state.board[y][x] {
                let led_idx = board_to_led_index(x, y, true);
                led_colors[led_idx] = color_to_rgb(color);
            }
        }
    }

    // Render current piece (if not game over)
    if !state.game_over {
        for &(dx, dy) in &state.current_piece {
            let x = (state.piece_x + dx) as usize;
            let y = (state.piece_y + dy) as usize;
            if x < BOARD_WIDTH && y < BOARD_HEIGHT {
                let led_idx = board_to_led_index(x, y, true);
                led_colors[led_idx] = color_to_rgb(state.current_color);
            }
        }
    }

    // Send data to LED strip
    for (_, &(r, g, b)) in led_colors.iter().enumerate() {
        let data = create_led_bits(r, g, b);
        let rt = channel.transmit(&data);
        match rt.await {
            Ok(_) => {}
            Err(e) => {
                info!("LED transmit error: {:?}", e);
                break;
            }
        }
    }
}

fn create_led_bits(r: u8, g: u8, b: u8) -> [PulseCode; 25] {
    use esp_hal::gpio::Level;

    let mut data = [PulseCode::default(); 25];
    let bytes = [g, r, b];

    let mut idx = 0;
    for byte in bytes {
        for bit in (0..8).rev() {
            data[idx] = if (byte & (1 << bit)) != 0 {
                PulseCode::new(Level::High, T1H, Level::Low, T1L)
            } else {
                PulseCode::new(Level::High, T0H, Level::Low, T0L)
            };
            idx += 1;
        }
    }
    data[24] = PulseCode::new(Level::Low, 800, Level::Low, 0);
    data
}

async fn advertise<'values, 'server, C: Controller>(
    peripheral: &mut Peripheral<'values, C, DefaultPacketPool>,
    server: &'server Server<'values>,
) -> Result<GattConnection<'values, 'server, DefaultPacketPool>, BleHostError<C::Error>>
{
    let mut advertiser_data = [0; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(b"ESP-TETRIS"),
        ],
        &mut advertiser_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &advertiser_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    info!("[adv] advertising");
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    info!("[adv] connection established");
    Ok(conn)
}