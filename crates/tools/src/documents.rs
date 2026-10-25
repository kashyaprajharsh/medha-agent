//! Text out of Word, Excel, PowerPoint and PDF files, which `read` would otherwise refuse as binary.

use quick_xml::events::{BytesStart, Event};
use std::{
    collections::{BTreeMap, HashMap},
    io::{Cursor, Read},
    path::Path,
};

/// Larger documents are refused rather than unpacked whole into memory.
pub(crate) const MAX_DOCUMENT_BYTES: usize = 50 * 1024 * 1024;
/// One unpacked part may not exceed this, so a small archive cannot inflate without bound.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SHEET_ROWS: usize = 5_000;
const MAX_SHEET_COLUMNS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Word,
    Excel,
    PowerPoint,
    Pdf,
}

impl Kind {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "docx" | "docm" => Some(Self::Word),
            "xlsx" | "xlsm" => Some(Self::Excel),
            "pptx" | "pptm" => Some(Self::PowerPoint),
            "pdf" => Some(Self::Pdf),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Word => "Word document",
            Self::Excel => "Excel workbook",
            Self::PowerPoint => "PowerPoint presentation",
            Self::Pdf => "PDF",
        }
    }
}

/// The document's text, one paragraph, row or slide line per line.
pub(crate) fn extract(kind: Kind, bytes: &[u8]) -> Result<String, String> {
    let text = match kind {
        Kind::Word => word(bytes)?,
        Kind::Excel => excel(bytes)?,
        Kind::PowerPoint => slides(bytes)?,
        Kind::Pdf => pdf_extract::extract_text_from_mem(bytes)
            .map_err(|error| format!("its text could not be extracted: {error}"))?,
    };
    if text.trim().is_empty() {
        return Err(match kind {
            Kind::Pdf => "it has no text layer (likely scanned pages)".into(),
            _ => "it contains no text".into(),
        });
    }
    Ok(text)
}

/// What a binary file is, from its first bytes, so the model knows without hunting for a tool.
pub(crate) fn describe_binary(sample: &[u8], size: u64) -> String {
    format!("{}, {}", binary_kind(sample), human_size(size))
}

/// Text never holds a NUL byte, and these formats announce themselves in their first bytes.
fn looks_binary(sample: &[u8]) -> bool {
    utf16(sample).is_none() && (sample.contains(&0) || binary_kind(sample) != UNKNOWN_BINARY)
}

/// Text saved as UTF-16 with its byte-order mark, as Windows PowerShell 5 writes `>` and `Out-File`.
pub(crate) fn utf16(bytes: &[u8]) -> Option<String> {
    let (rest, little) = match bytes {
        [0xff, 0xfe, rest @ ..] => (rest, true),
        [0xfe, 0xff, rest @ ..] => (rest, false),
        _ => return None,
    };
    let units = rest.chunks_exact(2).map(|pair| match little {
        true => u16::from_le_bytes([pair[0], pair[1]]),
        false => u16::from_be_bytes([pair[0], pair[1]]),
    });
    Some(
        char::decode_utf16(units)
            .map(|c| c.unwrap_or('\u{fffd}'))
            .collect(),
    )
}

const UNKNOWN_BINARY: &str = "unknown binary data";

/// Formats that are binary whatever their first bytes say.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tif", "tiff", "psd", "mp4", "mov", "avi",
    "mkv", "webm", "mp3", "wav", "ogg", "flac", "aac", "m4a", "zip", "tar", "gz", "tgz", "bz2",
    "xz", "7z", "rar", "iso", "exe", "dll", "so", "dylib", "bin", "o", "a", "lib", "obj", "msi",
    "class", "jar", "pyc", "wasm", "ttf", "otf", "woff", "woff2", "db", "sqlite", "sqlite3",
];

