//! The general journal.

/// One double-entry posting.
pub struct Posting {
    pub account: String,
    pub debit_cents: i64,
    pub credit_cents: i64,
}

/// Checks that debits equal credits for one journal entry.
pub fn is_balanced(postings: &[Posting]) -> bool {
    let debit: i64 = postings.iter().map(|p| p.debit_cents).sum();
    let credit: i64 = postings.iter().map(|p| p.credit_cents).sum();
    debit == credit
}

/// Closes a period by moving income balances to retained earnings.
pub fn close_period(postings: &mut Vec<Posting>, retained: &str) {
    let income: i64 = postings
        .iter()
        .filter(|p| p.account.starts_with("income:"))
        .map(|p| p.credit_cents - p.debit_cents)
        .sum();
    postings.push(Posting {
        account: retained.to_owned(),
        debit_cents: 0,
        credit_cents: income,
    });
}
