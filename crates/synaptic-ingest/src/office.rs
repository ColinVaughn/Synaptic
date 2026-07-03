//! Office-document ingestion (feature `office`). Reads spreadsheets in-house
//! with `zip` + `roxmltree` (both already in the workspace; no XML-streaming
//! deps) and renders them to markdown that the normal extraction pass picks up
//! ("shape A"). Covers .xlsx and .ods; legacy binary .xls is not a zip
//! container and is rejected with a convert-to-xlsx error.

use std::io::Read;
use std::path::Path;

/// One part of an office archive may legitimately be large (sharedStrings on a
/// big workbook), but an unbounded inflate is a zip-bomb vector on a file the
/// user was handed. Cap what we are willing to inflate per part.
const MAX_PART_BYTES: u64 = 256 << 20;

/// Cap for ODS `number-columns/rows-repeated`: producers pad sheets with cells
/// repeated to the 16k column limit, and a hostile file can claim millions.
const MAX_REPEAT: usize = 4096;

type Sheet = (String, Vec<Vec<String>>);

/// Render an `.xlsx`/`.ods` workbook to markdown: a `## Sheet` heading per
/// worksheet followed by ` | `-joined non-empty rows. Legacy binary `.xls` is
/// rejected. Returns an error string on an unreadable workbook.
pub fn xlsx_to_markdown(path: &Path) -> Result<String, String> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("xls"))
    {
        return Err(format!(
            "{}: legacy binary .xls is not supported; convert it to .xlsx",
            path.display()
        ));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("opening {}: {e}", path.display()))?;

    let sheets = if zip.file_names().any(|n| n == "xl/workbook.xml") {
        read_xlsx(&mut zip)?
    } else if zip.file_names().any(|n| n == "content.xml") {
        read_ods(&mut zip)?
    } else {
        return Err(format!("{}: not an xlsx or ods archive", path.display()));
    };
    Ok(render(&sheets))
}