fn binary_kind(sample: &[u8]) -> &'static str {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "PNG image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"GIF8", "GIF image"),
        (b"%PDF-", "PDF document"),
        (
            b"PK\x03\x04",
            "ZIP archive (also .jar, .apk and office files)",
        ),
        (b"PK\x05\x06", "empty ZIP archive"),
        (b"\x1f\x8b", "gzip data"),
        (b"BZh", "bzip2 data"),
        (b"\xfd7zXZ\x00", "xz data"),
        (b"7z\xbc\xaf\x27\x1c", "7-Zip archive"),
        (b"Rar!", "RAR archive"),
        (b"\x7fELF", "ELF executable"),
        (b"MZ", "Windows executable"),
        (b"\xcf\xfa\xed\xfe", "Mach-O executable"),
        (b"\xca\xfe\xba\xbe", "Mach-O universal binary or Java class"),
        (b"SQLite format 3\x00", "SQLite database"),
        (
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
            "legacy Office document (.doc, .xls, .ppt)",
        ),
        (b"RIFF", "RIFF media (WAV, AVI or WebP)"),
        (b"OggS", "Ogg media"),
        (b"fLaC", "FLAC audio"),
        (b"ID3", "MP3 audio"),
        (b"\x00asm", "WebAssembly module"),
    ];
    SIGNATURES
        .iter()
        .find(|(prefix, _)| sample.starts_with(prefix))
        .map(|(_, name)| *name)
        .or_else(|| (sample.get(4..8) == Some(b"ftyp")).then_some("MP4 or MOV media"))
        .unwrap_or(UNKNOWN_BINARY)
}

/// Why plain text must not be written here; `existing` is the start of the file when it already exists.
pub(crate) fn text_write_refusal(path: &Path, existing: Option<&[u8]>) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    // A SQLite journal holds raw database pages that the next open replays; text there corrupts the database.
    if [".db", ".sqlite", ".sqlite3"].iter().any(|database| {
        ["-wal", "-shm", "-journal"]
            .iter()
            .any(|side| name.ends_with(&format!("{database}{side}")))
    }) {
        return Some(format!(
            "{} is a SQLite journal: text written there corrupts its database. Change the \
             database with the sqlite3 command or a SQLite library.",
            path.display()
        ));
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if let Some(sample) = existing
        && !matches!(extension.as_str(), "pdf" | "rtf")
        && (BINARY_EXTENSIONS.contains(&extension.as_str()) || looks_binary(sample))
    {
        return Some(format!(
            "{} is an existing binary file ({}): writing text over it would destroy it. Change \
             it with a command or a tool made for its format; a new file with this extension \
             can still be written.",
            path.display(),
            binary_kind(sample)
        ));
    }
    let exists = existing.is_some();
    let packed = matches!(
        extension.as_str(),
        "docx"
            | "docm"
            | "dotx"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "pptx"
            | "pptm"
            | "ppsx"
            | "potx"
            | "doc"
            | "xls"
            | "ppt"
            | "odt"
            | "ods"
            | "odp"
            | "epub"
    );
    // A new PDF or RTF can be written as its own syntax; replacing an existing one with extracted text cannot.
    let syntax = matches!(extension.as_str(), "pdf" | "rtf");
    (packed || (syntax && exists)).then(|| {
        format!(
            "{} is a .{extension} document: read showed its extracted text, not the file, and \
             writing text over it would destroy it. Change it with a command and a library made \
             for the format (python-docx, openpyxl, python-pptx), or put the text in a new .md \
             or .txt file.",
            path.display()
        )
    })
}

pub(crate) fn human_size(size: u64) -> String {
    match size {
        size if size >= 1024 * 1024 => format!("{:.1} MB", size as f64 / (1024.0 * 1024.0)),
        size if size >= 1024 => format!("{:.1} KB", size as f64 / 1024.0),
        size => format!("{size} bytes"),
    }
}

type Archive<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

