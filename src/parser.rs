use std::collections::HashMap;
use std::io::{Cursor, Read};

use flate2::read::GzDecoder;

#[derive(Debug)]
pub struct Schematic {
    pub name: String,
    pub regions: Vec<Region>,
}

#[derive(Debug)]
pub struct Region {
    pub name: String,
    pub position: [i32; 3],
    pub size: [i32; 3],
    pub palette: Vec<PaletteEntry>,
    pub block_data: Vec<i64>,
    pub tile_entities: Vec<TileEntity>,
}

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub name: String,
    pub properties: HashMap<String, String>,
}

#[derive(Debug)]
pub struct TileEntity {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub raw_nbt: Vec<u8>,
}

impl Region {
    pub fn abs_size(&self) -> [i32; 3] {
        [self.size[0].abs(), self.size[1].abs(), self.size[2].abs()]
    }

    pub fn bits_per_entry(&self) -> u32 {
        let palette_size = self.palette.len() as u32;
        std::cmp::max(2, 32 - palette_size.saturating_sub(1).leading_zeros())
    }

    pub fn get_block_index(&self, x: i32, y: i32, z: i32) -> usize {
        let [sx, _, sz] = self.abs_size();
        (y * sx * sz + z * sx + x) as usize
    }

    pub fn get_palette_index(&self, x: i32, y: i32, z: i32) -> u16 {
        let block_index = self.get_block_index(x, y, z);
        let bits = self.bits_per_entry();
        let mask = (1u64 << bits) - 1;

        let start_offset = (block_index as u64) * (bits as u64);
        let start_arr_index = (start_offset >> 6) as usize;
        let start_bit_offset = (start_offset & 0x3F) as u32;

        if start_arr_index >= self.block_data.len() {
            return 0;
        }

        let end_arr_index = (((block_index as u64 + 1) * (bits as u64)) - 1) >> 6;

        if start_arr_index == end_arr_index as usize {
            ((self.block_data[start_arr_index] as u64 >> start_bit_offset) & mask) as u16
        } else {
            let end_idx = end_arr_index as usize;
            if end_idx >= self.block_data.len() {
                return 0;
            }
            let end_offset = 64 - start_bit_offset;
            (((self.block_data[start_arr_index] as u64 >> start_bit_offset)
                | (self.block_data[end_idx] as u64) << end_offset)
                & mask) as u16
        }
    }
}

pub fn parse_litematica(data: &[u8]) -> Result<Schematic, String> {
    let mut decoder = GzDecoder::new(Cursor::new(data));
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("GZIP decode failed: {e}"))?;

    let nbt = parse_nbt(&decompressed)?;

    let root = match nbt {
        NbtValue::Compound(map) => map,
        _ => return Err("Root is not a compound".into()),
    };

    let schematic_name = get_compound(&root, "Metadata")
        .and_then(|m| get_string(m, "Name"))
        .unwrap_or_else(|| "unknown".into());

    let regions_compound = get_compound(&root, "Regions").ok_or("Missing 'Regions' tag")?;

    let mut regions = Vec::new();
    for (region_name, region_val) in regions_compound {
        let region = match region_val {
            NbtValue::Compound(r) => r,
            _ => continue,
        };

        let position = get_int_triple(region, "Position")?;
        let size = get_int_triple(region, "Size")?;

        let palette = parse_palette(region)?;
        let block_data = get_long_array(region, "BlockStates")?;
        let tile_entities = parse_tile_entities(region);

        regions.push(Region {
            name: region_name.clone(),
            position,
            size,
            palette,
            block_data,
            tile_entities,
        });
    }

    Ok(Schematic {
        name: schematic_name,
        regions,
    })
}

