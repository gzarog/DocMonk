//! `ragmonk doctor [--json]` and `ragmonk health [--json]`.

use docmonk_core::errors::RagMonkError;
use docmonk_ops::doctor::{overall, run_checks, sections_json, verdict};
use serde_json::json;

use crate::{prepared_home, print_json};

pub fn doctor(json_output: bool) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    let sections = run_checks(&home)?;
    let result = overall(&sections);
    if json_output {
        print_json(&sections_json(&sections, result))?;
    } else {
        println!("\nRagMonk Doctor\n");
        for s in &sections {
            println!("{}", s.name);
            for c in &s.checks {
                println!("  {} {}", c.status.to_uppercase(), c.detail);
            }
            println!();
        }
        println!("Result:\n  {result}");
    }
    verdict(result)
}

pub fn health(json_output: bool) -> Result<(), RagMonkError> {
    let home = prepared_home()?;
    let result = overall(&run_checks(&home)?);
    if json_output {
        print_json(&json!({ "result": result }))?;
    } else {
        println!("{result}");
    }
    verdict(result)
}
