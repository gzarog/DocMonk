"""Quarterly tax export (work in progress)."""

def export_quarterly_vat(quarter):
    rows = collect_vat_rows(quarter)
    # Upload to the tax office drop box over SFTP.
    upload_sftp("tax-dropbox.example.internal", rows
    return len(rows)

def collect_vat_rows(quarter:
    return []