fn open(bytes: &[u8], kind: Kind) -> Result<Archive<'_>, String> {
    zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("it is not a valid {}: {error}", kind.label()))
}

/// One part of the package, or `None` when the package does not have it.
fn part(archive: &mut Archive<'_>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(format!("{name} could not be opened: {error}")),
    };
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_PART_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name} could not be unpacked: {error}"))?;
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(format!("{name} unpacks to more than 64 MB"));
    }
    Ok(Some(bytes))
}

enum Node<'a> {
    Open(&'a BytesStart<'a>),
    Close(&'a [u8]),
    Text(&'a str),
}

/// One pass over a part: elements by local name, and text with its entities resolved.
fn walk(xml: &[u8], mut visit: impl FnMut(Node<'_>)) -> Result<(), String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buffer = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("malformed XML: {error}"))?;
        match event {
            Event::Start(start) => visit(Node::Open(&start)),
            Event::Empty(start) => {
                visit(Node::Open(&start));
                visit(Node::Close(start.local_name().as_ref()));
            }
            Event::End(end) => visit(Node::Close(end.local_name().as_ref())),
            Event::Text(text) => {
                let text = text.decode().map_err(|error| error.to_string())?;
                visit(Node::Text(&text));
            }
            Event::CData(text) => {
                let text = text.decode().map_err(|error| error.to_string())?;
                visit(Node::Text(&text));
            }
            Event::GeneralRef(reference) => {
                let text = match reference.resolve_char_ref() {
                    Ok(Some(character)) => character.to_string(),
                    _ => match reference
                        .decode()
                        .map_err(|error| error.to_string())?
                        .as_ref()
                    {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        _ => String::new(),
                    },
                };
                visit(Node::Text(&text));
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
        buffer.clear();
    }
}

fn local(start: &BytesStart<'_>) -> Vec<u8> {
    start.local_name().as_ref().to_vec()
}

/// An attribute by its exact name, or for `r:id` by any prefix, since packages vary the prefix.
fn attribute(start: &BytesStart<'_>, name: &str) -> Option<String> {
    start.attributes().flatten().find_map(|found| {
        let key = found.key.as_ref();
        let matches = match name.strip_prefix("r:") {
            Some(local) => key.ends_with(format!(":{local}").as_bytes()),
            None => key == name.as_bytes(),
        };
        matches.then(|| String::from_utf8_lossy(&found.value).into_owned())
    })
}

/// Relationship id to package path, resolved against the folder the relationships belong to.
fn relationships(xml: &[u8], folder: &str) -> Result<HashMap<String, String>, String> {
    let mut targets = HashMap::new();
    walk(xml, |node| {
        if let Node::Open(start) = node
            && local(start) == b"Relationship"
            && let (Some(id), Some(target)) = (attribute(start, "Id"), attribute(start, "Target"))
        {
            targets.insert(id, joined(folder, &target));
        }
    })?;
    Ok(targets)
}

fn joined(folder: &str, target: &str) -> String {
    let mut parts: Vec<&str> = match target.strip_prefix('/') {
        Some(_) => Vec::new(),
        None => folder.split('/').filter(|part| !part.is_empty()).collect(),
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn word(bytes: &[u8]) -> Result<String, String> {
    let mut archive = open(bytes, Kind::Word)?;
    let xml = part(&mut archive, "word/document.xml")?
        .ok_or("it has no word/document.xml, so it is not a Word document")?;
    let mut out = String::new();
    // Tabs and breaks count only inside a run: `w:tab` also names tab stops in paragraph settings.
    let (mut run, mut text, mut guide) = (0usize, false, 0usize);
    walk(&xml, |node| match node {
        Node::Open(start) => match local(start).as_slice() {
            b"r" => run += 1,
            b"t" => text = true,
            // A ruby guide annotates the text above it; it is not part of the sentence.
            b"rt" => guide += 1,
            b"tab" if run > 0 && guide == 0 => out.push('\t'),
            b"br" | b"cr" if run > 0 && guide == 0 => out.push('\n'),
            _ => {}
        },
        Node::Close(name) => match name {
            b"r" => run = run.saturating_sub(1),
            b"t" => text = false,
            b"rt" => guide = guide.saturating_sub(1),
            b"p" => out.push('\n'),
            _ => {}
        },
        Node::Text(value) if text && guide == 0 => out.push_str(value),
        Node::Text(_) => {}
    })?;
    Ok(out)
}

fn excel(bytes: &[u8]) -> Result<String, String> {
    let mut archive = open(bytes, Kind::Excel)?;
    let shared = match part(&mut archive, "xl/sharedStrings.xml")? {
        Some(xml) => shared_strings(&xml)?,
        None => Vec::new(),
    };
    let relationships = match part(&mut archive, "xl/_rels/workbook.xml.rels")? {
        Some(xml) => relationships(&xml, "xl")?,
        None => HashMap::new(),
    };
    let workbook = part(&mut archive, "xl/workbook.xml")?
        .ok_or("it has no xl/workbook.xml, so it is not an Excel workbook")?;
    let mut sheets = Vec::new();
    walk(&workbook, |node| {
        if let Node::Open(start) = node
            && local(start) == b"sheet"
        {
            sheets.push((
                attribute(start, "name").unwrap_or_else(|| "Sheet".into()),
                attribute(start, "state").unwrap_or_default(),
                attribute(start, "r:id").and_then(|id| relationships.get(&id).cloned()),
            ));
        }
    })?;
    let (mut out, mut hidden) = (String::new(), Vec::new());
    for (name, state, target) in sheets {
        if matches!(state.as_str(), "hidden" | "veryHidden") {
            hidden.push(name);
            continue;
        }
        let Some(xml) = target
            .map(|target| part(&mut archive, &target))
            .transpose()?
            .flatten()
        else {
            continue;
        };
        let (rows, cut) = sheet_rows(&xml, &shared)?;
        out.push_str(&format!("# Sheet: {name}\n"));
        if rows.is_empty() {
            out.push_str("(empty)\n");
        }
        for row in rows {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        if cut > 0 {
            out.push_str(&format!("(… {cut} more rows not shown)\n"));
        }
        out.push('\n');
    }
    if !hidden.is_empty() {
        out.push_str(&format!("Hidden sheets not shown: {}\n", hidden.join(", ")));
    }
    Ok(out)
}

fn shared_strings(xml: &[u8]) -> Result<Vec<String>, String> {
    let (mut strings, mut current) = (Vec::new(), String::new());
    let (mut text, mut phonetic) = (false, 0usize);
    walk(xml, |node| match node {
        Node::Open(start) => match local(start).as_slice() {
            b"si" => current.clear(),
            b"t" => text = true,
            // Phonetic readings annotate a string; they are not part of it.
            b"rPh" => phonetic += 1,
            _ => {}
        },
        Node::Close(name) => match name {
            b"si" => strings.push(std::mem::take(&mut current)),
            b"t" => text = false,
            b"rPh" => phonetic = phonetic.saturating_sub(1),
            _ => {}
        },
        Node::Text(value) if text && phonetic == 0 => current.push_str(value),
        Node::Text(_) => {}
    })?;
    Ok(strings)
}

/// A sheet's rows as cell text, at most [`MAX_SHEET_ROWS`], with how many more there were.
fn sheet_rows(xml: &[u8], shared: &[String]) -> Result<(Vec<Vec<String>>, usize), String> {
    let (mut rows, mut cut) = (Vec::new(), 0usize);
    let mut row: BTreeMap<usize, String> = BTreeMap::new();
    let (mut next, mut column, mut kind) = (0usize, 0usize, String::new());
    let (mut value, mut in_value, mut in_text, mut phonetic) =
        (String::new(), false, false, 0usize);
    walk(xml, |node| match node {
        Node::Open(start) => match local(start).as_slice() {
            b"row" => {
                row.clear();
                next = 0;
            }
            b"c" => {
                column = attribute(start, "r").map_or(next, |reference| column_index(&reference));
                kind = attribute(start, "t").unwrap_or_default();
                value.clear();
            }
            b"v" => in_value = true,
            b"t" => in_text = true,
            b"rPh" => phonetic += 1,
            _ => {}
        },
        Node::Close(name) => match name {
            b"v" => in_value = false,
            b"t" => in_text = false,
            b"rPh" => phonetic = phonetic.saturating_sub(1),
            b"c" => {
                let text = match kind.as_str() {
                    "s" => value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| shared.get(index).cloned())
                        .unwrap_or_default(),
                    "b" => match value.trim() {
                        "1" | "true" | "TRUE" => "TRUE".into(),
                        _ => "FALSE".into(),
                    },
                    "e" if value.is_empty() => "#ERROR".into(),
                    _ => value.clone(),
                };
                if column < MAX_SHEET_COLUMNS {
                    row.insert(column, text);
                }
                next = column + 1;
            }
            b"row" if rows.len() < MAX_SHEET_ROWS => {
                let width = row.keys().next_back().map_or(0, |last| last + 1);
                rows.push(
                    (0..width)
                        .map(|at| row.remove(&at).unwrap_or_default())
                        .collect(),
                );
            }
            b"row" => cut += 1,
            _ => {}
        },
        Node::Text(text) if in_value || (in_text && phonetic == 0) => value.push_str(text),
        Node::Text(_) => {}
    })?;
    while rows
        .last()
        .is_some_and(|row: &Vec<String>| row.iter().all(|cell| cell.trim().is_empty()))
    {
        rows.pop();
    }
    Ok((rows, cut))
}

/// The 0-based column of a cell reference: `A1` is 0, `AB7` is 27.
fn column_index(reference: &str) -> usize {
    reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .fold(0usize, |index, letter| {
            index * 26 + usize::from(letter.to_ascii_uppercase() - b'A') + 1
        })
        .saturating_sub(1)
}

fn slides(bytes: &[u8]) -> Result<String, String> {
    let mut archive = open(bytes, Kind::PowerPoint)?;
    let relationships = match part(&mut archive, "ppt/_rels/presentation.xml.rels")? {
        Some(xml) => relationships(&xml, "ppt")?,
        None => HashMap::new(),
    };
    let presentation = part(&mut archive, "ppt/presentation.xml")?
        .ok_or("it has no ppt/presentation.xml, so it is not a PowerPoint presentation")?;
    // Slides in the order the deck shows them, which is not the order of their file names.
    let mut order = Vec::new();
    walk(&presentation, |node| {
        if let Node::Open(start) = node
            && local(start) == b"sldId"
            && let Some(target) = attribute(start, "r:id").and_then(|id| relationships.get(&id))
        {
            order.push(target.clone());
        }
    })?;
    let mut out = String::new();
    for (number, target) in order.iter().enumerate() {
        let Some(xml) = part(&mut archive, target)? else {
            continue;
        };
        out.push_str(&format!("# Slide {}\n", number + 1));
        let (mut paragraph, mut text) = (String::new(), false);
        walk(&xml, |node| match node {
            Node::Open(start) => match local(start).as_slice() {
                b"t" => text = true,
                b"br" => paragraph.push('\n'),
                _ => {}
            },
            Node::Close(name) => match name {
                b"t" => text = false,
                b"p" if !paragraph.trim().is_empty() => {
                    out.push_str(paragraph.trim_end());
                    out.push('\n');
                    paragraph.clear();
                }
                b"p" => paragraph.clear(),
                _ => {}
            },
            Node::Text(value) if text => paragraph.push_str(value),
            Node::Text(_) => {}
        })?;
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[path = "documents_tests.rs"]
mod tests;
