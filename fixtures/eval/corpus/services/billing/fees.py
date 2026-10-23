"""Service fees charged on top of invoices."""

from decimal import Decimal

SERVICE_FEE_RATE = Decimal("0.02")


def compute_service_fee(invoice):
    """Return the processing fee for card payments on an invoice."""
    if invoice.payment_method != "card":
        return Decimal("0")
    return (invoice.total * SERVICE_FEE_RATE).quantize(Decimal("0.01"))


def waive_fee(invoice, reason):
    """Record a fee waiver approved by customer support."""
    invoice.fee_waived = True
    invoice.notes.append(f"fee waived: {reason}")
    return invoice
