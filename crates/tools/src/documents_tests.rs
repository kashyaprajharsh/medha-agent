use super::*;
use std::io::Write;

fn package(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in parts {
        writer.start_file(*name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

#[test]
fn word_keeps_paragraphs_runs_and_entities_but_not_tab_stops_or_ruby_guides() {
    let body = format!(
        r#"<w:document {W}><w:body>
        <w:p><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr>
          <w:r><w:t>Name</w:t></w:r><w:r><w:tab/><w:t>Tom &amp; Jerry</w:t></w:r></w:p>
        <w:p><w:r><w:ruby><w:rt><w:r><w:t>guide</w:t></w:r></w:rt><w:rubyBase><w:r><w:t>漢字</w:t></w:r></w:rubyBase></w:ruby></w:r></w:p>
        <w:p><w:r><w:t>line one</w:t><w:br/><w:t>line two</w:t></w:r></w:p>
        </w:body></w:document>"#
    );
    let text = extract(Kind::Word, &package(&[("word/document.xml", &body)])).unwrap();
    assert_eq!(text, "Name\tTom & Jerry\n漢字\nline one\nline two\n");
}

#[test]
fn excel_follows_the_workbook_and_names_what_it_leaves_out() {
    let workbook = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>
        <sheet name="Sales" sheetId="2" r:id="rId2"/>
        <sheet name="Secret" sheetId="3" state="hidden" r:id="rId3"/>
        <sheet name="Notes" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let rels = r#"<Relationships>
        <Relationship Id="rId1" Target="worksheets/sheet1.xml"/>
        <Relationship Id="rId2" Target="/xl/worksheets/sheet2.xml"/>
        <Relationship Id="rId3" Target="worksheets/sheet3.xml"/></Relationships>"#;
    let shared = r#"<sst><si><t>Region</t></si><si><r><t>No</t></r><r><t>rth</t></r><rPh><t>のーす</t></rPh></si></sst>"#;
    let sales = r#"<worksheet><sheetData>
        <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="inlineStr"><is><t>Total</t></is></c></row>
        <row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2" t="b"><v>1</v></c><c r="C2"><v>42.5</v></c></row>
        <row r="3"><c r="A3"/></row></sheetData></worksheet>"#;
    let notes = r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>ok</t></is></c></row></sheetData></worksheet>"#;
    let file = package(&[
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
        ("xl/sharedStrings.xml", shared),
        ("xl/worksheets/sheet2.xml", sales),
        ("xl/worksheets/sheet1.xml", notes),
        ("xl/worksheets/sheet3.xml", notes),
    ]);
    let text = extract(Kind::Excel, &file).unwrap();
    assert_eq!(
        text,
        "# Sheet: Sales\nRegion\t\tTotal\nNorth\tTRUE\t42.5\n\n# Sheet: Notes\nok\n\n\
         Hidden sheets not shown: Secret\n"
    );
}

#[test]
fn slides_come_in_the_order_the_deck_shows_them() {
    let presentation = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>
        <p:sldId id="256" r:id="rId9"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#;
    let rels = r#"<Relationships>
        <Relationship Id="rId2" Target="slides/slide1.xml"/>
        <Relationship Id="rId9" Target="slides/slide2.xml"/></Relationships>"#;
    let slide = |words: &str| {
        format!(
            r#"<p:sld xmlns:p="p" xmlns:a="a"><p:txBody><a:p><a:r><a:t>{words}</a:t></a:r></a:p><a:p></a:p></p:txBody></p:sld>"#
        )
    };
    let (opening, closing) = (slide("Opening &lt;title&gt;"), slide("Thanks"));
    let file = package(&[
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/slides/slide1.xml", &closing),
        ("ppt/slides/slide2.xml", &opening),
    ]);
    let text = extract(Kind::PowerPoint, &file).unwrap();
    assert_eq!(text, "# Slide 1\nOpening <title>\n\n# Slide 2\nThanks\n\n");
}

#[test]
fn a_file_that_only_claims_the_format_says_so() {
    let error = extract(Kind::Word, b"plain text named report.docx").unwrap_err();
    assert!(error.contains("not a valid Word document"), "{error}");
    let empty = format!("<w:document {W}><w:body><w:p/></w:body></w:document>");
    let error = extract(Kind::Word, &package(&[("word/document.xml", &empty)])).unwrap_err();
    assert!(error.contains("no text"), "{error}");
}

#[test]
fn a_binary_file_is_named_by_its_bytes_not_its_extension() {
    assert_eq!(
        describe_binary(b"\x89PNG\r\n\x1a\nrest", 2_048),
        "PNG image, 2.0 KB"
    );
    assert_eq!(
        describe_binary(b"\0\0\0\x18ftypisom", 3 * 1024 * 1024),
        "MP4 or MOV media, 3.0 MB"
    );
    assert_eq!(
        describe_binary(b"\x01\x02", 9),
        "unknown binary data, 9 bytes"
    );
}

#[test]
fn text_never_overwrites_a_binary_file_but_new_files_and_utf16_text_stay_writable() {
    let refuse =
        |name: &str, existing: Option<&[u8]>| text_write_refusal(Path::new(name), existing);
    // Its bytes decide, not its name: a PNG saved as notes.txt is still a PNG.
    let refused = refuse("notes.txt", Some(b"\x89PNG\r\n\x1a\n...")).unwrap();
    assert!(
        refused.contains("existing binary file (PNG image)"),
        "{refused}"
    );
    assert!(refuse("logo.png", Some(b"plain looking")).is_some());
    assert!(refuse("data.bin", Some(b"text\0with a NUL")).is_some());
    assert!(
        refuse("app.db-wal", None)
            .unwrap()
            .contains("SQLite journal")
    );
    assert!(refuse("report.docx", None).is_some());
    assert!(refuse("old.pdf", Some(b"%PDF-1.7")).is_some());
    // A new file of any of these kinds can be written, and existing text stays editable.
    for name in ["logo.png", "new.pdf", "letter.rtf", "data.db"] {
        assert!(refuse(name, None).is_none(), "{name}");
    }
    assert!(refuse("notes.txt", Some(b"plain text")).is_none());
    let powershell_out: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain("ok\r\n".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert!(
        refuse("out.txt", Some(&powershell_out)).is_none(),
        "UTF-16 text is text"
    );
    assert_eq!(utf16(&powershell_out).as_deref(), Some("ok\r\n"));
}
