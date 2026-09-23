//! Line-aware previews of captured output.
//!
//! A preview keeps whole lines from the start and the end of a stream. Once
//! rendered, it names the lines it leaves out and, for a line too long to show
//! whole, how many of its bytes are left out. Each gap also gives the byte
//! offset where it starts. Lines split on `\n` and offsets count raw bytes,
//! exactly as in the saved file, so a reader can fetch the missing part by
//! line (`read_file`) or by offset (`dd`, `tail -c`).

use std::collections::VecDeque;

use super::Channel;

/// The most of one line a preview or a `read_file` page shows.
pub const MAX_LINE_BYTES: usize = 2000;

#[derive(Clone, Debug, Default)]
struct Line {
    /// 1-based position in the stream.
    number: u64,
    /// Offset of the line's first byte in the stream.
    start: u64,
    /// Length in bytes, without the newline.
    len: u64,
    /// The start of the line that was kept.
    kept: Vec<u8>,
}

/// Memory charged for each retained line beyond its text: the record, the
/// spare and transient room its container takes while growing, and the text's
/// allocation header.
const LINE_OVERHEAD: usize = 3 * std::mem::size_of::<Line>() + 16;

impl Line {
    /// Memory the retained line is charged.
    fn cost(&self) -> usize {
        self.kept.len() + LINE_OVERHEAD
    }

    /// Offset just past the line's newline.
    fn end(&self) -> u64 {
        self.start + self.len + 1
    }

    /// The line whole when it fits `cap`; otherwise its start and a note
    /// giving the bytes left out and the offset they start at.
    fn render(&self, cap: usize) -> String {
        if self.len == self.kept.len() as u64 && self.kept.len() <= cap {
            let kept = self.kept.strip_suffix(b"\r").unwrap_or(&self.kept);
            return String::from_utf8_lossy(kept).into_owned();
        }
        let raw = &self.kept[..self.kept.len().min(cap)];
        let (text, used) = decode_within(&raw[..page_boundary(raw)], cap);
        let note = format!(
            "[line {} truncated: {} more bytes at offset {}]",
            self.number,
            self.len - used as u64,
            self.start + used as u64
        );
        if text.is_empty() {
            note
        } else {
            format!("{text} {note}")
        }
    }
}

/// The first and last lines of one stream, in at most `capacity` bytes of
/// memory.
#[derive(Clone, Debug)]
pub struct Preview {
    /// Most of one line kept; the line still arriving is buffered this far.
    line_cap: usize,
    /// Memory for lines at the start.
    head_cap: usize,
    /// Memory for lines at the end.
    tail_cap: usize,
    head: Vec<Line>,
    head_bytes: usize,
    /// A line has missed the head, so every later line goes to the tail.
    head_full: bool,
    tail: VecDeque<Line>,
    tail_bytes: usize,
    /// The line still arriving, its buffer reused for every line.
    open: Line,
    captured: u64,
}

impl Preview {
    pub fn new(capacity: usize) -> Self {
        let line_cap = MAX_LINE_BYTES.min(capacity / 8);
        let sides = capacity - line_cap;
        Self {
            line_cap,
            head_cap: sides.div_ceil(2),
            tail_cap: sides / 2,
            head: Vec::new(),
            head_bytes: 0,
            head_full: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            open: Line {
                number: 1,
                kept: Vec::with_capacity(line_cap),
                ..Line::default()
            },
            captured: 0,
        }
    }

    /// A preview of `text` in at most `capacity` bytes of memory.
    pub fn of(text: &str, capacity: usize) -> Self {
        let mut preview = Self::new(capacity);
        preview.append(text.as_bytes());
        preview
    }

    pub fn append(&mut self, mut bytes: &[u8]) {
        self.captured = self.captured.saturating_add(bytes.len() as u64);
        loop {
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let content = &bytes[..newline.unwrap_or(bytes.len())];
            let keep = content.len().min(self.line_cap - self.open.kept.len());
            self.open.kept.extend_from_slice(&content[..keep]);
            self.open.len += content.len() as u64;
            let Some(index) = newline else { break };
            bytes = &bytes[index + 1..];
            self.keep_open();
            self.start_next_line();
        }
    }