fn render(sheets: &[Sheet]) -> String {
    let mut out = String::new();
    for (name, rows) in sheets {
        out.push_str(&format!("## {name}\n\n"));
        for row in rows {
            if row.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            out.push_str(&row.join(" | "));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn read_part<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let mut f = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    if f.size() > MAX_PART_BYTES {
        return Err(format!("{name}: part too large ({} bytes)", f.size()));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(|e| format!("{name}: {e}"))?;
    Ok(s)
}

/// Namespace-agnostic attribute lookup (`r:id` and `table:name` carry
/// namespaces that vary by producer; the local name is the stable part).
fn attr<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

/// Render a numeric cell the way spreadsheets display it: xlsx stores ints as
/// floats, so whole values drop the trailing `.0`.
fn fmt_number(raw: &str) -> String {
    match raw.trim().parse::<f64>() {
        Ok(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
        Ok(f) => f.to_string(),
        Err(_) => raw.to_string(),
    }
}

/// Drop the all-empty leading columns shared by every non-empty row, so a
/// sheet whose data starts at column C doesn't render leading ` | ` noise.
fn strip_leading_empty_cols(rows: &mut [Vec<String>]) {
    let lead = rows
        .iter()
        .filter(|r| r.iter().any(|c| !c.trim().is_empty()))
        .map(|r| r.iter().take_while(|c| c.trim().is_empty()).count())
        .min()
        .unwrap_or(0);
    if lead == 0 {
        return;
    }
    for r in rows.iter_mut() {
        if r.len() >= lead {
            r.drain(..lead);
        } else {
            r.clear();
        }
    }
}

fn trim_trailing_empty(cells: &mut Vec<String>) {
    while cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
}

// --- xlsx ---

fn read_xlsx<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>) -> Result<Vec<Sheet>, String> {
    let wb_xml = read_part(zip, "xl/workbook.xml")?;
    let wb = roxmltree::Document::parse(&wb_xml).map_err(|e| format!("xl/workbook.xml: {e}"))?;
    let sheet_refs: Vec<(String, Option<String>)> = wb
        .descendants()
        .filter(|n| n.tag_name().name() == "sheet")
        .map(|n| {
            (
                attr(n, "name").unwrap_or("Sheet").to_string(),
                attr(n, "id").map(str::to_string),
            )
        })
        .collect();

    // rId -> part path, from the workbook relationships (absent in minimal
    // producers; fall back to the conventional worksheets/sheetN.xml layout).
    let mut rel_targets = std::collections::HashMap::new();
    if zip.file_names().any(|n| n == "xl/_rels/workbook.xml.rels") {
        let rels_xml = read_part(zip, "xl/_rels/workbook.xml.rels")?;
        if let Ok(rels) = roxmltree::Document::parse(&rels_xml) {
            for r in rels
                .descendants()
                .filter(|n| n.tag_name().name() == "Relationship")
            {
                if let (Some(id), Some(target)) = (attr(r, "Id"), attr(r, "Target")) {
                    let t = target.trim_start_matches('/');
                    let part = if t.starts_with("xl/") {
                        t.to_string()
                    } else {
                        format!("xl/{t}")
                    };
                    rel_targets.insert(id.to_string(), part);
                }
            }
        }
    }

    let shared = read_shared_strings(zip)?;

    let mut sheets = Vec::new();
    for (i, (name, rid)) in sheet_refs.iter().enumerate() {
        let part = rid
            .as_deref()
            .and_then(|id| rel_targets.get(id).cloned())
            .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", i + 1));
        let Ok(xml) = read_part(zip, &part) else {
            continue;
        };
        let Ok(doc) = roxmltree::Document::parse(&xml) else {
            continue;
        };
        let mut rows = Vec::new();
        for row in doc.descendants().filter(|n| n.tag_name().name() == "row") {
            let mut cells: Vec<String> = Vec::new();
            let mut next_col = 0usize;
            for c in row.children().filter(|n| n.tag_name().name() == "c") {
                let col = attr(c, "r").and_then(col_index).unwrap_or(next_col);
                next_col = col + 1;
                if col >= cells.len() {
                    cells.resize(col, String::new());
                    cells.push(cell_text(c, &shared));
                } else {
                    cells[col] = cell_text(c, &shared);
                }
            }
            trim_trailing_empty(&mut cells);
            rows.push(cells);
        }
        strip_leading_empty_cols(&mut rows);
        sheets.push((name.clone(), rows));
    }
    Ok(sheets)
}

fn read_shared_strings<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<Vec<String>, String> {
    if !zip.file_names().any(|n| n == "xl/sharedStrings.xml") {
        return Ok(Vec::new());
    }
    let xml = read_part(zip, "xl/sharedStrings.xml")?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("xl/sharedStrings.xml: {e}"))?;
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "si")
        .map(|si| {
            si.descendants()
                .filter(|d| d.tag_name().name() == "t")
                // skip phonetic (furigana) runs; they duplicate the base text
                .filter(|d| d.ancestors().all(|a| a.tag_name().name() != "rPh"))
                .filter_map(|t| t.text())
                .collect::<String>()
        })
        .collect())
}

/// `"BC12"` -> zero-based column index of `BC`.
fn col_index(cell_ref: &str) -> Option<usize> {
    let letters: String = cell_ref
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut idx = 0usize;
    for ch in letters.chars() {
        idx = idx * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    Some(idx - 1)
}

fn cell_text(c: roxmltree::Node, shared: &[String]) -> String {
    let v = || {
        c.children()
            .find(|n| n.tag_name().name() == "v")
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string()
    };
    match attr(c, "t").unwrap_or("n") {
        "s" => v()
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|i| shared.get(i).cloned())
            .unwrap_or_default(),
        "b" => (v().trim() == "1").to_string(),
        "e" => String::new(),
        "str" => v(),
        "inlineStr" => c
            .descendants()
            .filter(|d| d.tag_name().name() == "t")
            .filter_map(|t| t.text())
            .collect(),
        _ => fmt_number(&v()),
    }
}

// --- ods ---

