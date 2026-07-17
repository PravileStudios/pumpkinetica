use std::collections::HashMap;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world::{self, World};

use crate::parser::{Region, Schematic};
use crate::paste::{BlockPlacement, TileEntityPlacement};

const MAX_CLIPBOARD_BLOCKS: i64 = 64 * 1024 * 1024;

pub(crate) struct Clipboard {
    pub name: String,
    pub blocks: Vec<u16>,
    pub tile_entities: Vec<(BlockPos, Vec<u8>)>,
    pub size_x: i32,
    pub size_y: i32,
    pub size_z: i32,
    pub offset: BlockPos,
}

fn region_min(r: &Region) -> [i32; 3] {
    let n = |p: i32, s: i32| if s < 0 { p + s + 1 } else { p };
    [
        n(r.position[0], r.size[0]),
        n(r.position[1], r.size[1]),
        n(r.position[2], r.size[2]),
    ]
}

pub(crate) fn schematic_to_clipboard(
    name: String,
    schematic: &Schematic,
    palette_map: &[Vec<Option<u16>>],
) -> Result<Clipboard, String> {
    if schematic.regions.is_empty() {
        return Err("Schematic has no regions".into());
    }

    let mut gmin = [i32::MAX; 3];
    let mut gmax = [i32::MIN; 3];
    for r in &schematic.regions {
        let m = region_min(r);
        let [ax, ay, az] = r.abs_size();
        for i in 0..3 {
            gmin[i] = gmin[i].min(m[i]);
        }
        gmax[0] = gmax[0].max(m[0] + ax - 1);
        gmax[1] = gmax[1].max(m[1] + ay - 1);
        gmax[2] = gmax[2].max(m[2] + az - 1);
    }

    let (sx, sy, sz) = (
        gmax[0] - gmin[0] + 1,
        gmax[1] - gmin[1] + 1,
        gmax[2] - gmin[2] + 1,
    );
    let vol = sx as i64 * sy as i64 * sz as i64;
    if vol <= 0 || vol > MAX_CLIPBOARD_BLOCKS {
        return Err(format!("Schematic bounding box too large ({vol} blocks)"));
    }

    let mut blocks = vec![0u16; vol as usize];
    let mut tile_entities = Vec::new();

    for (ri, r) in schematic.regions.iter().enumerate() {
        let m = region_min(r);
        let base = [m[0] - gmin[0], m[1] - gmin[1], m[2] - gmin[2]];
        let [ax, ay, az] = r.abs_size();
        let pm = &palette_map[ri];

        for y in 0..ay {
            for z in 0..az {
                for x in 0..ax {
                    let pidx = r.get_palette_index(x, y, z) as usize;
                    let state = pm.get(pidx).copied().flatten().unwrap_or(0);
                    if state == 0 {
                        continue;
                    }
                    let (gx, gy, gz) = (base[0] + x, base[1] + y, base[2] + z);
                    let idx = (gy * sx * sz + gz * sx + gx) as usize;
                    blocks[idx] = state;
                }
            }
        }

        for te in &r.tile_entities {
            tile_entities.push((
                BlockPos {
                    x: base[0] + te.x,
                    y: base[1] + te.y,
                    z: base[2] + te.z,
                },
                te.raw_nbt.clone(),
            ));
        }
    }

    Ok(Clipboard {
        name,
        blocks,
        tile_entities,
        size_x: sx,
        size_y: sy,
        size_z: sz,
        offset: BlockPos {
            x: -gmin[0],
            y: -gmin[1],
            z: -gmin[2],
        },
    })
}

impl Clipboard {
    fn idx(&self, x: i32, y: i32, z: i32) -> usize {
        (y * self.size_x * self.size_z + z * self.size_x + x) as usize
    }