    /// Moves the finished line to the head or the tail, making room before
    /// copying it so the preview never holds more than it is charged.
    fn keep_open(&mut self) {
        if !self.head_full {
            if self.head_bytes + self.open.cost() <= self.head_cap {
                self.head_bytes += self.open.cost();
                self.head.push(self.copy_open(self.open.kept.len()));
                return;
            }
            self.head_full = true;
            // A first line too long for the head keeps what fits of its start.
            if self.head.is_empty() && self.head_cap >= LINE_OVERHEAD {
                let line = self.copy_open(self.head_cap - LINE_OVERHEAD);
                self.head_bytes = line.cost();
                self.head.push(line);
                return;
            }
        }
        // The tail takes it, keeping what fits of its start, and older lines
        // give way. Without room for even its record, the tail cannot end at
        // the latest line, so it empties.
        let Some(room) = self.tail_cap.checked_sub(LINE_OVERHEAD) else {
            self.tail.clear();
            self.tail_bytes = 0;
            return;
        };
        let keep = self.open.kept.len().min(room);
        while self.tail_bytes + keep + LINE_OVERHEAD > self.tail_cap {
            let dropped = self.tail.pop_front().expect("an empty tail has room");
            self.tail_bytes -= dropped.cost();
        }
        let line = self.copy_open(keep);
        self.tail_bytes += line.cost();
        self.tail.push_back(line);
    }

    /// The finished line with an exact copy of its first `keep` kept bytes.
    fn copy_open(&self, keep: usize) -> Line {
        Line {
            number: self.open.number,
            start: self.open.start,
            len: self.open.len,
            kept: self.open.kept[..keep].to_vec(),
        }
    }

    fn start_next_line(&mut self) {
        self.open.number += 1;
        self.open.start += self.open.len + 1;
        self.open.len = 0;
        self.open.kept.clear();
    }

    pub fn captured_bytes(&self) -> u64 {
        self.captured
    }

    /// Memory the preview may hold.
    pub fn capacity(&self) -> usize {
        self.line_cap + self.head_cap + self.tail_cap
    }

    /// Every line is retained whole.
    pub fn complete(&self) -> bool {
        let (head, tail) = self.sides();
        head + tail == self.lines() as usize
            && self
                .retained()
                .all(|line| line.len == line.kept.len() as u64)
    }

    fn lines(&self) -> u64 {
        self.open.number - u64::from(self.open.len == 0)
    }

    /// The line still arriving belongs at the end of the head if it ended now.
    fn open_in_head(&self) -> bool {
        self.open.len > 0 && !self.head_full && self.head_bytes + self.open.cost() <= self.head_cap
    }

    /// How many lines the head and the tail hold, counting the line still
    /// arriving on the side it would go.
    fn sides(&self) -> (usize, usize) {
        let open_in_head = self.open_in_head();
        let open_in_tail = self.open.len > 0 && !open_in_head;
        (
            self.head.len() + usize::from(open_in_head),
            self.tail.len() + usize::from(open_in_tail),
        )
    }

    fn head_line(&self, index: usize) -> &Line {
        self.head.get(index).unwrap_or(&self.open)
    }

    fn tail_line(&self, index: usize) -> &Line {
        self.tail.get(index).unwrap_or(&self.open)
    }

    /// Every retained line in stream order.
    fn retained(&self) -> impl Iterator<Item = &Line> {
        let (head, tail) = self.sides();
        (0..head)
            .map(|index| self.head_line(index))
            .chain((0..tail).map(|index| self.tail_line(index)))
    }

