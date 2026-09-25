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
/// MAX_SEGMENT_CHARS (embedding-friendly and provenance-precise).
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

    /// Emit the first MAX_SEGMENT_CHARS bytes of the pending text (backed off
    /// to a char boundary) and keep the rest pending.
    fn cut(&mut self) -> String {
        let clen = self.current.len();
        let mut cut = MAX_SEGMENT_CHARS;
        loop {
            let on_boundary = if cut <= clen {
                self.current.is_char_boundary(cut)
            } else {
                self.body.is_char_boundary(self.tail.0 + cut - clen)
            };
            if on_boundary {
                break;
            }
            cut -= 1;
        }
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
            // A paragraph larger than the cap is split on char boundaries.
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
    /// produces exactly the same segments.
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
                    let mut cut = MAX_SEGMENT_CHARS;
                    while !current.is_char_boundary(cut) {
                        cut -= 1;
                    }
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
    }
}
