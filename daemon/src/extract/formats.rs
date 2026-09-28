//! Text from the document formats found in project folders: Word (`.docx`),
//! spreadsheets (`.xlsx`, `.xlsm`, `.xls`, `.ods`), HTML, and the many plain
//! text formats (code, CSV, JSON, YAML, …).
//!
//! Everything here is pure and synchronous: bytes in, text out. Office files
//! are zip archives, so each is checked against zip-bomb limits before any of
//! it is decompressed, and every read is bounded.

use std::borrow::Cow;
use std::io::{Cursor, Read};

use quick_xml::events::Event;
use regex::Regex;

/// Longest text kept from one file. Anything past it is dropped with a note,
/// so one enormous export can't stall extraction for everything behind it.
pub const MAX_TEXT_BYTES: usize = 20 * 1024 * 1024;
/// Most bytes one office file may expand to when unzipped.
const MAX_UNZIPPED_BYTES: u64 = 256 * 1024 * 1024;
/// Most an entry may expand relative to its compressed size before the file
/// is treated as a zip bomb (ordinary office files stay far below this).
const MAX_RATIO: u64 = 200;
/// Spreadsheet rows kept per sheet.
const MAX_ROWS_PER_SHEET: usize = 50_000;

/// Plain-text formats, by extension. Their bytes are the text.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "text",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "ndjson",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "xml",
    "svg",
    "css",
    "scss",
    "less",
    "sql",
    "graphql",
    "gql",
    "proto",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "py",
    "pyi",
    "ipynb",
    "rs",
    "go",
    "rb",
    "php",
    "pl",
    "lua",
    "r",
    "jl",
    "js",
    "mjs",
    "cjs",
    "ts",
    "mts",
    "cts",
    "tsx",
    "jsx",
    "vue",
    "svelte",
    "java",
    "kt",
    "kts",
    "scala",
    "groovy",
    "gradle",
    "swift",
    "m",
    "mm",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "hh",
    "cs",
    "fs",
    "vb",
    "dart",
    "ex",
    "exs",
    "erl",
    "hs",
    "clj",
    "elm",
    "zig",
    "nim",
    "tf",
    "hcl",
    "rst",
    "adoc",
    "asciidoc",
    "org",
    "tex",
    "bib",
    "srt",
    "vtt",
];
/// Extension-less files that are plain text.
const TEXT_NAMES: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "changelog",
    "authors",
    "contributing",
    "notice",
    "makefile",
    "dockerfile",
    "procfile",
    "gemfile",
    "rakefile",
    "vagrantfile",
    "jenkinsfile",
];
const HTML_EXTENSIONS: &[&str] = &["html", "htm", "xhtml"];
const SPREADSHEET_EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xlsb", "xls", "ods"];

/// How a file's text is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// UTF-8 text as is (Markdown, code, CSV, JSON, …).
    Plain,
    Html,
    Docx,
    Spreadsheet,
}

impl Format {
    /// The name recorded as the document's extraction tool.
    pub fn tool(self) -> &'static str {
        match self {
            Format::Plain => "utf8-passthrough",
            Format::Html => "html-text",
            Format::Docx => "docx-text",
            Format::Spreadsheet => "spreadsheet-text",
        }
    }
}

fn extension(filename: &str) -> Option<String> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let (stem, ext) = base.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

fn base_name(filename: &str) -> String {
    filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .to_ascii_lowercase()
}

/// The artifact kind for a file this module reads, from its name alone:
/// `document_text`, `document_docx` or `document_spreadsheet`. `None` for
/// everything else (PDFs, images and Markdown are classified by the caller).
pub fn kind_for(filename: &str) -> Option<&'static str> {
    match extension(filename).as_deref() {
        Some("docx" | "docm") => Some("document_docx"),
        Some(ext) if SPREADSHEET_EXTENSIONS.contains(&ext) => Some("document_spreadsheet"),
        Some(ext) if HTML_EXTENSIONS.contains(&ext) || TEXT_EXTENSIONS.contains(&ext) => {
            Some("document_text")
        }
        Some(_) => None,
        None => TEXT_NAMES
            .contains(&base_name(filename).as_str())
            .then_some("document_text"),
    }
}