    pub fn to_work_queue(
        &self,
        origin: BlockPos,
        at_feet: bool,
    ) -> (Vec<BlockPlacement>, Vec<TileEntityPlacement>) {
        let off = if at_feet {
            BlockPos { x: 0, y: 0, z: 0 }
        } else {
            self.offset
        };
        let mut queue = Vec::with_capacity(self.blocks.len());
        for y in 0..self.size_y {
            for z in 0..self.size_z {
                for x in 0..self.size_x {
                    let state_id = self.blocks[self.idx(x, y, z)];
                    if state_id == 0 {
                        continue;
                    }
                    queue.push(BlockPlacement {
                        pos: BlockPos {
                            x: origin.x - off.x + x,
                            y: origin.y - off.y + y,
                            z: origin.z - off.z + z,
                        },
                        state_id,
                    });
                }
            }
        }

        let te_queue = self.tile_entities.iter().map(|(rel_pos, nbt)| {
            TileEntityPlacement {
                pos: BlockPos {
                    x: origin.x - off.x + rel_pos.x,
                    y: origin.y - off.y + rel_pos.y,
                    z: origin.z - off.z + rel_pos.z,
                },
                nbt: nbt.clone(),
            }
        }).collect();

        (queue, te_queue)
    }
}

// The host serializes block-entity NBT with an unnamed root, but
// set_block_entity_nbt parses it as named. Insert an empty root name so the
// copied bytes round-trip and chest contents survive the paste.
fn to_named_root(nbt: Vec<u8>) -> Vec<u8> {
    if nbt.is_empty() {
        return nbt;
    }
    let mut out = Vec::with_capacity(nbt.len() + 2);
    out.push(nbt[0]);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&nbt[1..]);
    out
}

pub(crate) fn read_selection_chunk_batched(
    world: &World,
    min: BlockPos,
    max: BlockPos,
    sx: i32,
    sy: i32,
    sz: i32,
) -> (Vec<u16>, Vec<(BlockPos, Vec<u8>)>) {
    let total = (sx * sy * sz) as usize;
    let mut blocks = vec![0u16; total];
    let mut tile_entities = Vec::new();

    let min_cx = min.x.div_euclid(16);
    let max_cx = max.x.div_euclid(16);
    let min_cz = min.z.div_euclid(16);
    let max_cz = max.z.div_euclid(16);

    for cx in min_cx..=max_cx {
        for cz in min_cz..=max_cz {
            let chunk = world.get_chunk(cx, cz);

            let chunk_min_x = cx * 16;
            let chunk_max_x = chunk_min_x + 15;
            let chunk_min_z = cz * 16;
            let chunk_max_z = chunk_min_z + 15;

            let overlap_min_x = min.x.max(chunk_min_x);
            let overlap_max_x = max.x.min(chunk_max_x);
            let overlap_min_z = min.z.max(chunk_min_z);
            let overlap_max_z = max.z.min(chunk_max_z);

            for y in min.y..=max.y {
                for z in overlap_min_z..=overlap_max_z {
                    for x in overlap_min_x..=overlap_max_x {
                        let pos = BlockPos { x, y, z };
                        let state_id = match &chunk {
                            Some(c) => {
                                let local = BlockPos {
                                    x: x.rem_euclid(16),
                                    y,
                                    z: z.rem_euclid(16),
                                };
                                c.get_block_state_id(local)
                            }
                            None => world.get_block_state_id(pos),
                        };
                        let rel_x = x - min.x;
                        let rel_y = y - min.y;
                        let rel_z = z - min.z;
                        let idx = (rel_y * sx * sz + rel_z * sx + rel_x) as usize;
                        blocks[idx] = state_id;

                        // Air never carries a block entity — skip the host call.
                        if state_id != 0
                            && let Some(nbt) = world.get_block_entity_nbt(pos)
                        {
                            tile_entities.push((
                                BlockPos { x: rel_x, y: rel_y, z: rel_z },
                                to_named_root(nbt),
                            ));
                        }
                    }
                }
            }
        }
    }

    (blocks, tile_entities)
}