fn parse_palette(region: &HashMap<String, NbtValue>) -> Result<Vec<PaletteEntry>, String> {
    let list = match region.get("BlockStatePalette") {
        Some(NbtValue::List(l)) => l,
        _ => return Err("Missing BlockStatePalette".into()),
    };

    let mut palette = Vec::with_capacity(list.len());
    for entry in list {
        let compound = match entry {
            NbtValue::Compound(c) => c,
            _ => continue,
        };

        let name = get_string(compound, "Name").unwrap_or_default();
        let mut properties = HashMap::new();

        if let Some(NbtValue::Compound(props)) = compound.get("Properties") {
            for (k, v) in props {
                if let NbtValue::String(val) = v {
                    properties.insert(k.clone(), val.clone());
                }
            }
        }

        palette.push(PaletteEntry { name, properties });
    }

    Ok(palette)
}

fn parse_tile_entities(region: &HashMap<String, NbtValue>) -> Vec<TileEntity> {
    let list = match region.get("TileEntities") {
        Some(NbtValue::List(l)) => l,
        _ => return Vec::new(),
    };

    list.iter()
        .filter_map(|entry| {
            let compound = match entry {
                NbtValue::Compound(c) => c,
                _ => return None,
            };
            let x = get_int(compound, "x").unwrap_or(0);
            let y = get_int(compound, "y").unwrap_or(0);
            let z = get_int(compound, "z").unwrap_or(0);
            let bytes = serialize_nbt_value(entry);
            Some(TileEntity {
                raw_nbt: bytes,
                x,
                y,
                z,
            })
        })
        .collect()
}

fn get_int_triple(compound: &HashMap<String, NbtValue>, key: &str) -> Result<[i32; 3], String> {
    let inner = get_compound(compound, key).ok_or(format!("Missing '{key}'"))?;
    let x = get_int(inner, "x").ok_or(format!("Missing '{key}.x'"))?;
    let y = get_int(inner, "y").ok_or(format!("Missing '{key}.y'"))?;
    let z = get_int(inner, "z").ok_or(format!("Missing '{key}.z'"))?;
    Ok([x, y, z])
}

// Minimal NBT parser — enough for .litematica files.
// Not using pumpkin-nbt because it requires std::io::Seek which
// complicates WASM compatibility with the GzDecoder chain.

#[derive(Debug, Clone)]
pub enum NbtValue {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<i8>),
    String(String),
    List(Vec<NbtValue>),
    Compound(HashMap<String, NbtValue>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

fn parse_nbt(data: &[u8]) -> Result<NbtValue, String> {
    let mut cursor = Cursor::new(data);
    let tag_type = read_u8(&mut cursor)?;
    if tag_type != 10 {
        return Err(format!("Expected compound root, got tag type {tag_type}"));
    }
    let _name = read_string(&mut cursor)?;
    read_compound(&mut cursor)
}

fn read_tag(cursor: &mut Cursor<&[u8]>, tag_type: u8) -> Result<NbtValue, String> {
    match tag_type {
        1 => Ok(NbtValue::Byte(read_i8(cursor)?)),
        2 => Ok(NbtValue::Short(read_i16(cursor)?)),
        3 => Ok(NbtValue::Int(read_i32(cursor)?)),
        4 => Ok(NbtValue::Long(read_i64(cursor)?)),
        5 => Ok(NbtValue::Float(read_f32(cursor)?)),
        6 => Ok(NbtValue::Double(read_f64(cursor)?)),
        7 => {
            let len = read_i32(cursor)? as usize;
            let mut arr = vec![0i8; len];
            for item in &mut arr {
                *item = read_i8(cursor)?;
            }
            Ok(NbtValue::ByteArray(arr))
        }
        8 => Ok(NbtValue::String(read_string(cursor)?)),
        9 => {
            let list_type = read_u8(cursor)?;
            let len = read_i32(cursor)? as usize;
            let mut list = Vec::with_capacity(len);
            for _ in 0..len {
                list.push(read_tag(cursor, list_type)?);
            }
            Ok(NbtValue::List(list))
        }
        10 => read_compound(cursor),
        11 => {
            let len = read_i32(cursor)? as usize;
            let mut arr = Vec::with_capacity(len);
            for _ in 0..len {
                arr.push(read_i32(cursor)?);
            }
            Ok(NbtValue::IntArray(arr))
        }
        12 => {
            let len = read_i32(cursor)? as usize;
            let mut arr = Vec::with_capacity(len);
            for _ in 0..len {
                arr.push(read_i64(cursor)?);
            }
            Ok(NbtValue::LongArray(arr))
        }
        _ => Err(format!("Unknown NBT tag type: {tag_type}")),
    }
}

fn read_compound(cursor: &mut Cursor<&[u8]>) -> Result<NbtValue, String> {
    let mut map = HashMap::new();
    loop {
        let tag_type = read_u8(cursor)?;
        if tag_type == 0 {
            break;
        }
        let name = read_string(cursor)?;
        let value = read_tag(cursor, tag_type)?;
        map.insert(name, value);
    }
    Ok(NbtValue::Compound(map))
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf[0])
}