/// How to read a stored artifact of `kind` named `filename`.
pub fn format_for(kind: &str, filename: &str) -> Format {
    match kind {
        "document_docx" => Format::Docx,
        "document_spreadsheet" => Format::Spreadsheet,
        _ if extension(filename).is_some_and(|e| HTML_EXTENSIONS.contains(&e.as_str())) => {
            Format::Html
        }
        _ => Format::Plain,
    }
}

/// The text of a file, capped at [`MAX_TEXT_BYTES`]. The error is a short
/// reason a person can read ("not a text file", "damaged Word file").
pub fn to_text(format: Format, bytes: &[u8]) -> Result<Cow<'_, str>, String> {
    let text = match format {
        Format::Plain => {
            if looks_binary(bytes) {
                return Err("this looks like a binary file, not text".into());
            }
            String::from_utf8_lossy(bytes)
        }
        Format::Html => {
            if looks_binary(bytes) {
                return Err("this looks like a binary file, not HTML".into());
            }
            Cow::Owned(html_to_text(&String::from_utf8_lossy(bytes)))
        }
        Format::Docx => Cow::Owned(docx_to_text(bytes)?),
        Format::Spreadsheet => Cow::Owned(spreadsheet_to_text(bytes)?),
    };
    Ok(cap(text))
}

fn cap(text: Cow<'_, str>) -> Cow<'_, str> {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut kept = text[..end].to_string();
    kept.push_str(
        "\n\n[Gather: the rest of this file was not read; it is longer than 20 MB of text.]\n",
    );
    Cow::Owned(kept)
}

/// A NUL byte in the first 8 KB: text formats never contain one.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// ---------------------------------------------------------------- HTML ----

fn html_to_text(html: &str) -> String {
    use std::sync::OnceLock;
    static HIDDEN: OnceLock<Regex> = OnceLock::new();
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    static HEADING: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    static BLANKS: OnceLock<Regex> = OnceLock::new();
    let hidden = HIDDEN.get_or_init(|| {
        Regex::new(r"(?is)<!--.*?-->|<(script|style|noscript|template|svg)\b.*?</(script|style|noscript|template|svg)\s*>")
            .unwrap()
    });
    let heading = HEADING.get_or_init(|| Regex::new(r"(?i)<h([1-6])\b[^>]*>").unwrap());
    let block = BLOCK.get_or_init(|| {
        Regex::new(r"(?i)<(br|/p|/div|/li|/tr|/h[1-6]|/section|/article|/blockquote|/pre|/table|/ul|/ol|hr)\b[^>]*>")
            .unwrap()
    });
    let tag = TAG.get_or_init(|| Regex::new(r"(?s)<[^>]*>").unwrap());
    let blanks = BLANKS.get_or_init(|| Regex::new(r"\n[ \t]*(\n[ \t]*)+").unwrap());

    let text = hidden.replace_all(html, " ");
    // Headings keep their level as Markdown, so segmentation follows them.
    let text = heading.replace_all(&text, |c: &regex::Captures| {
        let level: usize = c[1].parse().unwrap_or(1);
        format!("\n\n{} ", "#".repeat(level))
    });
    let text = block.replace_all(&text, "\n");
    let text = tag.replace_all(&text, "");
    let text = decode_entities(&text);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    blanks
        .replace_all(&lines.join("\n"), "\n\n")
        .trim()
        .to_string()
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let decoded = tail.find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &tail[1..end];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "rsquo" => Some('’'),
                "lsquo" => Some('‘'),
                "rdquo" => Some('”'),
                "ldquo" => Some('“'),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// --------------------------------------------------------------- Office ----

/// Refuse an office file whose entries would expand past the limits.
fn check_zip(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| "not a valid Office file".to_string())?;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|_| "not a valid Office file".to_string())?;
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_UNZIPPED_BYTES
            || (size > 1024 * 1024 && size / entry.compressed_size().max(1) > MAX_RATIO)
        {
            return Err("this file expands far beyond its size; it was not opened".into());
        }
    }
    Ok(archive)
}

