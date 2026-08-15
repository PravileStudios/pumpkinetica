# Pumpkinetica

A [Pumpkin MC](https://github.com/Pumpkin-MC/Pumpkin) plugin that loads and pastes `.litematica` and `.schem` schematics into the world.

Built as a WASM plugin using the Pumpkin Plugin API.

## Features

- **Format support** — `.litematica` (Litematica) and `.schem` (WorldEdit/Sponge v1-v3)
- **Async pasting** — blocks placed over multiple ticks to avoid lag spikes
- **Concurrent pastes** — multiple players can paste simultaneously (configurable limit)
- **Block state resolution** — full property support (stairs, slabs, fences, etc.)
- **Fallback blocks** — configurable replacement for unsupported block types
- **Tile entities** — signs, chests, banners, etc. preserved via raw NBT
- **Custom NBT parser** — minimal parser built for WASM compatibility

## Commands

| Command | Description |
|---|---|
| `/schematic load <file>` | Load a schematic file |
| `/schematic paste` | Paste loaded schematic at your position |
| `/schematic list` | List available schematic files |
| `/schematic info` | Show details of loaded schematic |
| `/schematic status` | Show active paste operations |
| `/schematic reload` | Reload config from disk |
| `/schematic help` | Show help |

Alias: `/schem` works the same as `/schematic`.

Requires `pumpkinetica:command.schematic` permission (OP level 2 by default).

## Installation

1. Build the plugin:
```bash
cargo build --target wasm32-wasip1 --release
wasm-tools component new target/wasm32-wasip1/release/pumpkinetica.wasm \
  -o pumpkinetica_component.wasm \
  --adapt wasi_snapshot_preview1.reactor.wasm
```

2. Copy `pumpkinetica_component.wasm` to your Pumpkin server's `plugins/` directory as `pumpkinetica.wasm`.

3. Add the WASM file's SHA-256 hash to `plugins/permission_cache.json`.

4. Start the server. The plugin creates its config and schematics directory automatically.

## Configuration

Config file: `plugins/pumpkinetica/config.json`

```json
{
  "fallback_block": "minecraft:cobblestone",
  "blocks_per_tick": 4096,
  "max_concurrent_pastes": 4
}
```

| Option | Default | Description |
|---|---|---|
| `fallback_block` | `minecraft:cobblestone` | Block used when a schematic block type can't be resolved. Set to `skip` to omit unsupported blocks. |
| `blocks_per_tick` | `4096` | Number of blocks placed per server tick. Higher = faster paste, more lag. |
| `max_concurrent_pastes` | `4` | Maximum simultaneous paste operations server-wide. |

## Usage

1. Place schematic files in `plugins/pumpkinetica/schematics/`
2. In-game: `/schematic list` to see available files
3. `/schematic load mybuilding.litematica`
4. Stand where you want the origin point
5. `/schematic paste`

## Project Structure

```
src/
├── lib.rs       — Plugin entry, config, messaging, block resolution
├── commands.rs  — Command handlers (load, paste, list, info, status, reload, help)
├── paste.rs     — Paste engine (chunk batching, tick scheduler)
└── parser.rs    — Format parsers (.litematica + .schem), NBT parser
```

## License

MIT
