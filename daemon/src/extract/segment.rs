//! Deterministic markdown / plain-text segmentation, shared by synchronous
//! upload-time extraction (markdown/text) and the PDF extraction worker.

pub struct Segment {
    pub heading: Option<String>,
    pub content: String,
}

const MAX_SEGMENT_CHARS: usize = 2000;

/// Deterministic segmentation: split on markdown headings, then split any
/// oversized section on paragraph boundaries so each segment stays under
/// MAX_SEGMENT_CHARS (embedding-friendly and provenance-precise).
pub fn segment_text(text: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current: Vec<&str> = Vec::new();

    let flush = |heading: &Option<String>, lines: &mut Vec<&str>, segments: &mut Vec<Segment>| {
        let body = lines.join("\n").trim().to_string();
        lines.clear();
        if body.is_empty() {
            return;
        }
        for chunk in split_paragraph_chunks(&body) {
            segments.push(Segment {
                heading: heading.clone(),
                content: chunk,
            });
        }
    };

    for line in text.lines() {
        if let Some(heading) = parse_heading(line) {
            flush(&current_heading, &mut current, &mut segments);
            current_heading = Some(heading);
        } else {
            current.push(line);
        }
    }
    flush(&current_heading, &mut current, &mut segments);
    segments
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

fn split_paragraph_chunks(body: &str) -> Vec<String> {
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
        // A single paragraph larger than the cap is split on char boundaries.
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

#[cfg(test)]
mod tests {
    use super::*;

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
