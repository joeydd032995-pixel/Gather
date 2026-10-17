//! Deterministic markdown / plain-text segmentation, shared by synchronous
//! upload-time extraction (markdown/text) and the PDF extraction worker.

use std::borrow::Cow;

pub struct Segment {
    pub heading: Option<String>,
    pub content: String,
}

const MAX_SEGMENT_CHARS: usize = 2000;

/// Deterministic segmentation: split on markdown headings, then split any
/// oversized section on paragraph boundaries so each segment stays under
/// MAX_SEGMENT_CHARS (embedding-friendly and provenance-precise). A paragraph
/// that is itself over the cap is cut where the rule extractor would split a
/// sentence anyway (see [`cut_point`]), so a cut does not halve a sentence the
/// extractor could read.
pub fn segment_text(text: &str) -> Vec<Segment> {
    Segments::new(text).collect()
}

/// [`segment_text`] as a lazy iterator. Segments are produced one at a time,
/// borrowing from `text` rather than copying it, so segmenting a large
/// document costs memory proportional to one segment, not to the document.
/// The output is identical to the eager form.
pub struct Segments<'a> {
    text: &'a str,
    /// Byte offset of the next unread line.
    pos: usize,
    exhausted: bool,
    /// Heading that applies to the next section read.
    heading: Option<String>,
    /// The section currently being split, and its heading.
    chunks: Option<Chunks<'a>>,
    chunk_heading: Option<String>,
}

impl<'a> Segments<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            pos: 0,
            exhausted: false,
            heading: None,
            chunks: None,
            chunk_heading: None,
        }
    }

    /// Read lines up to the next heading (or the end): the next section's
    /// byte range, if it has any lines.
    fn next_section(&mut self) -> Option<(usize, usize)> {
        let mut section: Option<(usize, usize)> = None;
        let mut next_heading = None;
        let mut consumed = 0;
        for raw in self.text[self.pos..].split_inclusive('\n') {
            let line_start = self.pos + consumed;
            consumed += raw.len();
            // Same line splitting as `str::lines`.
            let line = match raw.strip_suffix('\n') {
                Some(l) => l.strip_suffix('\r').unwrap_or(l),
                None => raw,
            };
            if let Some(h) = parse_heading(line) {
                next_heading = Some(h);
                break;
            }
            let end = line_start + line.len();
            section = Some(section.map_or((line_start, end), |(start, _)| (start, end)));
        }
        self.pos += consumed;
        match next_heading {
            Some(h) => {
                self.chunk_heading = self.heading.replace(h);
            }
            None => {
                self.exhausted = true;
                self.chunk_heading = self.heading.clone();
            }
        }
        section
    }
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        loop {
            if let Some(chunks) = self.chunks.as_mut() {
                if let Some(content) = chunks.next() {
                    return Some(Segment {
                        heading: self.chunk_heading.clone(),
                        content,
                    });
                }
                self.chunks = None;
            }
            if self.exhausted {
                return None;
            }
            let Some((start, end)) = self.next_section() else {
                continue;
            };
            let slice = &self.text[start..end];
            // The section's lines joined by "\n": the text itself, unless it
            // contains carriage returns that line splitting would drop.
            let body: Cow<'_, str> = if slice.contains('\r') {
                Cow::Owned(
                    slice
                        .lines()
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string(),
                )
            } else {
                Cow::Borrowed(slice.trim())
            };
            if !body.is_empty() {
                self.chunks = Some(Chunks::new(body));
            }
        }
    }
}

fn parse_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
        Some(trimmed[hashes + 1..].trim().to_string())
    } else {
        None
    }
}

