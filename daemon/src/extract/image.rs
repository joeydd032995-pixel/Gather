//! Image analysis (write-up §5.1 phase 1): dimensions, EXIF metadata, and
//! OCR via the Tesseract CLI invoked as a local subprocess (no C bindings,
//! no network — consistent with the offline-by-default policy).

use std::io::Cursor;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Map, Value};
use tokio::process::Command;

pub struct ImageAnalysis {
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub exif: Value,
    pub taken_at: Option<DateTime<Utc>>,
}

/// Best-effort metadata pass; never fails (missing EXIF is normal for
/// screenshots, undecodable headers just yield NULL dimensions).
pub fn analyze(bytes: &[u8]) -> ImageAnalysis {
    let (width, height) = match imagesize::blob_size(bytes) {
        Ok(dim) => (Some(dim.width as i32), Some(dim.height as i32)),
        Err(_) => (None, None),
    };

    let mut exif_map = Map::new();
    let mut taken_at = None;
    if let Ok(exif) = exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) {
        for field in exif.fields() {
            exif_map.insert(
                field.tag.to_string(),
                Value::String(field.display_value().with_unit(&exif).to_string()),
            );
        }
        // EXIF timestamps are local naive times; interpret as UTC (documented
        // approximation — offset tags are rarely present).
        taken_at = exif
            .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .and_then(|s| NaiveDateTime::parse_from_str(&s, "%Y:%m:%d %H:%M:%S").ok())
            .map(|dt| DateTime::from_naive_utc_and_offset(dt, Utc));
    }

    ImageAnalysis {
        width,
        height,
        exif: Value::Object(exif_map),
        taken_at,
    }
}

pub struct OcrResult {
    pub text: String,
    /// Mean word confidence, 0.0–1.0.
    pub confidence: f32,
}

pub enum OcrOutcome {
    Ok(OcrResult),
    /// Tesseract produced no words (blank image is not an error).
    Empty,
    /// The tesseract binary is not installed/executable.
    Unavailable,
    Failed(String),
}

/// Run `tesseract <file> stdout tsv` and reassemble text line-by-line from
/// the TSV word rows (level 5), averaging per-word confidence.
pub async fn ocr(tesseract_path: &str, bytes: &[u8], extension: &str) -> OcrOutcome {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return OcrOutcome::Failed(format!("tempdir: {e}")),
    };
    let input = dir.path().join(format!("input.{extension}"));
    if let Err(e) = tokio::fs::write(&input, bytes).await {
        return OcrOutcome::Failed(format!("write temp image: {e}"));
    }

    let output = Command::new(tesseract_path)
        .arg(&input)
        .arg("stdout")
        .arg("tsv")
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OcrOutcome::Unavailable,
        Err(e) => return OcrOutcome::Failed(format!("spawn tesseract: {e}")),
    };
    if !output.status.success() {
        return OcrOutcome::Failed(format!(
            "tesseract exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    parse_tsv(&String::from_utf8_lossy(&output.stdout))
}

fn parse_tsv(tsv: &str) -> OcrOutcome {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line_key = (0u32, 0u32, 0u32, 0u32);
    let mut current_words: Vec<String> = Vec::new();
    let mut conf_sum = 0f32;
    let mut conf_count = 0u32;

    for row in tsv.lines().skip(1) {
        let cols: Vec<&str> = row.split('\t').collect();
        // level page block par line word left top width height conf text
        if cols.len() < 12 || cols[0] != "5" {
            continue;
        }
        let conf: f32 = cols[10].parse().unwrap_or(-1.0);
        let word = cols[11].trim();
        if conf < 0.0 || word.is_empty() {
            continue;
        }
        let key = (
            cols[1].parse().unwrap_or(0),
            cols[2].parse().unwrap_or(0),
            cols[3].parse().unwrap_or(0),
            cols[4].parse().unwrap_or(0),
        );
        if key != current_line_key && !current_words.is_empty() {
            lines.push(current_words.join(" "));
            current_words.clear();
        }
        current_line_key = key;
        current_words.push(word.to_string());
        conf_sum += conf;
        conf_count += 1;
    }
    if !current_words.is_empty() {
        lines.push(current_words.join(" "));
    }

    if conf_count == 0 {
        return OcrOutcome::Empty;
    }
    OcrOutcome::Ok(OcrResult {
        text: lines.join("\n"),
        confidence: (conf_sum / conf_count as f32 / 100.0).clamp(0.0, 1.0),
    })
}

/// File extension for the temp OCR input, from the artifact's media type.
pub fn extension_for(media_type: Option<&str>) -> &'static str {
    match media_type {
        Some("image/jpeg") => "jpg",
        Some("image/webp") => "webp",
        Some("image/tiff") => "tif",
        _ => "png",
    }
}

pub fn exif_debug_summary(analysis: &ImageAnalysis) -> Value {
    json!({
        "width": analysis.width,
        "height": analysis.height,
        "exif_fields": analysis.exif.as_object().map(|m| m.len()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_reads_png_dimensions() {
        let bytes = include_bytes!("../../tests/fixtures/tiny.png");
        let a = analyze(bytes);
        assert!(a.width.unwrap() > 0);
        assert!(a.height.unwrap() > 0);
        assert!(a.taken_at.is_none()); // PNG has no EXIF DateTimeOriginal
    }

    #[test]
    fn tsv_parser_averages_confidence_and_rebuilds_lines() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\tHello\n\
                   5\t1\t1\t1\t1\t2\t12\t0\t10\t10\t80\tworld\n\
                   5\t1\t1\t1\t2\t1\t0\t12\t10\t10\t70\tagain\n";
        match parse_tsv(tsv) {
            OcrOutcome::Ok(r) => {
                assert_eq!(r.text, "Hello world\nagain");
                assert!((r.confidence - 0.8).abs() < 0.001);
            }
            _ => panic!("expected Ok"),
        }
    }

    #[tokio::test]
    async fn missing_binary_reports_unavailable() {
        let bytes = include_bytes!("../../tests/fixtures/tiny.png");
        match ocr("/nonexistent/tesseract-binary", bytes, "png").await {
            OcrOutcome::Unavailable => {}
            _ => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn ocr_reads_fixture_when_tesseract_installed() {
        let bytes = include_bytes!("../../tests/fixtures/tiny.png");
        match ocr("tesseract", bytes, "png").await {
            OcrOutcome::Unavailable => eprintln!("tesseract not installed; skipping"),
            OcrOutcome::Ok(r) => {
                assert!(!r.text.is_empty());
                assert!(r.confidence > 0.0);
            }
            OcrOutcome::Empty => panic!("fixture contains large text; OCR saw nothing"),
            OcrOutcome::Failed(e) => panic!("ocr failed: {e}"),
        }
    }
}