    /// The preview within `limit` bytes, and whether it leaves anything out.
    ///
    /// Output that fits is returned exactly. Otherwise it shows whole lines
    /// from both ends, alternating, and names the lines left between them.
    /// Only the lines shown are rendered, so the work is bounded by `limit`.
    pub fn render(&self, limit: usize) -> (String, bool) {
        let (head, tail) = self.sides();
        let kept: usize = self.retained().map(|line| line.kept.len() + 1).sum();
        if self.complete() && kept <= limit.saturating_add(1) {
            let mut bytes = Vec::with_capacity(kept);
            for (index, line) in self.retained().enumerate() {
                if index > 0 {
                    bytes.push(b'\n');
                }
                bytes.extend_from_slice(&line.kept);
            }
            if self.captured > 0 && self.open.len == 0 {
                bytes.push(b'\n');
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if text.len() <= limit {
                return (text, false);
            }
        }
        // No single line may take more than half the space.
        let cap = MAX_LINE_BYTES.min(limit / 2);
        // Names the lines between the first `h` head lines and the last `t`
        // tail lines.
        let marker = |h: usize, t: usize| {
            let first = if h > 0 {
                self.head_line(h - 1).number + 1
            } else {
                1
            };
            let last = if t > 0 {
                self.tail_line(tail - t).number - 1
            } else {
                self.lines()
            };
            (first <= last).then(|| {
                let from = if h > 0 {
                    self.head_line(h - 1).end()
                } else {
                    0
                };
                let to = if t > 0 {
                    self.tail_line(tail - t).start
                } else {
                    self.captured
                };
                let lines = if first == last {
                    format!("line {first}")
                } else {
                    format!("lines {first}-{last}")
                };
                format!(
                    "[... {lines} omitted ({} bytes at offset {from}) ...]",
                    to - from
                )
            })
        };
        let size = |h: usize, t: usize, text: usize| {
            let marker = marker(h, t);
            let pieces = h + t + usize::from(marker.is_some());
            text + marker.map_or(0, |marker| marker.len()) + pieces.saturating_sub(1)
        };
        if size(0, 0, 0) > limit {
            return (String::new(), true);
        }
        // Chooses lines by their rendered length, rendering each again only
        // to write it, so no more than the result and one line are held.
        let rendered = |line: &Line| line.render(cap).len();
        let (mut h, mut t, mut text) = (0, 0, 0);
        let mut next_head = (head > 0).then(|| rendered(self.head_line(0)));
        let mut next_tail = (tail > 0).then(|| rendered(self.tail_line(tail - 1)));
        let mut head_turn = true;
        loop {
            let grow_head = next_head.is_some_and(|line| size(h + 1, t, text + line) <= limit);
            let grow_tail = next_tail.is_some_and(|line| size(h, t + 1, text + line) <= limit);
            let take_head = match (grow_head, grow_tail) {
                (false, false) => break,
                (true, false) => true,
                (false, true) => false,
                (true, true) => {
                    head_turn = !head_turn;
                    !head_turn
                }
            };
            if take_head {
                text += next_head.expect("a head line to show");
                h += 1;
                next_head = (h < head).then(|| rendered(self.head_line(h)));
            } else {
                text += next_tail.expect("a tail line to show");
                t += 1;
                next_tail = (t < tail).then(|| rendered(self.tail_line(tail - t - 1)));
            }
        }
        let marker = marker(h, t);
        let shown = (0..h)
            .map(|index| self.head_line(index).render(cap))
            .chain(marker)
            .chain((tail - t..tail).map(|index| self.tail_line(index).render(cap)));
        let mut result = String::with_capacity(size(h, t, text));
        for (index, piece) in shown.enumerate() {
            if index > 0 {
                result.push('\n');
            }
            result.push_str(&piece);
        }
        (result, true)
    }
}

/// What the model is shown of output kept outside memory: text placed ahead
/// of it, then each channel's preview.
#[derive(Clone, Debug, Default)]
pub struct OutputPreview {
    /// Placed first, such as an exit code. It is shortened only when it alone
    /// overflows the space.
    pub prefix: String,
    /// Each channel's preview, in capture order.
    pub channels: Vec<(Channel, Preview)>,
}

impl OutputPreview {
    /// A preview of `text` as the only channel, in at most `capacity` bytes
    /// of memory.
    pub fn of(channel: Channel, text: &str, capacity: usize) -> Self {
        Self {
            prefix: String::new(),
            channels: vec![(channel, Preview::of(text, capacity))],
        }
    }

    /// The preview within `limit` bytes, and whether it leaves anything out.
    /// Channels share the space evenly; the smaller ones render first and
    /// pass what they leave unused to the rest.
    pub fn render(&self, limit: usize) -> (String, bool) {
        let mut text = String::new();
        let mut cut = false;
        if self.prefix.len() <= limit {
            text.push_str(&self.prefix);
        } else {
            let mut end = limit;
            while !self.prefix.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&self.prefix[..end]);
            cut = true;
        }
        let channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, preview)| preview.captured_bytes() > 0)
            .collect();
        let headings: usize = channels
            .iter()
            .map(|(channel, _)| heading(*channel).map_or(0, |heading| heading.len() + 1))
            .sum();
        let mut left = limit.saturating_sub(text.len() + headings);
        let mut order: Vec<_> = (0..channels.len()).collect();
        order.sort_by_key(|&index| channels[index].1.captured_bytes());
        let mut shown = vec![String::new(); channels.len()];
        for (position, &index) in order.iter().enumerate() {
            let (rendered, omitted) = channels[index].1.render(left / (channels.len() - position));
            left -= rendered.len();
            cut |= omitted;
            shown[index] = rendered;
        }
        // Headings separate channels from each other, not from the prefix.
        let mut body = String::new();
        for ((channel, _), shown) in channels.iter().zip(&shown) {
            append_channel_text(&mut body, *channel, shown);
        }
        text.push_str(&body);
        (text, cut)
    }
}

/// The label placed before a channel's text when several share one result.
fn heading(channel: Channel) -> Option<&'static str> {
    match channel {
        Channel::Stderr => Some("stderr:\n"),
        Channel::Log => Some("log:\n"),
        _ => None,
    }
}

/// Appends one channel's text to a combined result, under its heading.
pub fn append_channel_text(text: &mut String, channel: Channel, content: &str) {
    if content.is_empty() {
        return;
    }
    if let Some(heading) = heading(channel) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(heading);
    }
    text.push_str(content);
}

/// The longest start of `text` that ends with a whole line and fits `limit`.
pub fn leading_lines(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    text.as_bytes()[..=limit]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or("", |end| &text[..end])
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
#[path = "preview_tests.rs"]
mod tests;
