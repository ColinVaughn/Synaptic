//! Office-document ingestion (feature `office`). Reads spreadsheets with the
//! pure-Rust `calamine` crate (no external tools) and renders them to markdown
//! that the normal extraction pass picks up ("shape A"). Covers the xlsx subset.

use std::path::Path;

#[cfg(feature = "office")]
use calamine::{open_workbook_auto, Data, Reader};

/// Render an `.xlsx`/`.xls`/`.ods` workbook to markdown: a `## Sheet` heading per
/// worksheet followed by ` | `-joined non-empty rows. Returns an error string on
/// an unreadable workbook.
pub fn xlsx_to_markdown(path: &Path) -> Result<String, String> {
    let mut wb =
        open_workbook_auto(path).map_err(|e| format!("opening {}: {e}", path.display()))?;
    let mut out = String::new();
    for name in wb.sheet_names() {
        let Ok(range) = wb.worksheet_range(&name) else {
            continue;
        };
        out.push_str(&format!("## {name}\n\n"));
        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(cell_to_string).collect();
            if cells.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(feature = "office")]
fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            // Render whole floats without a trailing `.0` (xlsx stores ints as floats).
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::DateTime(d) => d.to_string(),
        Data::Error(_) => String::new(),
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
}
