# ESP32-C3 BLE Tetris

A no_std Tetris game running on ESP32-C3 with NeoPixel LED display, controllable via BLE from any modern web browser.

## Hardware Requirements

- ESP32-C3 development board
- NeoPixel RGB LED strip (10x20 matrix = 200 LEDs)
- Connection status LED on GPIO8

## Wiring

| Component | GPIO |
|-----------|------|
| LED Strip Data | GPIO4 |
| Status LED | GPIO8 |

## LED Driving Method

This project uses **SPI DMA** to drive the NeoPixel LED strip, not the RMT (Remote Control) peripheral.

### Why SPI Instead of RMT?

RMT (Remote Control peripheral) and BLE share hardware resources on ESP32-C3. When both are used simultaneously, RMT transfers cause BLE connection drops and communication errors. To maintain stable BLE connectivity, we drive NeoPixels via SPI DMA instead:

- **SPI2** peripheral with DMA channel 0
- **6.4 MHz SPI clock** - produces correct NeoPixel timing:
  - '1' bit: 0xF0 (11110000) → ~625ns high, ~625ns low
  - '0' bit: 0xC0 (11000000) → ~312ns high, ~937ns low
- **DMA transfer** - non-blocking bulk transfer to LED strip
- **Software reset** - 300μs gap after each frame (WS2812B requirement)

This approach trades the RMT's hardware-assisted timing for BLE stability.

## Features

- BLE GATT server for wireless control
- Web Bluetooth interface (no app installation required)
- NeoPixel LED matrix display (serpentine wiring)
- Auto-falling tetrominos with manual controls

## Controls

| Button | Action |
|--------|--------|
| Left | Move piece left |
| Right | Move piece right |
| Rotate | Rotate piece clockwise |
| Drop | Drop piece faster |
| Start | Start/Restart game |

## Building

```bash
# Build the firmware
cargo build --release
```

## Flashing

```bash
# Using espflash
cargo espflash flash --release --monitor
```

## Web Interface

The control webpage is hosted at: **https://hahihula.github.io/esp-ble-nostd-tetris/**

Alternatively, serve locally:
```bash
# Serve the web folder
caddy run
```

Then open http://localhost:443 in Chrome/Edge (Web Bluetooth required). ( Bluetooth web need https )

## Architecture

- `src/bin/main.rs` - Main firmware with BLE GATT server and game logic
- `web/index.html` - Web Bluetooth control interface
- `no_std_tetris/` - Tetris game logic library

## BLE Service UUIDs

| Service | `12345678-1234-5678-1234-56789abcdef0` |
|---------|---------------------------------------|
| Control Characteristic | `12345678-1234-5678-1234-56789abcdef1` (write) |

## License

MIT