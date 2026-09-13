// AnthroForge Part Pack ("AFPP") builder.
//
// Native-only, self-contained binary. Deliberately does NOT depend on
// anthroforge_core's library code (lib.rs, gltf_loader.rs, obj_loader.rs):
// it re-implements the small amount of directory-walking / numeric-prefix
// part-id logic it needs directly, so it has zero risk of colliding with
// any other change happening to `lib.rs` at the same time, and can be
// built/run with a plain `cargo build`/`cargo run` (no wasm32 target).
//
// Output format (fixed, see TASK-PART-B-pack-authoring-tool.md):
//
//   offset 0   : magic       b"AFPP"           (4 bytes, ASCII, no NUL)
//   offset 4   : version     u32 LE = 1
//   offset 8   : part_count  u32 LE (N, N >= 1)
//   offset 12  : skel_len    u32 LE
//   offset 16  : skeleton    skel_len raw bytes (master_skeleton.json,
//                            verbatim, UTF-8)
//   then N fixed-size 13-byte index entries, back-to-back, no padding:
//                part_id     u32 LE
//                src_type    u8   (0 = OBJ, 1 = glTF/GLB)
//                data_offset u32 LE (absolute offset from start of buffer)
//                data_len    u32 LE
//   then the N parts' raw source bytes.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: pack_builder <input_asset_dir> <output_pack_file>");
        std::process::exit(1);
    }

    let input_dir = PathBuf::from(&args[1]);
    let output_path = PathBuf::from(&args[2]);

    match run(&input_dir, &output_path) {
        Ok((part_count, total_bytes)) => {
            println!(
                "wrote {part_count} part(s), {total_bytes} bytes, to '{}'",
                output_path.display()
            );
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// One accepted input part file, before its bytes are read.
struct PendingPart {
    part_id: u32,
    src_type: u8, // 0 = OBJ, 1 = glTF/GLB
    path: PathBuf,
}

fn run(input_dir: &Path, output_path: &Path) -> Result<(usize, usize), String> {
    if !input_dir.is_dir() {
        return Err(format!(
            "input asset dir '{}' does not exist or is not a directory",
            input_dir.display()
        ));
    }

    // master_skeleton.json must exist directly inside the input directory.
    let skeleton_path = input_dir.join("master_skeleton.json");
    let skeleton_bytes = fs::read(&skeleton_path).map_err(|e| {
        format!(
            "expected master_skeleton.json at '{}': {e}",
            skeleton_path.display()
        )
    })?;
    let skel_len = u32::try_from(skeleton_bytes.len()).map_err(|_| {
        format!(
            "master_skeleton.json at '{}' is too large to represent in this format's u32 length field ({} bytes)",
            skeleton_path.display(),
            skeleton_bytes.len()
        )
    })?;

    // Scan the directory for part files.
    let entries = fs::read_dir(input_dir)
        .map_err(|e| format!("failed to read input asset dir '{}': {e}", input_dir.display()))?;

    let mut pending: Vec<PendingPart> = Vec::new();
    // Tracks which file first claimed each part id, so a collision can name
    // both files.
    let mut seen_ids: HashMap<u32, PathBuf> = HashMap::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read a directory entry: {e}"))?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());

        let src_type: u8 = match ext.as_deref() {
            Some("obj") => 0,
            Some("glb") => 1,
            Some("gltf") => {
                eprintln!(
                    "warning: skipping '{}': .gltf not supported by this tool, convert to .glb first",
                    path.display()
                );
                continue;
            }
            Some("json") if path.file_name().and_then(|n| n.to_str()) == Some("master_skeleton.json") => {
                // The skeleton file itself; not a part.
                continue;
            }
            _ => {
                return Err(format!(
                    "unrecognized file extension for '{}' (only .obj and .glb part files, plus master_skeleton.json, are accepted)",
                    path.display()
                ));
            }
        };

        let part_id = match parse_part_id(&path) {
            Some(id) => id,
            None => {
                eprintln!(
                    "warning: skipping '{}': filename does not start with a numeric part id",
                    path.display()
                );
                continue;
            }
        };

        if let Some(prior_path) = seen_ids.get(&part_id) {
            return Err(format!(
                "duplicate part id {part_id}: both '{}' and '{}' resolve to this id",
                prior_path.display(),
                path.display()
            ));
        }
        seen_ids.insert(part_id, path.clone());

        pending.push(PendingPart {
            part_id,
            src_type,
            path,
        });
    }

    if pending.is_empty() {
        return Err(format!(
            "no valid .obj/.glb part files were found in '{}'",
            input_dir.display()
        ));
    }

    let part_count = pending.len();
    let part_count_u32 =
        u32::try_from(part_count).map_err(|_| format!("too many parts ({part_count}) to represent in this format's u32 part_count field"))?;

    // Read every part's raw bytes up front, so we know each one's length
    // before laying out the index table's data_offset/data_len fields.
    let mut part_bytes: Vec<Vec<u8>> = Vec::with_capacity(part_count);
    for p in &pending {
        let bytes = fs::read(&p.path)
            .map_err(|e| format!("failed to read part file '{}': {e}", p.path.display()))?;
        part_bytes.push(bytes);
    }

    // Header: magic(4) + version(4) + part_count(4) + skel_len(4) = 16
    // Index table: part_count * 13 bytes.
    let header_len: usize = 16;
    let index_table_len: usize = part_count * 13;
    let skeleton_region_start: usize = header_len;
    let data_region_start: usize = header_len + skeleton_bytes.len() + index_table_len;

    // Compute each part's absolute data_offset/data_len, laid out
    // contiguously immediately after the index table.
    let mut index_entries: Vec<(u32, u8, u32, u32)> = Vec::with_capacity(part_count);
    let mut running_offset: usize = data_region_start;
    for (p, bytes) in pending.iter().zip(part_bytes.iter()) {
        let data_offset = u32::try_from(running_offset).map_err(|_| {
            format!(
                "output pack would exceed this format's u32 offset range while placing part id {}",
                p.part_id
            )
        })?;
        let data_len = u32::try_from(bytes.len()).map_err(|_| {
            format!(
                "part file '{}' (part id {}) is too large to represent in this format's u32 length field ({} bytes)",
                p.path.display(),
                p.part_id,
                bytes.len()
            )
        })?;
        index_entries.push((p.part_id, p.src_type, data_offset, data_len));
        running_offset += bytes.len();
    }

    let total_len = running_offset;
    let mut out: Vec<u8> = Vec::with_capacity(total_len);

    // Header.
    out.extend_from_slice(b"AFPP");
    out.extend_from_slice(&1u32.to_le_bytes()); // version
    out.extend_from_slice(&part_count_u32.to_le_bytes());
    out.extend_from_slice(&skel_len.to_le_bytes());

    // Skeleton bytes.
    out.extend_from_slice(&skeleton_bytes);
    debug_assert_eq!(out.len(), skeleton_region_start + skeleton_bytes.len());

    // Index table.
    for (part_id, src_type, data_offset, data_len) in &index_entries {
        out.extend_from_slice(&part_id.to_le_bytes());
        out.push(*src_type);
        out.extend_from_slice(&data_offset.to_le_bytes());
        out.extend_from_slice(&data_len.to_le_bytes());
    }
    debug_assert_eq!(out.len(), data_region_start);

    // Part data, in the same order as the index table.
    for bytes in &part_bytes {
        out.extend_from_slice(bytes);
    }
    debug_assert_eq!(out.len(), total_len);

    fs::write(output_path, &out)
        .map_err(|e| format!("failed to write output pack '{}': {e}", output_path.display()))?;

    Ok((part_count, total_len))
}

/// Part ids are taken from the leading run of ASCII digits in the file
/// stem, e.g. `1001_head_male.glb` -> `1001`. Returns `None` (skip, don't
/// error the whole run out) if the filename doesn't start with a digit.
fn parse_part_id(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

// ============================================================================
// Structural self-check: writes a tiny fixture pack in-memory (via `run`,
// against a real tempdir on disk) and reads it back byte-by-byte, verifying
// magic/version/part_count and that every index entry's
// data_offset + data_len stays within bounds. Does not parse OBJ/glTF
// content itself — this tool only needs to embed that content correctly,
// never to understand it.
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_tmp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "pack_builder_test_{name}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn structural_round_trip() {
        let dir = unique_tmp_dir("roundtrip");
        fs::write(
            dir.join("master_skeleton.json"),
            br#"{"bones":{"root":0}}"#,
        )
        .unwrap();
        fs::write(dir.join("1001_head.obj"), b"v 0 0 0\n").unwrap();
        fs::write(dir.join("1002_torso.glb"), b"glTF\x02\x00\x00\x00fakebinarydata").unwrap();

        let out_path = dir.join("out.afpp");
        let (part_count, total_len) = run(&dir, &out_path).expect("run should succeed");
        assert_eq!(part_count, 2);

        let buf = fs::read(&out_path).unwrap();
        assert_eq!(buf.len(), total_len);

        assert_eq!(&buf[0..4], b"AFPP");
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(version, 1);
        let part_count_field = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(part_count_field as usize, part_count);
        let skel_len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;

        let index_start = 16 + skel_len;
        for i in 0..part_count {
            let entry_start = index_start + i * 13;
            let entry = &buf[entry_start..entry_start + 13];
            let data_offset = u32::from_le_bytes(entry[5..9].try_into().unwrap()) as usize;
            let data_len = u32::from_le_bytes(entry[9..13].try_into().unwrap()) as usize;
            assert!(data_offset + data_len <= buf.len(), "index entry {i} reads out of bounds");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_part_id_errors() {
        let dir = unique_tmp_dir("dupe");
        fs::write(dir.join("master_skeleton.json"), br#"{"bones":{"root":0}}"#).unwrap();
        fs::write(dir.join("1001_head.obj"), b"v 0 0 0\n").unwrap();
        fs::write(dir.join("1001_head_alt.glb"), b"glTF\x02\x00\x00\x00x").unwrap();

        let out_path = dir.join("out.afpp");
        let result = run(&dir, &out_path);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gltf_file_is_skipped_with_warning_and_run_still_succeeds() {
        let dir = unique_tmp_dir("gltfskip");
        fs::write(dir.join("master_skeleton.json"), br#"{"bones":{"root":0}}"#).unwrap();
        fs::write(dir.join("1001_head.obj"), b"v 0 0 0\n").unwrap();
        fs::write(dir.join("2002_legacy.gltf"), b"{\"not\":\"embedded\"}").unwrap();

        let out_path = dir.join("out.afpp");
        let result = run(&dir, &out_path);
        assert!(result.is_ok());
        let (part_count, _) = result.unwrap();
        // Only the .obj part should have been accepted; the .gltf is skipped.
        assert_eq!(part_count, 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