fn read_ods<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>) -> Result<Vec<Sheet>, String> {
    let xml = read_part(zip, "content.xml")?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("content.xml: {e}"))?;
    let mut sheets = Vec::new();
    for table in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "table" && n.tag_name().namespace().is_some())
    {
        let name = attr(table, "name").unwrap_or("Sheet").to_string();
        let mut rows = Vec::new();
        for row in table
            .descendants()
            .filter(|n| n.tag_name().name() == "table-row")
        {
            let mut cells: Vec<String> = Vec::new();
            for cell in row
                .children()
                .filter(|n| matches!(n.tag_name().name(), "table-cell" | "covered-table-cell"))
            {
                let repeat = attr(cell, "number-columns-repeated")
                    .and_then(|r| r.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, MAX_REPEAT);
                let text = ods_cell_text(cell);
                for _ in 0..repeat {
                    cells.push(text.clone());
                }
            }
            trim_trailing_empty(&mut cells);
            let repeat = attr(row, "number-rows-repeated")
                .and_then(|r| r.parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, MAX_REPEAT);
            for _ in 0..repeat {
                rows.push(cells.clone());
            }
        }
        strip_leading_empty_cols(&mut rows);
        sheets.push((name, rows));
    }
    Ok(sheets)
}

fn ods_cell_text(cell: roxmltree::Node) -> String {
    match attr(cell, "value-type") {
        Some("float") | Some("percentage") | Some("currency") => {
            attr(cell, "value").map(fmt_number).unwrap_or_default()
        }
        Some("boolean") => attr(cell, "boolean-value").unwrap_or("false").to_string(),
        Some("date") => attr(cell, "date-value").unwrap_or("").to_string(),
        Some("time") => attr(cell, "time-value").unwrap_or("").to_string(),
        _ => cell
            .descendants()
            .filter(|d| d.tag_name().name() == "p")
            .filter_map(|p| {
                let s: String = p
                    .descendants()
                    .filter_map(|t| if t.is_text() { t.text() } else { None })
                    .collect();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "office")]
    #[test]
    fn xlsx_to_markdown_renders_sheets_and_rows() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.xlsx");

        let mut wb = Workbook::new();
        let sheet = wb.add_worksheet();
        sheet.set_name("Sales").unwrap();
        sheet.write(0, 0, "Region").unwrap();
        sheet.write(0, 1, "Revenue").unwrap();
        sheet.write(1, 0, "EMEA").unwrap();
        sheet.write(1, 1, 1200).unwrap();
        wb.save(&path).unwrap();

        let md = xlsx_to_markdown(&path).expect("read xlsx");
        assert!(md.contains("## Sales"), "sheet heading: {md}");
        assert!(md.contains("Region | Revenue"), "header row: {md}");
        assert!(md.contains("EMEA | 1200"), "data row (int, no .0): {md}");
    }

    #[cfg(feature = "office")]
    #[test]
    fn xlsx_to_markdown_errors_on_missing_file() {
        assert!(xlsx_to_markdown(Path::new("/no/such/file.xlsx")).is_err());
    }

    #[cfg(feature = "office")]
    #[test]
    fn xlsx_renders_bools_floats_and_column_gaps() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.xlsx");

        let mut wb = Workbook::new();
        let sheet = wb.add_worksheet();
        sheet.write(0, 0, true).unwrap();
        sheet.write(0, 1, 1.5).unwrap();
        sheet.write(0, 2, 3.0).unwrap();
        // Sheet whose used range starts at column B, with a gap at column C.
        let sheet2 = wb.add_worksheet();
        sheet2.write(0, 1, "name").unwrap();
        sheet2.write(0, 3, "qty").unwrap();
        sheet2.write(1, 1, "apple").unwrap();
        sheet2.write(1, 3, 7).unwrap();
        wb.save(&path).unwrap();

        let md = xlsx_to_markdown(&path).expect("read xlsx");
        assert!(md.contains("true | 1.5 | 3"), "bool/float row: {md}");
        assert!(md.contains("name |  | qty"), "gap kept, lead trimmed: {md}");
        assert!(md.contains("apple |  | 7"), "gap in data row: {md}");
    }

    #[cfg(feature = "office")]
    #[test]
    fn xlsx_orders_sheets_as_in_workbook() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.xlsx");

        let mut wb = Workbook::new();
        wb.add_worksheet().set_name("Alpha").unwrap();
        wb.add_worksheet().set_name("Beta").unwrap();
        wb.save(&path).unwrap();

        let md = xlsx_to_markdown(&path).expect("read xlsx");
        let a = md.find("## Alpha").expect("Alpha heading");
        let b = md.find("## Beta").expect("Beta heading");
        assert!(a < b, "workbook sheet order preserved: {md}");
    }

    #[cfg(feature = "office")]
    #[test]
    fn ods_renders_sheets_values_and_repeated_columns() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.ods");
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
 <office:body><office:spreadsheet>
  <table:table table:name="Inventory">
   <table:table-row>
    <table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell>
    <table:table-cell office:value-type="string"><text:p>Count</text:p></table:table-cell>
   </table:table-row>
   <table:table-row>
    <table:table-cell office:value-type="string"><text:p>Bolt</text:p></table:table-cell>
    <table:table-cell office:value-type="float" office:value="250"><text:p>250</text:p></table:table-cell>
   </table:table-row>
   <table:table-row>
    <table:table-cell office:value-type="string"><text:p>Nut</text:p></table:table-cell>
    <table:table-cell table:number-columns-repeated="2"/>
    <table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p>TRUE</text:p></table:table-cell>
   </table:table-row>
  </table:table>
 </office:spreadsheet></office:body>
