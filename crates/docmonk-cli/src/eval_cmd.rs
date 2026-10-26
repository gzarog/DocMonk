//! `eval record|list|remove|run`: personal retrieval evaluation on your own
//! corpus (docmonk_service::personal_eval). Queries stay in
//! `<home>/eval/queries.json`; nothing is uploaded.

use clap::Subcommand;
use docmonk_core::errors::DocMonkError;
use docmonk_service::personal_eval::{self, Expect};
use serde_json::{json, Value};

use crate::{prepared_home, print_json};

#[derive(Subcommand)]
pub enum EvalCommand {
    /// Record a query and where its answer is (repeat --expect for more).
    Record {
        /// The question, as you would ask it.
        query: String,
        /// A file the answer is in, optionally with a line: path[:line].
        #[arg(long = "expect", required_unless_present = "unanswerable")]
        expect: Vec<String>,
        /// The corpus has no answer: a good run abstains.
        #[arg(long, conflicts_with = "expect")]
        unanswerable: bool,
        #[arg(long = "json")]
        json: bool,
    },
    /// List the recorded queries.
    List {
        #[arg(long = "json")]
        json: bool,
    },
    /// Remove a recorded query by id.
    Remove { id: String },
    /// Score the recorded queries: recall@1/10, MRR and abstention.
    Run {
        /// Index the sources again with this embedding model (in a
        /// throwaway home under <home>/eval/runs) and score there.
        #[arg(long, conflicts_with = "compare")]
        model: Option<String>,
        /// Two or more models, comma-separated, side by side.
        #[arg(long, value_delimiter = ',')]
        compare: Vec<String>,
        #[arg(long = "json")]
        json: bool,
    },
}

fn pct(v: &Value) -> String {
    v.as_f64()
        .map_or_else(|| "-".to_owned(), |x| format!("{x:.3}"))
}

pub fn run(cmd: EvalCommand) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    match cmd {
        EvalCommand::Record {
            query,
            expect,
            unanswerable: _,
            json: json_output,
        } => {
            let expect = expect
                .iter()
                .map(|e| Expect::parse(e))
                .collect::<Result<Vec<_>, _>>()?;
            let q = personal_eval::record(&home, &query, expect)?;
            if json_output {
                return print_json(&q);
            }
            println!(
                "Recorded {} in {}",
                q.id,
                personal_eval::queries_path(&home).display()
            );
        }
        EvalCommand::List { json: json_output } => {
            let set = personal_eval::load_queries(&home)?;
            if json_output {
                return print_json(&set);
            }
            if set.queries.is_empty() {
                println!("No queries recorded.");
            }
            for q in &set.queries {
                let expect: Vec<String> = q
                    .expect
                    .iter()
                    .map(|e| match e.line {
                        Some(l) => format!("{}:{l}", e.path),
                        None => e.path.clone(),
                    })
                    .collect();
                let expect = if expect.is_empty() {
                    "(unanswerable)".to_owned()
                } else {
                    expect.join(", ")
                };
                println!("{}  {}  ->  {expect}", q.id, q.query);
            }
        }
        EvalCommand::Remove { id } => {
            personal_eval::remove(&home, &id)?;
            println!("Removed {id}");
        }
        EvalCommand::Run {
            model,
            compare,
            json: json_output,
        } => {
            let models: Vec<String> = model.into_iter().chain(compare).collect();
            let r = personal_eval::run(&home, &models)?;
            if json_output {
                return print_json(&r);
            }
            println!(
                "{:<24} {:>9} {:>9} {:>9} {:>9} {:>9}",
                "model", "recall@1", "recall@10", "MRR", "false-abst", "false-supp"
            );
            for run in r["runs"].as_array().into_iter().flatten() {
                let m = &run["metrics"];
                println!(
                    "{:<24} {:>9} {:>9} {:>9} {:>9} {:>9}",
                    format!(
                        "{}{}",
                        run["embedding_model"].as_str().unwrap_or_default(),
                        if run["semantic"] == true {
                            ""
                        } else {
                            " (lexical)"
                        }
                    ),
                    pct(&m["recall@1"]),
                    pct(&m["recall@10"]),
                    pct(&m["mrr"]),
                    pct(&m["false_abstention"]),
                    pct(&m["false_support"]),
                );
            }
            let first = &r["runs"][0];
            let missed: Vec<Value> = first["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|q| q["answerable"] == true && q["rank"].is_null())
                .map(|q| json!(q["id"]))
                .collect();
            if !missed.is_empty() {
                println!(
                    "\nNot found in the top {}: {}",
                    personal_eval::DEPTH,
                    missed
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(())
}
