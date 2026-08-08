use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn len(self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeSet {
    ranges: Vec<ByteRange>,
}

impl RangeSet {
    pub fn ranges(&self) -> &[ByteRange] {
        &self.ranges
    }

    pub fn uncovered(&self, requested: ByteRange) -> Vec<ByteRange> {
        let mut cursor = requested.start;
        let mut result = Vec::new();
        for existing in &self.ranges {
            if existing.end <= cursor {
                continue;
            }
            if existing.start >= requested.end {
                break;
            }
            if existing.start > cursor {
                result.push(ByteRange {
                    start: cursor,
                    end: existing.start.min(requested.end),
                });
            }
            cursor = cursor.max(existing.end);
            if cursor >= requested.end {
                break;
            }
        }
        if cursor < requested.end {
            result.push(ByteRange {
                start: cursor,
                end: requested.end,
            });
        }
        result
    }

    pub fn insert(&mut self, range: ByteRange) {
        let mut merged = range;
        let mut output = Vec::with_capacity(self.ranges.len() + 1);
        let mut inserted = false;
        for existing in self.ranges.drain(..) {
            if existing.end < merged.start {
                output.push(existing);
            } else if merged.end < existing.start {
                if !inserted {
                    output.push(merged);
                    inserted = true;
                }
                output.push(existing);
            } else {
                merged.start = merged.start.min(existing.start);
                merged.end = merged.end.max(existing.end);
            }
        }
        if !inserted {
            output.push(merged);
        }
        self.ranges = output;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncovered_and_insert_coalesce() {
        let mut set = RangeSet::default();
        set.insert(ByteRange::new(4, 8).unwrap());
        set.insert(ByteRange::new(12, 16).unwrap());
        assert_eq!(
            set.uncovered(ByteRange::new(0, 20).unwrap()),
            vec![
                ByteRange { start: 0, end: 4 },
                ByteRange { start: 8, end: 12 },
                ByteRange { start: 16, end: 20 },
            ]
        );
        set.insert(ByteRange::new(8, 12).unwrap());
        set.insert(ByteRange::new(0, 4).unwrap());
        assert_eq!(set.ranges(), &[ByteRange { start: 0, end: 16 }]);
    }
}
