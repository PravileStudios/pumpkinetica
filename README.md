# Pumpkinetica

A [Pumpkin MC](https://github.com/Pumpkin-MC/Pumpkin) plugin for loading, pasting, and editing `.litematica` and `.schem` schematics — a lightweight WorldEdit-style clipboard built as a WASM component plugin.

## Features

- **Format support** — `.litematica` / `.litematic` (Litematica) and `.schem` (WorldEdit/Sponge v1–v3)
- **Selection wand** — set two corners by looking at blocks or with `pos1`/`pos2`
- **Clipboard ops** — copy a selection, paste, rotate, flip, and save back to `.schem`
- **In-world editing** — `set` and `replace` blocks across a selection
- **Undo / redo** — per-player history, volume-bounded
- **Async pasting** — blocks placed over multiple ticks to avoid lag spikes; multiple players can run ops concurrently (configurable limit)
- **Block state resolution** — full property support (stairs, slabs, fences, rotation/mirroring of directional blocks)
- **Block entities** — chests, barrels, furnaces, signs, etc. keep their contents and GUIs on paste
- **Fallback blocks** — configurable replacement (or skip) for unresolved block types
- **Debug logging** — opt-in server-side diagnostics

## Commands

Base command `/schematic` (alias `/schem`). Run with no args for help.

| Command | Description |
|---|---|
| `/schem load <file>` | Load a schematic file into your clipboard |
| `/schem paste` | Paste clipboard (or loaded schematic) at your position |
| `/schem list` | List available schematic files |
| `/schem info` | Show details of the loaded schematic |
| `/schem status` | Show active paste operations |
| `/schem wand` | Get the selection wand |
| `/schem pos1` / `pos2` | Set selection corner at the looked-at block |
| `/schem copy` | Copy the current selection to the clipboard |
| `/schem save <name>` | Save the clipboard to a `.schem` file |
| `/schem rotate <90\|180\|270>` | Rotate the clipboard |
| `/schem flip <x\|z>` | Mirror the clipboard |
| `/schem set <block>` | Fill the selection with a block |
| `/schem replace <from> <to>` | Replace blocks in the selection |
| `/schem undo` | Undo the last operation |
| `/schem redo` | Redo the last undone operation |
| `/schem reload` | Reload config from disk |
| `/schem help` | Show help |

Requires the `pumpkinetica:command.schematic` permission (OP level 2 by default).

## Installation

Builds are produced on a remote build host (see `deploy.sh`), never locally.

1. Build + componentize:
```bash
cargo build --target wasm32-wasip1 --release
wasm-tools component new target/wasm32-wasip1/release/pumpkinetica.wasm \
  -o pumpkinetica_component.wasm \
  --adapt wasi_snapshot_preview1.reactor.wasm
```

2. Copy `pumpkinetica_component.wasm` to your Pumpkin server's `plugins/` directory as `pumpkinetica.wasm`.

3. Add the WASM file's SHA-256 hash to `plugins/permission_cache.json`.

4. Start the server. The plugin creates its config and `schematics/` directory automatically.

## Configuration

Config file: `plugins/data/pumpkinetica/config.json`

```json
{
  "fallback_block": "minecraft:cobblestone",
  "blocks_per_tick": 4096,
  "max_concurrent_pastes": 4,
  "wand_item": "minecraft:wooden_axe",
  "max_undo_history": 20,
  "max_selection_volume": 10000000,
  "max_undo_volume": 1000000,
  "debug": false
}
```

| Option | Default | Description |
|---|---|---|
| `fallback_block` | `minecraft:cobblestone` | Block used when a schematic block can't be resolved. Set to `skip` to omit unresolved blocks. |
| `blocks_per_tick` | `4096` | Blocks placed per server tick. Higher = faster paste, more lag. |
| `max_concurrent_pastes` | `4` | Maximum simultaneous paste operations server-wide. |
| `wand_item` | `minecraft:wooden_axe` | Item used as the selection wand. |
| `max_undo_history` | `20` | Undo steps kept per player. |
| `max_selection_volume` | `10000000` | Maximum selectable volume, in blocks. |
| `max_undo_volume` | `1000000` | Operations larger than this skip undo recording (bounds memory). |
| `debug` | `false` | Enable server-side diagnostic logging. |

Adding a new config key? Existing `config.json` files keep their values — new keys fall back to defaults in memory but aren't written back automatically.

## Usage

1. Drop schematic files in `plugins/data/pumpkinetica/schematics/`, or build a selection in-world.
2. `/schem list` to see available files.
3. `/schem load mybuilding.litematic` (or `/schem wand` → set `pos1`/`pos2` → `/schem copy`).
4. Stand at the origin point.
5. `/schem paste`. Use `/schem undo` to revert.

## License

MIT
