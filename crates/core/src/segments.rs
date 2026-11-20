use serde::{Deserialize, Serialize};

/// A contiguous inclusive byte range of the target file and how much of it is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub written: u64,
}

impl Segment {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end, written: 0 }
    }

    pub fn next_offset(&self) -> u64 {
        self.start + self.written
    }

    pub fn is_done(&self) -> bool {
        self.next_offset() > self.end
    }
}

pub const MIN_SEGMENT: u64 = 256 * 1024;

/// Splits `length` bytes into at most `count` segments, none smaller than `min_size`
/// (except when the whole file is smaller than that).
pub fn plan(length: u64, count: usize, min_size: u64) -> Vec<Segment> {
    if length == 0 {
        return Vec::new();
    }
    let by_min_size = (length / min_size.max(1)).max(1);
    let n = (count as u64).min(by_min_size).max(1);
    let chunk = length.div_ceil(n);
    let mut out = Vec::with_capacity(n as usize);
    let mut start = 0;
    while start < length {
        let end = (start + chunk - 1).min(length - 1);
        out.push(Segment::new(start, end));
        start = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_evenly_and_covers_everything() {
        let s = plan(10 * 1024 * 1024, 8, MIN_SEGMENT);
        assert_eq!(s.len(), 8);
        assert_eq!(s[0].start, 0);
        assert_eq!(s.last().unwrap().end, 10 * 1024 * 1024 - 1);
        for w in s.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start);
        }
    }

    #[test]
    fn small_files_get_fewer_segments() {
        assert_eq!(plan(300 * 1024, 8, MIN_SEGMENT).len(), 1);
        assert_eq!(plan(600 * 1024, 8, MIN_SEGMENT).len(), 2);
        assert_eq!(plan(10, 8, MIN_SEGMENT), vec![Segment::new(0, 9)]);
        assert!(plan(0, 8, MIN_SEGMENT).is_empty());
    }

    #[test]
    fn done_after_last_byte() {
        let mut s = Segment::new(10, 19);
        assert!(!s.is_done());
        s.written = 10;
        assert!(s.is_done());
        assert_eq!(s.next_offset(), 20);
    }
}
