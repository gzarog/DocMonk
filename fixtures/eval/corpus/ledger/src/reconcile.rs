//! Bank statement matching for the ledger.

/// A booked ledger line.
pub struct Entry {
    pub amount_cents: i64,
    pub day: i32,
}

/// One line of a bank statement.
pub struct BankLine {
    pub amount_cents: i64,
    pub day: i32,
}

/// Pairs each bank line with the closest booked entry. Amounts may differ
/// by at most one cent and dates by at most three days.
pub fn match_bank_lines(entries: &[Entry], lines: &[BankLine]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let best = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| (e.amount_cents - line.amount_cents).abs() <= 1)
            .filter(|(_, e)| (e.day - line.day).abs() <= 3)
            .min_by_key(|(_, e)| (e.day - line.day).abs());
        if let Some((ei, _)) = best {
            out.push((ei, li));
        }
    }
    out
}

pub struct Matcher {
    pub weight_amount: f64,
}

impl Matcher {
    /// Scores a candidate pair; exact cents score double.
    pub fn score(&self, entry: &Entry, line: &BankLine) -> f64 {
        let exact = if entry.amount_cents == line.amount_cents { 2.0 } else { 1.0 };
        exact * self.weight_amount - f64::from((entry.day - line.day).abs())
    }
}
