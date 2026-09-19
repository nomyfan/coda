use std::collections::VecDeque;

/// Keeps exact bytes while small, then a stable head and rolling tail.
#[derive(Debug)]
pub struct Preview {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    capacity: usize,
    captured: u64,
}

impl Preview {
    pub fn new(capacity: usize) -> Self {
        Self {
            head: Vec::with_capacity(capacity.div_ceil(2)),
            tail: VecDeque::with_capacity(capacity / 2),
            capacity,
            captured: 0,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.captured = self.captured.saturating_add(bytes.len() as u64);
        let head_bytes = bytes.len().min(self.capacity.div_ceil(2) - self.head.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        let bytes = &bytes[head_bytes..];
        let tail_capacity = self.capacity / 2;
        if bytes.len() >= tail_capacity {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - tail_capacity..]);
        } else {
            let remove = (self.tail.len() + bytes.len()).saturating_sub(tail_capacity);
            self.tail.drain(..remove);
            self.tail.extend(bytes);
        }
    }

    pub fn captured_bytes(&self) -> u64 {
        self.captured
    }

    pub fn retained_bytes(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    pub fn complete(&self) -> bool {
        self.captured == self.retained_bytes() as u64
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.head
            .iter()
            .copied()
            .chain(self.tail.iter().copied())
            .collect()
    }

    pub fn text(&self) -> String {
        if self.complete() {
            return String::from_utf8_lossy(&self.bytes()).into_owned();
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let head = trim_incomplete_end(&self.head);
        // At most three continuation bytes can belong to a character that
        // starts before the retained tail. Interior invalid data remains lossy.
        let tail_start = tail
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count();
        let tail = &tail[tail_start..];
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            String::from_utf8_lossy(head),
            self.captured - (head.len() + tail.len()) as u64,
            String::from_utf8_lossy(tail)
        )
    }
}

fn trim_incomplete_end(bytes: &[u8]) -> &[u8] {
    let last_character = bytes.len().saturating_sub(4);
    let Some(start) = bytes[last_character..]
        .iter()
        .rposition(|byte| byte & 0xc0 != 0x80)
    else {
        return bytes;
    };
    let start = last_character + start;
    match std::str::from_utf8(&bytes[start..]) {
        Err(error) if error.error_len().is_none() => &bytes[..start],
        _ => bytes,
    }
}

/// Keep a trailing partial scalar for the next raw-byte page, even after invalid bytes.
pub fn page_boundary(bytes: &[u8]) -> usize {
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(error) => {
                offset += error.valid_up_to();
                if let Some(invalid) = error.error_len() {
                    offset += invalid;
                } else {
                    return offset;
                }
            }
        }
    }
    offset
}

/// Decodes lossily until the text would exceed `limit` bytes, returning the
/// text and how many raw bytes it covers, so a replaced invalid byte can never
/// push a page past its budget or skip unread input.
pub fn decode_within(bytes: &[u8], limit: usize) -> (String, usize) {
    let mut text = String::new();
    let mut used = 0;
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            if text.len() + character.len_utf8() > limit {
                return (text, used);
            }
            text.push(character);
            used += character.len_utf8();
        }
        if !chunk.invalid().is_empty() {
            if text.len() + char::REPLACEMENT_CHARACTER.len_utf8() > limit {
                return (text, used);
            }
            text.push(char::REPLACEMENT_CHARACTER);
            used += chunk.invalid().len();
        }
    }
    (text, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_chunks_keep_the_original_head_and_latest_tail() {
        let mut preview = Preview::new(8);
        preview.append(b"abcdef");
        assert_eq!(preview.bytes(), b"abcdef");
        assert!(preview.complete());
        preview.append(b"ghijklmnopqrstuvwxyz");
        assert_eq!(preview.bytes(), b"abcdwxyz");
        assert_eq!(preview.captured_bytes(), 26);
        assert_eq!(preview.retained_bytes(), 8);
        assert!(!preview.complete());
    }

    #[test]
    fn truncated_chinese_drops_partial_characters_and_counts_their_bytes() {
        let mut preview = Preview::new(8);
        preview.append("你好吗世界".as_bytes());
        let retained = preview.bytes();
        assert_eq!(preview.text(), "你\n[... 9 bytes omitted ...]\n界");
        assert_eq!(
            preview.bytes(),
            retained,
            "rendering must not change captured bytes"
        );
        assert_eq!(preview.retained_bytes(), 8);
    }

    #[test]
    fn truncated_emoji_and_cross_block_characters_render_without_replacements() {
        let mut preview = Preview::new(6);
        for bytes in "a🙂b🚀c".as_bytes().chunks(2) {
            preview.append(bytes);
        }
        assert_eq!(preview.text(), "a\n[... 9 bytes omitted ...]\nc");
    }

    #[test]
    fn utf8_rendering_matches_original_substrings_across_capacities_and_chunks() {
        for original in ["", "你好", "🙂🚀🌍", "a你🙂好e\u{301}世界z"] {
            for capacity in 0..=original.len() + 1 {
                let expected = if capacity >= original.len() {
                    original.to_string()
                } else {
                    let boundaries: Vec<_> = original
                        .char_indices()
                        .map(|(index, _)| index)
                        .chain(std::iter::once(original.len()))
                        .collect();
                    let head_end = *boundaries
                        .iter()
                        .rev()
                        .find(|index| **index <= capacity.div_ceil(2))
                        .unwrap();
                    let tail_start = *boundaries
                        .iter()
                        .find(|index| **index >= original.len() - capacity / 2)
                        .unwrap();
                    format!(
                        "{}\n[... {} bytes omitted ...]\n{}",
                        &original[..head_end],
                        tail_start - head_end,
                        &original[tail_start..]
                    )
                };
                for chunk_size in 1..=original.len().max(1) {
                    let mut preview = Preview::new(capacity);
                    for chunk in original.as_bytes().chunks(chunk_size) {
                        preview.append(chunk);
                    }
                    assert_eq!(
                        preview.text(),
                        expected,
                        "input={original:?}, capacity={capacity}, chunk={chunk_size}"
                    );
                }
            }
        }
    }

    #[test]
    fn genuine_invalid_bytes_inside_retained_parts_still_use_lossy_decoding() {
        let mut preview = Preview::new(8);
        preview.append(b"a\xffbc01234567x\xffyz");
        assert_eq!(preview.text(), "a�bc\n[... 8 bytes omitted ...]\nx�yz");
        let mut complete = Preview::new(8);
        complete.append(b"a\xffb");
        assert_eq!(complete.text(), "a�b");
    }
}