</office:document-content>"#;

        let file = std::fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(file);
        z.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        z.write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .unwrap();
        z.start_file("content.xml", SimpleFileOptions::default())
            .unwrap();
        z.write_all(content.as_bytes()).unwrap();
        z.finish().unwrap();

        let md = xlsx_to_markdown(&path).expect("read ods");
        assert!(md.contains("## Inventory"), "sheet heading: {md}");
        assert!(md.contains("Item | Count"), "string row: {md}");
        assert!(md.contains("Bolt | 250"), "float rendered whole: {md}");
        assert!(
            md.contains("Nut |  |  | true"),
            "repeated empty cells: {md}"
        );
    }

    #[cfg(feature = "office")]
    #[test]
    fn xls_gives_clear_unsupported_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.xls");
        std::fs::write(&path, b"\xd0\xcf\x11\xe0not-a-zip").unwrap();

        let err = xlsx_to_markdown(&path).expect_err("xls must not parse");
        assert!(
            err.contains(".xlsx"),
            "error should point at converting to .xlsx: {err}"
        );
    }

    #[cfg(feature = "office")]
    #[test]
    fn corrupt_xlsx_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.xlsx");
        std::fs::write(&path, b"this is not a zip archive").unwrap();
        assert!(xlsx_to_markdown(&path).is_err());
    }

    #[cfg(feature = "office")]
    #[test]
    fn handcrafted_xlsx_inline_strings_and_error_cells() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        // Parts a non-writer producer emits: inline strings, formula-string
        // cells (t="str"), error cells, explicit cell refs with gaps, and no
        // sharedStrings.xml at all.
        let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <sheets><sheet name="Report" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
        let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
 <sheetData>
  <row r="1">
   <c r="A1" t="inlineStr"><is><t>hello</t></is></c>
   <c r="C1" t="str"><v>world</v></c>
   <c r="D1" t="e"><v>#DIV/0!</v></c>
   <c r="E1"><v>42</v></c>
  </row>
 </sheetData>
</worksheet>"#;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crafted.xlsx");
        let file = std::fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(file);
        for (name, body) in [
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/worksheets/sheet1.xml", sheet),
        ] {
            z.start_file(name, SimpleFileOptions::default()).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();

        let md = xlsx_to_markdown(&path).expect("read crafted xlsx");
        assert!(md.contains("## Report"), "sheet heading: {md}");
        assert!(
            md.contains("hello |  | world |  | 42"),
            "inline/str/error/gap row: {md}"
        );
    }
}
