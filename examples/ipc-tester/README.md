# Pumpkinetica IPC tester

Example Pumpkin plugin that drives Pumpkinetica over plugin IPC. Use it to
exercise the `pumpkinetica:*/v1` message contract end to end.

## Build

Same remote flow as the main plugin (never build locally):

```bash
cargo build --target wasm32-wasip1 --release
wasm-tools component new \
  target/wasm32-wasip1/release/pumpkinetica_ipc_tester.wasm \
  -o pumpkinetica_ipc_tester_component.wasm \
  --adapt wasi_snapshot_preview1.reactor.wasm
```

Drop the component `.wasm` in the server `plugins/` dir alongside pumpkinetica,
add its sha256 to `plugins/permission_cache.json`, and start the server.

## Commands

`/pptest` (op level 2):

| Sub | Sends | Notes |
|-----|-------|-------|
| `caps` | `capabilities/v1` | discovery; reply lists `kinds` + `external_paths` |
| `list` | `list/v1` | schematics in the plugin dir |
| `status` | `status/v1` | active vs max concurrent pastes |
| `paste <name>` | `paste/v1` with `schematic` | pastes at your feet, your dimension |
| `pastepath <abs path>` | `paste/v1` with `path` | needs `ipc_allow_external_paths` + an allowlisted dir |

Replies print in chat: green = `ok:true`, yellow = `ok:false` (with `code`),
red = protocol failure or target unreachable.

## Testing external paths

1. In `plugins/data/pumpkinetica/config.json` set `ipc_allow_external_paths: true`
   and add a dir to `ipc_allowed_paste_dirs`, e.g. `["/srv/shared/builds"]`.
2. `/schematic reload` (or restart) so pumpkinetica picks up the config.
3. `/pptest pastepath /srv/shared/builds/castle.schem` — expect `ok:true`.
4. `/pptest pastepath /etc/passwd` — expect `ok:false` with `PATH_DENIED`.
