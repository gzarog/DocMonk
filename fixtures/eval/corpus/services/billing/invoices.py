"""Invoice lifecycle helpers for the billing service."""

from datetime import date, timedelta
from decimal import Decimal, ROUND_HALF_UP

GRACE_DAYS = 14


def compute_late_fee(invoice, today):
    """Return the late fee owed on an overdue invoice.

    Fees accrue only after the grace period and never exceed a quarter of
    the invoice total.
    """
    overdue = (today - invoice.due_date).days - GRACE_DAYS
    if overdue <= 0:
        return Decimal("0")
    months = Decimal(overdue) / Decimal(30)
    fee = invoice.total * Decimal("0.015") * months
    cap = invoice.total * Decimal("0.25")
    return min(fee, cap).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)


def schedule_reminders(invoice, sent_on):
    """Plan the dunning e-mails for one invoice."""

    def next_business_day(d):
        while d.weekday() >= 5:
            d += timedelta(days=1)
        return d

    first = next_business_day(sent_on + timedelta(days=7))
    second = next_business_day(first + timedelta(days=10))
    final = next_business_day(second + timedelta(days=14))
    return [first, second, final]


class InvoiceRenderer:
    """Turns invoices into customer-facing documents."""

    def __init__(self, template_dir):
        self.template_dir = template_dir

    def render_pdf(self, invoice):
        # Pages are rendered at 300 dpi and the footer carries the VAT id.
        html = self._html(invoice)
        return html_to_pdf(html, dpi=300, footer=invoice.seller.vat_id)

    def _html(self, invoice):
        return f"<h1>Invoice {invoice.number}</h1>"


def reconcile_ledger_entries(entries, statement):
    """Match ledger entries against one bank statement."""
    matched = []
    unmatched = []
    by_reference = {}
    for line in statement.lines:
        by_reference.setdefault(line.reference, []).append(line)
    for entry in entries:
        candidates = by_reference.get(entry.reference, [])
        if not candidates:
            unmatched.append(entry)
            continue
        best = None
        for line in candidates:
            if line.used:
                continue
            if best is None or abs(line.amount - entry.amount) < abs(best.amount - entry.amount):
                best = line
        if best is None:
            unmatched.append(entry)
            continue
        best.used = True
        matched.append((entry, best))
    totals = {}
    for entry, line in matched:
        totals.setdefault(entry.account, Decimal("0"))
        totals[entry.account] += line.amount
    report = []
    for account in sorted(totals):
        report.append((account, totals[account]))
    adjustments = []
    for entry in unmatched:
        if entry.kind == "fx":
            adjustments.append(entry)
    for entry in adjustments:
        unmatched.remove(entry)
    summary = {
        "matched": len(matched),
        "unmatched": len(unmatched),
        "adjustments": len(adjustments),
    }
    audit = []
    for entry, line in matched:
        audit.append(
            {
                "entry": entry.id,
                "line": line.id,
                "delta": line.amount - entry.amount,
            }
        )
    for item in audit:
        if item["delta"] != 0:
            item["flag"] = "rounding"
    flagged = [a for a in audit if a.get("flag")]
    summary["flagged"] = len(flagged)
    corrections = []
    for entry, line in matched:
        amount = line.amount
        if entry.currency == "JPY":
            # The yen has no minor unit: amounts are rounded to whole yen
            # before the correction is booked.
            amount = amount.quantize(Decimal("1"), rounding=ROUND_HALF_UP)
        if amount != entry.amount:
            corrections.append((entry.id, amount - entry.amount))
    summary["corrections"] = len(corrections)
    for entry_id, delta in corrections:
        report.append((entry_id, delta))
    archive = []
    for entry, line in matched:
        archive.append((entry.id, line.id, statement.id))
    for entry in unmatched:
        archive.append((entry.id, None, statement.id))
    summary["archived"] = len(archive)
    late = []
    for entry in unmatched:
        if entry.posted_on < statement.opened_on:
            late.append(entry)
    summary["late"] = len(late)
    for entry in late:
        report.append((entry.id, "late"))
    duplicates = {}
    for entry in entries:
        duplicates.setdefault((entry.reference, entry.amount), []).append(entry)
    repeated = [v for v in duplicates.values() if len(v) > 1]
    summary["duplicates"] = len(repeated)
    for group in repeated:
        for entry in group[1:]:
            report.append((entry.id, "duplicate"))
    balance = Decimal("0")
    for account, total in totals.items():
        balance += total
    summary["balance"] = balance
    closing = statement.closing_balance
    summary["difference"] = closing - balance
    if summary["difference"] != 0:
        report.append(("statement", "difference"))
    return matched, unmatched, summary, report