fn docx_to_text(bytes: &[u8]) -> Result<String, String> {
    let mut archive = check_zip(bytes)?;
    let mut xml = Vec::new();
    archive
        .by_name("word/document.xml")
        .map_err(|_| "this Word file has no document body".to_string())?
        .take(MAX_UNZIPPED_BYTES)
        .read_to_end(&mut xml)
        .map_err(|_| "this Word file is damaged".to_string())?;

    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut paragraph = String::new();
    let mut heading: Option<usize> = None;
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"t" => in_text = true,
                b"p" => {
                    paragraph.clear();
                    heading = None;
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"tab" => paragraph.push('\t'),
                b"br" | b"cr" => paragraph.push('\n'),
                b"pStyle" => {
                    heading = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == b"val")
                        .and_then(|a| {
                            let v = String::from_utf8_lossy(&a.value).to_ascii_lowercase();
                            if v == "title" {
                                return Some(1);
                            }
                            v.strip_prefix("heading")
                                .and_then(|n| n.trim().parse::<usize>().ok())
                                .map(|n| n.clamp(1, 6))
                        });
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                if let Ok(s) = t.decode() {
                    paragraph.push_str(&s);
                }
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    paragraph.push(c);
                } else if let Ok(name) = r.decode() {
                    if let Some(s) = quick_xml::escape::resolve_predefined_entity(&name) {
                        paragraph.push_str(s);
                    }
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"tc" => paragraph.push_str(" | "),
                b"p" => {
                    let line = paragraph.trim();
                    if !line.is_empty() {
                        if let Some(level) = heading {
                            out.push('\n');
                            out.push_str(&"#".repeat(level));
                            out.push(' ');
                        }
                        out.push_str(line);
                        out.push('\n');
                        if heading.is_some() {
                            out.push('\n');
                        }
                    }
                    paragraph.clear();
                    if out.len() > MAX_TEXT_BYTES {
                        break;
                    }
                }
                b"tr" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return Err("this Word file is damaged".into()),
            _ => {}
        }
        buf.clear();
    }
    Ok(out.trim().to_string())
}