pub(crate) fn rotate_clipboard(clip: &mut Clipboard, degrees: i32) {
    let (sx, sy, sz) = (clip.size_x, clip.size_y, clip.size_z);
    let (new_sx, new_sz) = match degrees {
        90 | 270 => (sz, sx),
        180 => (sx, sz),
        _ => return,
    };

    let mut new_blocks = vec![0u16; clip.blocks.len()];
    for y in 0..sy {
        for z in 0..sz {
            for x in 0..sx {
                let old_idx = (y * sx * sz + z * sx + x) as usize;
                let (nx, nz) = match degrees {
                    90 => (sz - 1 - z, x),
                    180 => (sx - 1 - x, sz - 1 - z),
                    270 => (z, sx - 1 - x),
                    _ => unreachable!(),
                };
                let new_idx = (y * new_sx * new_sz + nz * new_sx + nx) as usize;
                new_blocks[new_idx] = rotate_block_state(clip.blocks[old_idx], degrees);
            }
        }
    }

    clip.blocks = new_blocks;
    clip.size_x = new_sx;
    clip.size_z = new_sz;

    for te in &mut clip.tile_entities {
        let (tx, tz) = (te.0.x, te.0.z);
        let (nx, nz) = match degrees {
            90 => (sz - 1 - tz, tx),
            180 => (sx - 1 - tx, sz - 1 - tz),
            270 => (tz, sx - 1 - tx),
            _ => unreachable!(),
        };
        te.0.x = nx;
        te.0.z = nz;
    }

    // Re-anchor the offset the same way block indices are re-anchored above
    // (the `sz - 1 - z` / `sx - 1 - x` terms). A pure vector rotation would
    // shift the paste anchor by up to size-1 blocks.
    let (ox, oz) = (clip.offset.x, clip.offset.z);
    match degrees {
        90 => {
            clip.offset.x = sz - 1 - oz;
            clip.offset.z = ox;
        }
        180 => {
            clip.offset.x = sx - 1 - ox;
            clip.offset.z = sz - 1 - oz;
        }
        270 => {
            clip.offset.x = oz;
            clip.offset.z = sx - 1 - ox;
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub(crate) enum FlipAxis {
    X,
    Z,
}

pub(crate) fn flip_clipboard(clip: &mut Clipboard, axis: FlipAxis) {
    let (sx, sy, sz) = (clip.size_x, clip.size_y, clip.size_z);
    let mut new_blocks = vec![0u16; clip.blocks.len()];

    for y in 0..sy {
        for z in 0..sz {
            for x in 0..sx {
                let old_idx = (y * sx * sz + z * sx + x) as usize;
                let (nx, nz) = match axis {
                    FlipAxis::X => (sx - 1 - x, z),
                    FlipAxis::Z => (x, sz - 1 - z),
                };
                let new_idx = (y * sx * sz + nz * sx + nx) as usize;
                new_blocks[new_idx] = flip_block_state(clip.blocks[old_idx], axis);
            }
        }
    }

    clip.blocks = new_blocks;

    for te in &mut clip.tile_entities {
        match axis {
            FlipAxis::X => te.0.x = sx - 1 - te.0.x,
            FlipAxis::Z => te.0.z = sz - 1 - te.0.z,
        }
    }

    // Re-anchor the offset to match the mirrored block indices (`sx - 1 - x`).
    match axis {
        FlipAxis::X => clip.offset.x = sx - 1 - clip.offset.x,
        FlipAxis::Z => clip.offset.z = sz - 1 - clip.offset.z,
    }
}

fn rotate_block_state(state_id: u16, degrees: i32) -> u16 {
    if state_id == 0 {
        return 0;
    }

    let Some(info) = world::block_state_to_info(state_id) else {
        return state_id;
    };

    let mut props: HashMap<String, String> = info.properties.into_iter().collect();
    let mut changed = false;

    if let Some(facing) = props.get("facing")
        && let Some(new_facing) = rotate_facing(facing, degrees)
    {
        props.insert("facing".into(), new_facing.into());
        changed = true;
    }

    if let Some(axis) = props.get("axis")
        && let Some(new_axis) = rotate_axis(axis, degrees)
    {
        props.insert("axis".into(), new_axis.into());
        changed = true;
    }

    if let Some(rotation) = props.get("rotation")
        && let Ok(r) = rotation.parse::<i32>()
    {
        let steps = degrees / 90;
        let new_r = ((r + steps * 4) % 16 + 16) % 16;
        props.insert("rotation".into(), new_r.to_string());
        changed = true;
    }

    if !changed {
        return state_id;
    }

    let prop_vec: Vec<(String, String)> = props.into_iter().collect();
    world::resolve_block_state(&info.name, &prop_vec).unwrap_or(state_id)
}

fn flip_block_state(state_id: u16, axis: FlipAxis) -> u16 {
    if state_id == 0 {
        return 0;
    }

    let Some(info) = world::block_state_to_info(state_id) else {
        return state_id;
    };

    let mut props: HashMap<String, String> = info.properties.into_iter().collect();
    let mut changed = false;

    if let Some(facing) = props.get("facing") {
        let new_facing = match axis {
            FlipAxis::X => match facing.as_str() {
                "east" => Some("west"),
                "west" => Some("east"),
                _ => None,
            },
            FlipAxis::Z => match facing.as_str() {
                "north" => Some("south"),
                "south" => Some("north"),
                _ => None,
            },
        };
        if let Some(nf) = new_facing {
            props.insert("facing".into(), nf.into());
            changed = true;
        }
    }

    if !changed {
        return state_id;
    }

    let prop_vec: Vec<(String, String)> = props.into_iter().collect();
    world::resolve_block_state(&info.name, &prop_vec).unwrap_or(state_id)
}

fn rotate_facing(facing: &str, degrees: i32) -> Option<&'static str> {
    let order = ["north", "east", "south", "west"];
    let idx = order.iter().position(|&f| f == facing)?;
    let steps = ((degrees / 90) % 4 + 4) % 4;
    Some(order[(idx + steps as usize) % 4])
}

fn rotate_axis(axis: &str, degrees: i32) -> Option<&'static str> {
    if degrees % 180 == 0 {
        return None;
    }
    match axis {
        "x" => Some("z"),
        "z" => Some("x"),
        _ => None,
    }
}

pub(crate) fn clipboard_to_schem_data(
    clip: &Clipboard,
) -> (HashMap<String, u16>, Vec<u16>, usize) {
    let mut palette_map: HashMap<u16, u16> = HashMap::new();
    let mut palette_strings: HashMap<String, u16> = HashMap::new();
    let mut next_idx: u16 = 0;
    let mut unresolved = 0usize;

    let mut indices = Vec::with_capacity(clip.blocks.len());

    for &state_id in &clip.blocks {
        let palette_idx = if let Some(&existing) = palette_map.get(&state_id) {
            existing
        } else {
            let block_str = if state_id == 0 {
                "minecraft:air".to_string()
            } else if let Some(info) = world::block_state_to_info(state_id) {
                if info.properties.is_empty() {
                    info.name
                } else {
                    let props: Vec<String> = info
                        .properties
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    format!("{}[{}]", info.name, props.join(","))
                }
            } else {
                unresolved += 1;
                "minecraft:stone".to_string()
            };

            let idx = if let Some(&existing_idx) = palette_strings.get(&block_str) {
                existing_idx
            } else {
                let idx = next_idx;
                palette_strings.insert(block_str, idx);
                next_idx += 1;
                idx
            };
            palette_map.insert(state_id, idx);
            idx
        };
        indices.push(palette_idx);
    }

    (palette_strings, indices, unresolved)
}
