//! `cargo xtask adr-audit`: lists ADRs that still carry open items, so an
//! unmeasured claim cannot hide in ADR prose. An ADR is listed when its
//! `Status:` paragraph says the work is in progress or only proposed, or
//! when its text says something is not measured yet, still to be measured
//! or stays open. The audit reports and never fails: CI prints it on every
//! PR and the nightly workflow writes it to the job summary
//! (`--markdown`).

use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
    pub text: String,
}

fn status_re() -> Regex {
    Regex::new(r"(?i)\b(in progress|proposed|draft|not (yet )?implemented|partially implemented)\b")
        .expect("static regex")
}

fn open_re() -> Regex {
    Regex::new(
        r"(?i)\b(not (yet )?measured|not measured yet|still to be measured|yet to be measured|to be measured|(stays|remains|is still|left) open|open measurement|not yet run|still to be run)\b",
    )
    .expect("static regex")
}

/// The open items of one ADR (`name` is its file name).
pub fn scan(name: &str, text: &str) -> Vec<Finding> {
    let (status, open) = (status_re(), open_re());
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    // The Status paragraph: the `Status:` line and its wrapped continuation.
    if let Some(start) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("Status:"))
    {
        let para: Vec<&str> = lines[start..]
            .iter()
            .take_while(|l| !l.trim().is_empty())
            .copied()
            .collect();
        let joined = para.join(" ");
        if status.is_match(&joined) {
            out.push(Finding {
                file: name.to_owned(),
                line: start + 1,
                kind: "status",
                text: joined.split_whitespace().collect::<Vec<_>>().join(" "),
            });
        }
    }
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("Status:") {
            continue;
        }
        if open.is_match(l) {
            out.push(Finding {
                file: name.to_owned(),
                line: i + 1,
                kind: "open",
                text: l.trim().to_owned(),
            });
        }
    }
    out
}

pub fn audit(dir: &Path) -> Result<Vec<Finding>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text =
            std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
        let name = f.file_name().unwrap_or_default().to_string_lossy();
        out.extend(scan(&name, &text));
    }
    Ok(out)
}

pub fn run(root: &Path, markdown: bool) -> Result<()> {
    let findings = audit(&root.join("docs/adr"))?;
    let adrs = {
        let mut f: Vec<&str> = findings.iter().map(|f| f.file.as_str()).collect();
        f.dedup();
        f.len()
    };
    if markdown {
        println!("## ADRs with open items\n");
        if findings.is_empty() {
            println!("None.");
            return Ok(());
        }
        println!("| ADR | Line | Kind | Text |\n|---|---:|---|---|");
        for f in &findings {
            println!(
                "| `{}` | {} | {} | {} |",
                f.file,
                f.line,
                f.kind,
                f.text.replace('|', "\\|")
            );
        }
        println!("\n{} open item(s) in {adrs} ADR(s).", findings.len());
    } else {
        for f in &findings {
            println!("docs/adr/{}:{}: [{}] {}", f.file, f.line, f.kind, f.text);
        }
        println!(
            "adr audit: {} open item(s) in {adrs} ADR(s) (reported, not failing)",
            findings.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_open_measurements_are_listed() {
        let text = "# ADR 9\n\nStatus: accepted; implementation in progress (plan,\nphase 5).\n\n\
                    ## Measurement\n\nThe live numbers are not measured yet.\n\
                    Still to be measured: recall on both engines.\nIt stays open.\n";
        let f = scan("0009-x.md", text);
        assert_eq!(f.len(), 4, "{f:?}");
        assert_eq!((f[0].kind, f[0].line), ("status", 3));
        assert!(f[0].text.contains("phase 5"), "wrapped status is joined");
        assert_eq!((f[1].kind, f[1].line), ("open", 8));
        assert_eq!(f[2].line, 9);
        assert_eq!(f[3].line, 10);
    }

    #[test]
    fn closed_adrs_are_not_listed() {
        let text = "# ADR 1\n\nStatus: accepted and implemented.\n\n\
                    Published vs in progress: the status report separates them.\n\
                    Measured: recall 0.96 at 20k vectors.\n";
        assert!(scan("0001-x.md", text).is_empty());
    }
}
