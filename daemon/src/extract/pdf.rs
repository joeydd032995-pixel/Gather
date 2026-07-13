//! PDF text extraction (write-up §5.1 phase 1).
//!
//! Pure-Rust (`pdf-extract` + `lopdf`), no system dependencies. Extraction
//! runs on a blocking thread with a panic guard because malformed PDFs can
//! panic inside the parser; a bad document fails alone, never the worker.

pub struct PdfExtraction {
    pub text: String,
    pub page_count: Option<i32>,
}

pub enum PdfOutcome {
    /// Usable text extracted.
    Ok(PdfExtraction),
    /// Structurally valid but effectively textless (scanned/image-only PDF);
    /// needs the OCR-rasterization path that is not implemented yet.
    NeedsOcr { page_count: Option<i32> },
    /// Unparseable or panicking document.
    Failed(String),
}

/// Average extracted chars per page below which a PDF is treated as scanned.
const MIN_CHARS_PER_PAGE: usize = 50;

pub async fn extract(bytes: Vec<u8>) -> PdfOutcome {
    let joined =
        tokio::task::spawn_blocking(move || std::panic::catch_unwind(|| extract_sync(&bytes)))
            .await;
    match joined {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_panic)) => PdfOutcome::Failed("pdf parser panicked on this document".to_string()),
        Err(join_err) => PdfOutcome::Failed(format!("extraction task failed: {join_err}")),
    }
}

fn extract_sync(bytes: &[u8]) -> PdfOutcome {
    let page_count = lopdf::Document::load_mem(bytes)
        .ok()
        .map(|doc| doc.get_pages().len() as i32);

    let text = match pdf_extract::extract_text_from_mem(bytes) {
        Ok(t) => t,
        Err(e) => return PdfOutcome::Failed(format!("pdf text extraction failed: {e}")),
    };

    let trimmed_len = text.trim().len();
    let pages = page_count.unwrap_or(1).max(1) as usize;
    if trimmed_len / pages < MIN_CHARS_PER_PAGE {
        return PdfOutcome::NeedsOcr { page_count };
    }
    PdfOutcome::Ok(PdfExtraction {
        text: unwrap_hard_lines(&text),
        page_count,
    })
}

/// PDF text extraction yields hard-wrapped lines that split sentences
/// mid-thought. Join single newlines within a paragraph into spaces while
/// preserving blank-line paragraph breaks, so segmentation and sentence-level
/// unit extraction see natural prose.
fn unwrap_hard_lines(text: &str) -> String {
    text.split("\n\n")
        .map(|para| {
            para.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extracts_text_from_fixture() {
        let bytes = include_bytes!("../../tests/fixtures/tiny.pdf").to_vec();
        match extract(bytes).await {
            PdfOutcome::Ok(out) => {
                assert!(out.text.contains("budget"), "text was: {}", out.text);
                assert_eq!(out.page_count, Some(1));
            }
            PdfOutcome::NeedsOcr { .. } => panic!("fixture should have extractable text"),
            PdfOutcome::Failed(e) => panic!("fixture failed to parse: {e}"),
        }
    }

    #[tokio::test]
    async fn garbage_bytes_fail_gracefully() {
        match extract(b"not a pdf at all".to_vec()).await {
            PdfOutcome::Failed(_) => {}
            _ => panic!("garbage must be Failed, not Ok/NeedsOcr"),
        }
    }
}