fn spreadsheet_to_text(bytes: &[u8]) -> Result<String, String> {
    use calamine::Reader;
    // Only zip-based formats (xlsx, ods) can be zip bombs; .xls is not a zip.
    if bytes.starts_with(b"PK") {
        check_zip(bytes)?;
    }
    let mut workbook = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|_| "this spreadsheet could not be opened".to_string())?;
    let mut out = String::new();
    for name in workbook.sheet_names() {
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        let mut rows = range.rows().filter(|row| {
            row.iter()
                .any(|c| !matches!(c, calamine::Data::Empty) && !c.to_string().trim().is_empty())
        });
        let Some(header) = rows.next() else {
            continue;
        };
        out.push_str(&format!("## {name}\n\n"));
        let header: Vec<String> = header
            .iter()
            .map(|c| c.to_string().trim().to_string())
            .collect();
        out.push_str(&header.join(" | "));
        out.push('\n');
        for (i, row) in rows.enumerate() {
            if i >= MAX_ROWS_PER_SHEET || out.len() > MAX_TEXT_BYTES {
                out.push_str("[Gather: more rows were not read]\n");
                break;
            }
            let cells: Vec<String> = row
                .iter()
                .map(|c| c.to_string().trim().to_string())
                .collect();
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
        out.push('\n');
    }
    if out.trim().is_empty() {
        return Err("this spreadsheet has no cells with content".into());
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn kinds_come_from_names() {
        assert_eq!(kind_for("src/main.rs"), Some("document_text"));
        assert_eq!(kind_for("data/People.CSV"), Some("document_text"));
        assert_eq!(kind_for("site/index.html"), Some("document_text"));
        assert_eq!(kind_for("Makefile"), Some("document_text"));
        assert_eq!(kind_for("docs/Plan.docx"), Some("document_docx"));
        assert_eq!(kind_for("budget.xlsx"), Some("document_spreadsheet"));
        assert_eq!(kind_for("photo.jpg"), None);
        assert_eq!(kind_for("archive.zip"), None);
        assert_eq!(
            kind_for(".env"),
            None,
            "a dotfile has no extension to go by"
        );
        assert_eq!(format_for("document_text", "a/page.HTM"), Format::Html);
        assert_eq!(format_for("document_text", "a/app.ts"), Format::Plain);
    }

    #[test]
    fn plain_text_rejects_binary() {
        assert!(to_text(Format::Plain, b"fn main() {}").is_ok());
        assert!(to_text(Format::Plain, b"\x7fELF\x00\x01").is_err());
    }

    #[test]
    fn html_keeps_text_and_headings() {
        let html = r#"<html><head><title>T</title><style>p{color:red}</style>
            <script>alert("x")</script></head><body><h2>Plan &amp; scope</h2>
            <p>Ship in <b>May</b>&nbsp;2026.</p><!-- hidden --><ul><li>One</li><li>Two &#x2014; three</li></ul></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("## Plan & scope"), "{text}");
        assert!(text.contains("Ship in May 2026."));
        assert!(text.contains("Two — three"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn docx_paragraphs_headings_and_tables() {
        let document = br#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Project Atlas</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">The budget is </w:t></w:r><w:r><w:t>$40,000 &amp; rising.</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Owner</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Dana</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#;
        let docx = zip_with(&[("word/document.xml", document)]);
        let text = to_text(Format::Docx, &docx).unwrap();
        assert!(text.contains("# Project Atlas"), "{text}");
        assert!(text.contains("The budget is $40,000 & rising."), "{text}");
        assert!(text.contains("Owner"));
        assert!(text.contains("Dana"));
    }

    #[test]
    fn damaged_office_files_are_refused_with_a_reason() {
        assert!(to_text(Format::Docx, b"not a zip").is_err());
        let no_body = zip_with(&[("word/other.xml", b"<x/>")]);
        assert_eq!(
            to_text(Format::Docx, &no_body).unwrap_err(),
            "this Word file has no document body"
        );
    }

    #[test]
    fn zip_bombs_are_not_opened() {
        let zeros = vec![0u8; 8 * 1024 * 1024];
        let bomb = zip_with(&[("word/document.xml", &zeros)]);
        let err = to_text(Format::Docx, &bomb).unwrap_err();
        assert!(err.contains("expands"), "{err}");
    }

    #[test]
    fn spreadsheet_sheets_become_tables() {
        let ct: &[u8] = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
        let rels: &[u8] = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let wb: &[u8] = br#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let wb_rels: &[u8] = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let sheet: &[u8] = br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Cost</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>Hosting</t></is></c><c r="B2"><v>1200</v></c></row>
</sheetData></worksheet>"#;
        let xlsx = zip_with(&[
            ("[Content_Types].xml", ct),
            ("_rels/.rels", rels),
            ("xl/workbook.xml", wb),
            ("xl/_rels/workbook.xml.rels", wb_rels),
            ("xl/worksheets/sheet1.xml", sheet),
        ]);
        let text = to_text(Format::Spreadsheet, &xlsx).unwrap();
        assert!(text.starts_with("## Budget"), "{text}");
        assert!(text.contains("Item | Cost"), "{text}");
        assert!(text.contains("Hosting | 1200"), "{text}");
    }

    #[test]
    fn long_text_is_capped_on_a_char_boundary() {
        let long = "é".repeat(MAX_TEXT_BYTES);
        let text = to_text(Format::Plain, long.as_bytes()).unwrap();
        assert!(text.len() < MAX_TEXT_BYTES + 200);
        assert!(text.ends_with("longer than 20 MB of text.]\n"));
    }
}