/// The largest char boundary of `s` at or below `i`. (`str::floor_char_boundary`
/// is not stable on our MSRV.)
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Where to end the next chunk of an oversized paragraph: a byte offset into
/// `pending` (the paragraph's remaining text, longer than `max`) on a char
/// boundary, at most `max` (or the first char, if that alone is longer).
///
/// The rule extractor reads one sentence at a time. It splits after `.`/`!`/`?`
/// followed by whitespace and at every newline (`rules::sentences`), and it
/// ignores any sentence over 400 bytes. A cut in the middle of a sentence hands
/// the extractor two halves that are each short enough to be read as a
/// statement of their own. So the cut goes where the extractor would have split
/// anyway, preferring, in order: the last sentence end, the last newline, the
/// last whitespace, and only then the hard cut at `max`. Offsets below `max / 4`
/// are ignored so a stray early boundary does not leave a runt chunk behind.
///
/// Only the first `max + 1` bytes are read: the extra byte says whether a
/// terminator at the very end of the window is followed by whitespace. That
/// keeps a cut independent of how long the rest of the paragraph is.
fn cut_point(pending: &str, max: usize) -> usize {
    let window = &pending.as_bytes()[..floor_boundary(pending, pending.len().min(max + 1))];
    let (mut newline, mut space) = (None, None);
    for cut in ((max / 4).max(1)..=max.min(window.len())).rev() {
        let next = window.get(cut).copied();
        let before_space = next.is_some_and(|b| b.is_ascii_whitespace());
        if matches!(window[cut - 1], b'.' | b'!' | b'?') && before_space {
            return cut;
        }
        if newline.is_none() && next == Some(b'\n') {
            newline = Some(cut);
        }
        if space.is_none() && before_space {
            space = Some(cut);
        }
    }
    newline
        .or(space)
        .unwrap_or_else(|| match floor_boundary(pending, max) {
            // Only a cap smaller than one char gets here; still make progress.
            0 => pending.chars().next().map_or(0, char::len_utf8),
            cut => cut,
        })
}

/// Splits one section body on paragraph boundaries into chunks of at most
/// MAX_SEGMENT_CHARS, lazily. Pending text is `current` (at most one chunk,
/// owned) followed by `tail`, a byte range of `body` that is never copied
/// until it is emitted, so a huge paragraph costs one chunk of memory.
struct Chunks<'a> {
    body: Cow<'a, str>,
    single: bool,
    next_para: Option<usize>,
    current: String,
    tail: (usize, usize),
    finished: bool,
}

impl<'a> Chunks<'a> {
    fn new(body: Cow<'a, str>) -> Self {
        let single = body.len() <= MAX_SEGMENT_CHARS;
        Self {
            body,
            single,
            next_para: Some(0),
            current: String::new(),
            tail: (0, 0),
            finished: false,
        }
    }

    fn pending_len(&self) -> usize {
        self.current.len() + (self.tail.1 - self.tail.0)
    }

    fn pending(&self) -> String {
        let mut s = self.current.clone();
        s.push_str(&self.body[self.tail.0..self.tail.1]);
        s
    }

    /// Byte range of the next "\n\n"-separated paragraph (as `str::split`).
    fn take_paragraph(&mut self) -> Option<(usize, usize)> {
        let start = self.next_para?;
        match self.body[start..].find("\n\n") {
            Some(i) => {
                self.next_para = Some(start + i + 2);
                Some((start, start + i))
            }
            None => {
                self.next_para = None;
                Some((start, self.body.len()))
            }
        }
    }

    /// Emit the next chunk of the pending text, ended where [`cut_point`]
    /// says, and keep the rest pending.
    fn cut(&mut self) -> String {
        let clen = self.current.len();
        // `cut_point` reads at most MAX + 1 bytes, so copy only that much of
        // the pending text: the whole of a huge paragraph would make every
        // cut cost the paragraph's length.
        let want = MAX_SEGMENT_CHARS + 1;
        let mut head = String::with_capacity(want);
        if clen >= want {
            head.push_str(&self.current[..floor_boundary(&self.current, want)]);
        } else {
            head.push_str(&self.current);
            let tail = &self.body[self.tail.0..self.tail.1];
            head.push_str(&tail[..floor_boundary(tail, want - clen)]);
        }
        let cut = cut_point(&head, MAX_SEGMENT_CHARS);
        if cut <= clen {
            let chunk = self.current[..cut].trim().to_string();
            self.current.drain(..cut);
            chunk
        } else {
            let take = cut - clen;
            let mut chunk = std::mem::take(&mut self.current);
            chunk.push_str(&self.body[self.tail.0..self.tail.0 + take]);
            self.tail.0 += take;
            chunk.trim().to_string()
        }
    }
}