fn read_i8(cursor: &mut Cursor<&[u8]>) -> Result<i8, String> {
    Ok(read_u8(cursor)? as i8)
}

fn read_i16(cursor: &mut Cursor<&[u8]>) -> Result<i16, String> {
    let mut buf = [0u8; 2];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(i16::from_be_bytes(buf))
}

fn read_i32(cursor: &mut Cursor<&[u8]>) -> Result<i32, String> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(i32::from_be_bytes(buf))
}

fn read_i64(cursor: &mut Cursor<&[u8]>) -> Result<i64, String> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(i64::from_be_bytes(buf))
}

fn read_f32(cursor: &mut Cursor<&[u8]>) -> Result<f32, String> {
    let mut buf = [0u8; 4];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(f32::from_be_bytes(buf))
}

fn read_f64(cursor: &mut Cursor<&[u8]>) -> Result<f64, String> {
    let mut buf = [0u8; 8];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(f64::from_be_bytes(buf))
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> Result<String, String> {
    let len = read_i16(cursor)? as usize;
    let mut buf = vec![0u8; len];
    cursor.read_exact(&mut buf).map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

fn serialize_nbt_value(value: &NbtValue) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(10); // compound root tag
    out.extend_from_slice(&0i16.to_be_bytes()); // empty name
    write_compound_payload(&mut out, value);
    out
}

fn write_compound_payload(out: &mut Vec<u8>, value: &NbtValue) {
    let NbtValue::Compound(map) = value else {
        return;
    };
    for (key, val) in map {
        let tag_id = nbt_tag_id(val);
        out.push(tag_id);
        let key_bytes = key.as_bytes();
        out.extend_from_slice(&(key_bytes.len() as i16).to_be_bytes());
        out.extend_from_slice(key_bytes);
        write_tag_payload(out, val);
    }
    out.push(0); // end tag
}

fn write_tag_payload(out: &mut Vec<u8>, value: &NbtValue) {
    match value {
        NbtValue::Byte(v) => out.push(*v as u8),
        NbtValue::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtValue::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtValue::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtValue::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtValue::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
        NbtValue::ByteArray(arr) => {
            out.extend_from_slice(&(arr.len() as i32).to_be_bytes());
            for b in arr {
                out.push(*b as u8);
            }
        }
        NbtValue::String(s) => {
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as i16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        NbtValue::List(list) => {
            let elem_type = list.first().map(nbt_tag_id).unwrap_or(0);
            out.push(elem_type);
            out.extend_from_slice(&(list.len() as i32).to_be_bytes());
            for item in list {
                write_tag_payload(out, item);
            }
        }
        NbtValue::Compound(_) => write_compound_payload(out, value),
        NbtValue::IntArray(arr) => {
            out.extend_from_slice(&(arr.len() as i32).to_be_bytes());
            for v in arr {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        NbtValue::LongArray(arr) => {
            out.extend_from_slice(&(arr.len() as i32).to_be_bytes());
            for v in arr {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
}

fn nbt_tag_id(value: &NbtValue) -> u8 {
    match value {
        NbtValue::Byte(_) => 1,
        NbtValue::Short(_) => 2,
        NbtValue::Int(_) => 3,
        NbtValue::Long(_) => 4,
        NbtValue::Float(_) => 5,
        NbtValue::Double(_) => 6,
        NbtValue::ByteArray(_) => 7,
        NbtValue::String(_) => 8,
        NbtValue::List(_) => 9,
        NbtValue::Compound(_) => 10,
        NbtValue::IntArray(_) => 11,
        NbtValue::LongArray(_) => 12,
    }
}

fn get_compound<'a>(
    map: &'a HashMap<String, NbtValue>,
    key: &str,
) -> Option<&'a HashMap<String, NbtValue>> {
    match map.get(key) {
        Some(NbtValue::Compound(c)) => Some(c),
        _ => None,
    }
}

fn get_string(map: &HashMap<String, NbtValue>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(NbtValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn get_int(map: &HashMap<String, NbtValue>, key: &str) -> Option<i32> {
    match map.get(key) {
        Some(NbtValue::Int(v)) => Some(*v),
        _ => None,
    }
}

fn get_long_array(map: &HashMap<String, NbtValue>, key: &str) -> Result<Vec<i64>, String> {
    match map.get(key) {
        Some(NbtValue::LongArray(arr)) => Ok(arr.clone()),
        _ => Err(format!("Missing or invalid '{key}' (expected LongArray)")),
    }
}

fn get_short(map: &HashMap<String, NbtValue>, key: &str) -> Option<i16> {
    match map.get(key) {
        Some(NbtValue::Short(v)) => Some(*v),
        _ => None,
    }
}

fn get_byte_array(map: &HashMap<String, NbtValue>, key: &str) -> Option<Vec<i8>> {
    match map.get(key) {
        Some(NbtValue::ByteArray(arr)) => Some(arr.clone()),
        _ => None,
    }
}

pub fn parse_schematic(data: &[u8], filename: &str) -> Result<Schematic, String> {
    if filename.ends_with(".schem") {
        parse_schem(data, filename)
    } else {
        parse_litematica(data)
    }
}

pub fn parse_schem(data: &[u8], filename: &str) -> Result<Schematic, String> {
    let mut decoder = GzDecoder::new(Cursor::new(data));
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("GZIP decode failed: {e}"))?;

    let nbt = parse_nbt(&decompressed)?;

    let root = match &nbt {
        NbtValue::Compound(map) => map,
        _ => return Err("Root is not a compound".into()),
    };

    let schem_data = if let Some(NbtValue::Compound(s)) = root.get("Schematic") {
        s
    } else {
        root
    };

    let width = get_short(schem_data, "Width").ok_or("Missing Width")? as i32;
    let height = get_short(schem_data, "Height").ok_or("Missing Height")? as i32;
    let length = get_short(schem_data, "Length").ok_or("Missing Length")? as i32;

    let (palette_compound, block_data_bytes) =
        if let Some(NbtValue::Compound(blocks)) = schem_data.get("Blocks") {
            let p = get_compound(blocks, "Palette").ok_or("Missing Blocks.Palette")?;
            let d = get_byte_array(blocks, "Data").ok_or("Missing Blocks.Data")?;
            (p, d)
        } else {
            let p = get_compound(schem_data, "Palette").ok_or("Missing Palette")?;
            let d = get_byte_array(schem_data, "BlockData").ok_or("Missing BlockData")?;
            (p, d)
        };

    let mut palette_entries: Vec<(i32, PaletteEntry)> = Vec::new();
    for (block_str, idx_val) in palette_compound {
        let idx = match idx_val {
            NbtValue::Int(i) => *i,
            _ => continue,
        };

        let (name, properties) = parse_block_state_string(block_str);
        palette_entries.push((idx, PaletteEntry { name, properties }));
    }

    palette_entries.sort_by_key(|(idx, _)| *idx);
    let max_idx = palette_entries.last().map(|(i, _)| *i).unwrap_or(0) as usize;
    let mut palette = vec![
        PaletteEntry {
            name: "minecraft:air".into(),
            properties: HashMap::new(),
        };
        max_idx + 1
    ];
    for (idx, entry) in palette_entries {
        palette[idx as usize] = entry;
    }

    let total_blocks = (width * height * length) as usize;
    let block_indices = decode_varint_array(&block_data_bytes, total_blocks)?;

    let bits = std::cmp::max(
        2,
        32 - (palette.len() as u32).saturating_sub(1).leading_zeros(),
    );
    let block_data = pack_indices_to_longs(&block_indices, bits);

    let tile_entities = if let Some(NbtValue::Compound(blocks)) = schem_data.get("Blocks") {
        parse_schem_block_entities(blocks)
    } else {
        parse_schem_block_entities(schem_data)
    };

    let name = filename.trim_end_matches(".schem").to_string();

    Ok(Schematic {
        name,
        regions: vec![Region {
            name: "main".into(),
            position: [0, 0, 0],
            size: [width, height, length],
            palette,
            block_data,
            tile_entities,
        }],
    })
}

fn parse_block_state_string(s: &str) -> (String, HashMap<String, String>) {
    let mut properties = HashMap::new();
    if let Some(bracket) = s.find('[') {
        let name = s[..bracket].to_string();
        let props_str = &s[bracket + 1..s.len().saturating_sub(1)];
        for pair in props_str.split(',') {
            let pair = pair.trim();
            if let Some(eq) = pair.find('=') {
                let key = pair[..eq].trim().to_string();
                let val = pair[eq + 1..].trim().to_string();
                properties.insert(key, val);
            }
        }
        (name, properties)
    } else {
        (s.to_string(), properties)
    }
}

fn decode_varint_array(bytes: &[i8], expected_len: usize) -> Result<Vec<u16>, String> {
    let mut result = Vec::with_capacity(expected_len);
    let mut i = 0;
    let data: Vec<u8> = bytes.iter().map(|b| *b as u8).collect();

    while i < data.len() && result.len() < expected_len {
        let mut value: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            if i >= data.len() {
                return Err("Unexpected end of varint data".into());
            }
            let byte = data[i];
            i += 1;
            value |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return Err("Varint too large".into());
            }
        }
        result.push(value as u16);
    }

    Ok(result)
}

fn pack_indices_to_longs(indices: &[u16], bits: u32) -> Vec<i64> {
    let entries_per_long = 64 / bits;
    let num_longs = (indices.len() as u64).div_ceil(entries_per_long as u64);
    let mut longs = vec![0i64; num_longs as usize];

    for (i, &idx) in indices.iter().enumerate() {
        let start_offset = (i as u64) * (bits as u64);
        let arr_index = (start_offset >> 6) as usize;
        let bit_offset = (start_offset & 0x3F) as u32;

        longs[arr_index] |= (idx as i64) << bit_offset;

        if bit_offset + bits > 64 && arr_index + 1 < longs.len() {
            longs[arr_index + 1] |= (idx as i64) >> (64 - bit_offset);
        }
    }

    longs
}

fn parse_schem_block_entities(schem_data: &HashMap<String, NbtValue>) -> Vec<TileEntity> {
    let key = if schem_data.contains_key("BlockEntities") {
        "BlockEntities"
    } else {
        "TileEntities"
    };

    let list = match schem_data.get(key) {
        Some(NbtValue::List(l)) => l,
        _ => return Vec::new(),
    };

    list.iter()
        .filter_map(|entry| {
            let compound = match entry {
                NbtValue::Compound(c) => c,
                _ => return None,
            };

            let pos = match compound.get("Pos") {
                Some(NbtValue::IntArray(arr)) if arr.len() >= 3 => (arr[0], arr[1], arr[2]),
                _ => {
                    let x = get_int(compound, "x").unwrap_or(0);
                    let y = get_int(compound, "y").unwrap_or(0);
                    let z = get_int(compound, "z").unwrap_or(0);
                    (x, y, z)
                }
            };

            let bytes = serialize_nbt_value(entry);
            Some(TileEntity {
                raw_nbt: bytes,
                x: pos.0,
                y: pos.1,
                z: pos.2,
            })
        })
        .collect()
}
