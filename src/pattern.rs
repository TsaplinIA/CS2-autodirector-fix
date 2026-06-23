#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PatternSearch {
    pub(crate) matches: usize,
    pub(crate) unique_offset: Option<usize>,
}

pub(crate) fn find_pattern(data: &[u8], pattern: &[Option<u8>]) -> PatternSearch {
    if data.len() < pattern.len() {
        return PatternSearch {
            matches: 0,
            unique_offset: None,
        };
    }

    let mut matches = 0;
    let mut unique_offset = None;

    for (offset, window) in data.windows(pattern.len()).enumerate() {
        if !bytes_match(window, pattern) {
            continue;
        }

        matches += 1;
        unique_offset = if matches == 1 { Some(offset) } else { None };
    }

    PatternSearch {
        matches,
        unique_offset,
    }
}

fn bytes_match(data: &[u8], pattern: &[Option<u8>]) -> bool {
    if data.len() != pattern.len() {
        return false;
    }

    for (offset, expected) in pattern.iter().enumerate() {
        if let Some(byte) = expected
            && data[offset] != *byte
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{PatternSearch, bytes_match, find_pattern};

    #[test]
    fn matches_wildcard_pattern() {
        let pattern = [Some(0xe8), None, None, Some(0x48)];

        assert!(bytes_match(&[0xe8, 0x11, 0x22, 0x48], &pattern));
        assert!(!bytes_match(&[0xe8, 0x11, 0x22, 0x49], &pattern));
    }

    #[test]
    fn finds_unique_pattern_match() {
        let data = [0x90, 0xe8, 0x01, 0x48, 0x90];
        let pattern = [Some(0xe8), None, Some(0x48)];

        assert_eq!(
            find_pattern(&data, &pattern),
            PatternSearch {
                matches: 1,
                unique_offset: Some(1),
            }
        );
    }

    #[test]
    fn rejects_ambiguous_pattern_matches() {
        let data = [0x90, 0xe8, 0x01, 0x48, 0xe8, 0x02, 0x48, 0x90];
        let pattern = [Some(0xe8), None, Some(0x48)];

        assert_eq!(
            find_pattern(&data, &pattern),
            PatternSearch {
                matches: 2,
                unique_offset: None,
            }
        );
    }
}
