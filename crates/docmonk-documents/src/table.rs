//! Row-aware table rendering and row/cell-boundary splitting.

use crate::tokenization::count_tokens;
use crate::tokenizer::model_tokenizer;

pub type Row = Vec<String>;

const CELL_SEPARATOR: &str = " | ";

pub fn render_rows(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| r.join(CELL_SEPARATOR))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_table(rows: &[Row], caption: Option<&str>) -> String {
    let body = render_rows(rows);
    match caption.filter(|c| !c.is_empty()) {
        Some(c) if !body.is_empty() => format!("{c}\n\n{body}"),
        Some(c) => c.to_owned(),
        None => body,
    }
}

fn joined(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Groups data rows so each group, rendered under the repeated header and
/// `fixed_overhead`, fits `max_tokens`; an oversized row stays alone.
pub fn split_data_rows(
    data_rows: &[Row],
    header_rows: &[Row],
    max_tokens: i64,
    fixed_overhead: &str,
) -> Vec<Vec<Row>> {
    if data_rows.is_empty() {
        return Vec::new();
    }
    let header_text = render_rows(header_rows);
    let tokens =
        |g: &[Row]| count_tokens(&joined(&[fixed_overhead, &header_text, &render_rows(g)]));
    let mut groups = Vec::new();
    let mut current: Vec<Row> = Vec::new();
    for row in data_rows {
        let mut candidate = current.clone();
        candidate.push(row.clone());
        if !current.is_empty() && tokens(&candidate) > max_tokens {
            groups.push(std::mem::take(&mut current));
            current.push(row.clone());
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn slice(row: &[String], cols: impl Iterator<Item = usize>) -> Row {
    cols.map(|c| row.get(c).cloned().unwrap_or_default())
        .collect()
}

/// Splits one oversized row at cell boundaries (token-splitting a single
/// overflowing cell), repeating the sliced header on every segment.
pub fn segment_oversized_row(
    row: &[String],
    header_rows: &[Row],
    max_tokens: i64,
    fixed_overhead: &str,
) -> Vec<(Vec<Row>, Row)> {
    let n = row.len();
    if n == 0 {
        return vec![(Vec::new(), Vec::new())];
    }
    let header_slice =
        |a: usize, b: usize| -> Vec<Row> { header_rows.iter().map(|h| slice(h, a..b)).collect() };
    let fits = |a: usize, b: usize| {
        let hs = header_slice(a, b);
        let ds = vec![slice(row, a..b)];
        count_tokens(&joined(&[
            fixed_overhead,
            &render_rows(&hs),
            &render_rows(&ds),
        ])) <= max_tokens
    };
    let mut segments = Vec::new();
    let mut i = 0;
    while i < n {
        if !fits(i, i + 1) {
            let hs = header_slice(i, i + 1);
            let rendered = render_rows(&hs);
            let parts = joined(&[fixed_overhead, &rendered]);
            let overhead = if parts.is_empty() {
                0
            } else {
                count_tokens(&parts)
            };
            let budget = (max_tokens - overhead).max(1);
            for frag in model_tokenizer().expect("tokenizer").split(&row[i], budget) {
                segments.push((hs.clone(), vec![frag]));
            }
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && fits(i, j + 1) {
            j += 1;
        }
        segments.push((header_slice(i, j), slice(row, i..j)));
        i = j;
    }
    segments
}