impl Iterator for Chunks<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        if self.single {
            if self.finished {
                return None;
            }
            self.finished = true;
            return Some(self.body.to_string());
        }
        loop {
            // A paragraph larger than the cap is cut where `cut_point` says.
            if self.pending_len() > MAX_SEGMENT_CHARS {
                return Some(self.cut());
            }
            let Some((start, end)) = self.take_paragraph() else {
                if self.finished {
                    return None;
                }
                self.finished = true;
                let rest = self.pending();
                let rest = rest.trim();
                return (!rest.is_empty()).then(|| rest.to_string());
            };
            let emitted = if self.pending_len() > 0
                && self.pending_len() + (end - start) + 2 > MAX_SEGMENT_CHARS
            {
                let full = self.pending().trim().to_string();
                self.current.clear();
                self.tail = (end, end);
                Some(full)
            } else {
                None
            };
            // What is still pending is under the cap: own it, then borrow
            // the new paragraph.
            let (t0, t1) = self.tail;
            self.current.push_str(&self.body[t0..t1]);
            if !self.current.is_empty() {
                self.current.push_str("\n\n");
            }
            self.tail = (start, end);
            if emitted.is_some() {
                return emitted;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original eager implementation, kept to prove the lazy one
    /// produces exactly the same segments. It shares `cut_point`, so this
    /// proves the lazy bookkeeping; the cut rule has its own tests below.
    fn reference(text: &str) -> Vec<(Option<String>, String)> {
        fn split(body: &str) -> Vec<String> {
            if body.len() <= MAX_SEGMENT_CHARS {
                return vec![body.to_string()];
            }
            let mut chunks = Vec::new();
            let mut current = String::new();
            for para in body.split("\n\n") {
                if !current.is_empty() && current.len() + para.len() + 2 > MAX_SEGMENT_CHARS {
                    chunks.push(current.trim().to_string());
                    current = String::new();
                }
                if !current.is_empty() {
                    current.push_str("\n\n");
                }
                current.push_str(para);
                while current.len() > MAX_SEGMENT_CHARS {
                    let cut = cut_point(&current, MAX_SEGMENT_CHARS);
                    let rest = current.split_off(cut);
                    chunks.push(current.trim().to_string());
                    current = rest;
                }
            }
            if !current.trim().is_empty() {
                chunks.push(current.trim().to_string());
            }
            chunks
        }
        let mut out = Vec::new();
        let mut heading: Option<String> = None;
        let mut lines: Vec<&str> = Vec::new();
        let mut flush = |heading: &Option<String>, lines: &mut Vec<&str>| {
            let body = lines.join("\n").trim().to_string();
            lines.clear();
            if !body.is_empty() {
                for chunk in split(&body) {
                    out.push((heading.clone(), chunk));
                }
            }
        };
        for line in text.lines() {
            if let Some(h) = parse_heading(line) {
                flush(&heading, &mut lines);
                heading = Some(h);
            } else {
                lines.push(line);
            }
        }
        flush(&heading, &mut lines);
        out
    }

    #[test]
    fn lazy_segmentation_matches_the_original_exactly() {
        const PIECES: &[&str] = &[
            "word ",
            "a",
            " ",
            "\n",
            "\n\n",
            "\r\n",
            "\r",
            "# Heading\n",
            "## Sub\n",
            "é",
            "日本",
            "  \n",
            "\n\n\n",
            "#nothead\n",
            "longword",
            // Sentence ends and near-misses, so the oversized paragraphs
            // above reach every branch of `cut_point`.
            "Yes. ",
            "Really? ",
            "Done!\n",
            "end.",
            "3.5 ",
            "e.g.",
        ];
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..400 {
            // Mix short and long documents so both the single-chunk and the
            // paragraph-splitting paths run, including oversized paragraphs.
            let len = if case % 4 == 0 {
                3000
            } else {
                (rand() % 400) as usize
            };
            let mut text = String::new();
            for _ in 0..len {
                let piece = PIECES[(rand() % PIECES.len() as u64) as usize];
                let repeat = if rand() % 50 == 0 { 400 } else { 1 };
                for _ in 0..repeat {
                    text.push_str(piece);
                }
            }
            let lazy: Vec<(Option<String>, String)> = segment_text(&text)
                .into_iter()
                .map(|s| (s.heading, s.content))
                .collect();
            assert_eq!(lazy, reference(&text), "case {case} differs");
        }
    }

    #[test]
    fn segments_markdown_by_heading() {
        let md = "# Intro\nHello world.\n\n## Details\nMore text here.\n";
        let segs = segment_text(md);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].heading.as_deref(), Some("Intro"));
        assert_eq!(segs[0].content, "Hello world.");
        assert_eq!(segs[1].heading.as_deref(), Some("Details"));
    }

    #[test]
    fn splits_oversized_sections() {
        let long_para = "word ".repeat(1000); // ~5000 chars, single paragraph
        let segs = segment_text(&long_para);
        assert!(segs.len() >= 3);
        assert!(segs.iter().all(|s| s.content.len() <= MAX_SEGMENT_CHARS));
        // No sentence ends to cut at, so the cut is at a space: whole words.
        assert!(segs
            .iter()
            .all(|s| s.content.starts_with("word") && s.content.ends_with("word")));
    }

    /// A xorshift generator, as in the parity test above.
    fn rng(mut seed: u64) -> impl FnMut() -> u64 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        }
    }

    #[test]
    fn an_oversized_paragraph_is_cut_between_sentences() {
        let sentence = "The quick brown fox jumps over the lazy dog near the river bank.";
        let text = vec![sentence; 100].join(" "); // about 6.5 KB, one paragraph
        let segs = segment_text(&text);
        assert!(segs.len() >= 4);
        let mut sentences = 0;
        for seg in &segs {
            let c = &seg.content;
            assert!(c.len() <= MAX_SEGMENT_CHARS);
            assert!(
                c.starts_with("The quick") && c.ends_with("river bank."),
                "{c:?}"
            );
            let starts = c.matches("The quick").count();
            assert_eq!(
                starts,
                c.matches("river bank.").count(),
                "a sentence was split"
            );
            sentences += starts;
        }
        assert_eq!(sentences, 100, "no sentence lost or repeated");
    }

    #[test]
    fn paragraphs_that_fit_are_never_split() {
        let words = [
            "alpha",
            "beta",
            "gamma",
            "delta",
            "Sentence one.",
            "why?",
            "ok!",
        ];
        let mut rand = rng(0xDEAD_BEEF_CAFE_F00D);
        for case in 0..200 {
            let paragraphs: Vec<String> = (0..1 + rand() % 12)
                .map(|_| {
                    let target = 20 + (rand() % 1400) as usize; // always under the cap
                    let mut p = String::new();
                    while p.len() < target {
                        if !p.is_empty() {
                            p.push(if rand() % 9 == 0 { '\n' } else { ' ' });
                        }
                        p.push_str(words[(rand() % words.len() as u64) as usize]);
                    }
                    p
                })
                .collect();
            let segs = segment_text(&paragraphs.join("\n\n"));
            for p in &paragraphs {
                assert!(
                    segs.iter().any(|s| s.content.contains(p.as_str())),
                    "case {case}: a paragraph of {} bytes was split",
                    p.len()
                );
            }
        }
    }

    #[test]
    fn cut_point_prefers_the_last_sentence_end() {
        // Each sentence is 24 bytes, so its terminator is at 24k + 22 and the
        // last end inside a 100-byte chunk leaves the cut at 95.
        for end in ['.', '!', '?'] {
            let text = format!("Alpha beta gamma delta{end} ").repeat(10);
            assert_eq!(cut_point(&text, 100), 95, "{end}");
        }
    }

    #[test]
    fn cut_point_falls_back_to_the_last_newline_then_the_last_space() {
        // No terminators: the last line break beats a later space.
        let lines = "alpha beta gamma delta\n".repeat(10);
        assert_eq!(cut_point(&lines, 100), 91);
        // No line breaks either: the last space.
        assert_eq!(cut_point(&"alpha ".repeat(50), 100), 95);
    }

    #[test]
    fn cut_point_ignores_boundaries_too_early_to_be_worth_a_chunk() {
        // The only sentence end is 10 bytes in, below max / 4: not worth a
        // runt chunk, so this is the hard cut.
        let text = format!("Short one. {}", "x".repeat(300));
        assert_eq!(cut_point(&text, 100), 100);
    }

    #[test]
    fn cut_point_needs_whitespace_after_a_terminator() {
        // "3.5", "e.g.x" and "a.b" do not end a sentence, as for the extractor:
        // the `.` at byte 99 is followed by `b`, so the cut is the last space.
        let text = format!(
            "{}{}.b{}",
            "word ".repeat(10),
            "a".repeat(49),
            "c".repeat(60)
        );
        assert_eq!(cut_point(&text, 100), 49);
        // The same `.` followed by a space (outside the chunk, inside the
        // window) does end it.
        let text = format!("{}. {}", "a".repeat(99), "b".repeat(50));
        assert_eq!(cut_point(&text, 100), 100);
    }

    #[test]
    fn cut_point_never_splits_a_char() {
        // A 3-byte char straddles max + 1, so the window stops before it. The
        // `.` that ends the window is then not known to be followed by a space
        // (it is followed by "日"), and the cut is the space at byte 40.
        let text = format!(
            "{} {}.{}",
            "x".repeat(40),
            "y".repeat(57),
            "日本語".repeat(20)
        );
        assert_eq!(cut_point(&text, 100), 40);
        // Without any boundary it is today's hard cut, backed off to a char.
        assert_eq!(cut_point(&"x".repeat(300), 100), 100);
        assert_eq!(cut_point(&"é".repeat(150), 101), 100);
        assert_eq!(cut_point(&"日".repeat(100), 100), 99);
        // Only the first max + 1 bytes matter, however long the rest is.
        let text = format!("{}. {}", "a".repeat(60), "b".repeat(10_000));
        assert_eq!(cut_point(&text, 100), cut_point(&text[..101], 100));
    }

    #[test]
    fn cut_point_always_makes_progress_and_stays_in_bounds() {
        // A cap smaller than one char still advances by that char.
        assert_eq!(cut_point("日本日本", 2), 3);
        const PIECES: &[&str] = &["a", " ", "\n", ". ", "? ", "!", "é", "日", "word "];
        let mut rand = rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..2000 {
            let mut text = String::new();
            while text.len() < 20 + (rand() % 200) as usize {
                text.push_str(PIECES[(rand() % PIECES.len() as u64) as usize]);
            }
            for max in [8, 20, 64] {
                if text.len() <= max {
                    continue;
                }
                let cut = cut_point(&text, max);
                assert!(
                    (1..=max).contains(&cut),
                    "{cut} outside 1..={max} for {text:?}"
                );
                assert!(
                    text.is_char_boundary(cut),
                    "{cut} splits a char in {text:?}"
                );
            }
        }
    }
}
