//! Fragmented-MP4 (CMAF) fixups for segments that get joined into one file.

fn u32_at(d: &[u8], at: usize) -> u64 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]).into()
}

/// (type, header length, total size) of the box at `at`, or `None` if it is malformed or overruns `end`.
fn box_at(d: &[u8], at: usize, end: usize) -> Option<([u8; 4], usize, usize)> {
    if end.checked_sub(at)? < 8 {
        return None;
    }
    let mut size = u32_at(d, at);
    let mut header = 8;
    if size == 1 {
        if end - at < 16 {
            return None;
        }
        size = u64::from_be_bytes(d[at + 8..at + 16].try_into().ok()?);
        header = 16;
    } else if size == 0 {
        size = (end - at) as u64;
    }
    // Sizes come from the server: compare before converting so a huge value cannot overflow.
    if size < header as u64 || size > (end - at) as u64 {
        return None;
    }
    Some((d[at + 4..at + 8].try_into().ok()?, header, size as usize))
}

/// Rewrites each top-level `moof`'s `mfhd` sequence number to `next`, `next + 1`, ...
/// Servers that render segments independently (Tella) number every one 1; some players then read
/// only the first fragment of the joined file and the video comes out a few seconds long.
/// Malformed boxes are left alone.
pub fn renumber_fragments(data: &mut [u8], next: &mut u32) {
    let end = data.len();
    let mut off = 0;
    while let Some((kind, header, size)) = box_at(data, off, end) {
        if &kind == b"moof" {
            let mut c = off + header;
            while let Some((child, ch, cs)) = box_at(data, c, off + size) {
                // mfhd: header, 1-byte version, 3-byte flags, 4-byte sequence_number.
                if &child == b"mfhd" && cs >= ch + 8 {
                    let at = c + ch + 4;
                    data[at..at + 4].copy_from_slice(&next.to_be_bytes());
                    *next = next.wrapping_add(1);
                    break;
                }
                c += cs;
            }
        }
        off += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn segment() -> Vec<u8> {
        let mfhd = bx(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        let traf = bx(b"traf", &[9; 12]);
        let moof = bx(b"moof", &[mfhd, traf].concat());
        [moof, bx(b"mdat", &[7; 20])].concat()
    }

    fn seq(d: &[u8]) -> u32 {
        u32::from_be_bytes(d[20..24].try_into().unwrap())
    }

    #[test]
    fn renumbers_fragments_across_segments() {
        let mut next = 1;
        let mut out = Vec::new();
        for _ in 0..3 {
            let mut s = segment();
            renumber_fragments(&mut s, &mut next);
            out.push(s);
        }
        assert_eq!(out.iter().map(|s| seq(s)).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(next, 4);
        let mut expected = segment();
        expected[20..24].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(out[2], expected, "nothing else changes");
    }

    #[test]
    fn leaves_malformed_data_alone() {
        let mut next = 5;
        let mut junk = vec![0xff, 0xff, 0xff, 0xff, b'm', b'o', b'o', b'f', 1, 2, 3];
        let before = junk.clone();
        renumber_fragments(&mut junk, &mut next);
        assert_eq!(junk, before);
        assert_eq!(next, 5);
        let mut tiny = vec![0, 0];
        renumber_fragments(&mut tiny, &mut next);
        // 64-bit size larger than the data.
        let mut big = [&[0, 0, 0, 1][..], b"moof", &u64::MAX.to_be_bytes()].concat();
        renumber_fragments(&mut big, &mut next);
        assert_eq!(next, 5);
    }
}
