//! Messaging topology (ADR 0044) over `fixtures/messaging`: a C# publisher
//! source (RabbitMQ, MassTransit, Confluent.Kafka), a C# consumer source,
//! a kafkajs consumer and a Spring listener. `impact` on a message
//! contract lists its publishers and consumers across sources, and the
//! result is identical across fresh indexes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn docmonk(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_docmonk"))
        .args(args)
        .env("DOCMONK_HOME", home)
        .env("DOCMONK_SEARCH__SEMANTIC", "false")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    assert!(o.status.success(), "{o:?}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json(home: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&out(&docmonk(home, args))).unwrap()["data"].clone()
}

fn copy_tree(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// A fresh home indexing the four fixture sources (copied under `dir`).
fn indexed(dir: &Path) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/messaging");
    let home = dir.join("home");
    out(&docmonk(&home, &["init"]));
    for name in ["orders", "billing", "web", "java"] {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        copy_tree(&fixtures.join(name), &root);
        out(&docmonk(&home, &["source", "add", root.to_str().unwrap()]));
    }
    out(&docmonk(&home, &["index"]));
    home
}

fn names(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not an array: {v}"))
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn impact_lists_publishers_and_consumers_across_sources() {
    let dir = tempfile::tempdir().unwrap();
    let home = indexed(dir.path());

    // RabbitMQ: the exchange's publisher, and the consumer of the queue
    // bound with a matching key ("order.*"), not the one bound to
    // "invoice.#".
    let i = json(&home, &["impact", "orders", "--json"]);
    assert_eq!(i["found"], true, "{i}");
    assert_eq!(
        names(&i["publishers"]),
        ["Orders.OrderService.PlaceOrder"],
        "{i}"
    );
    assert_eq!(
        names(&i["consumers"]),
        ["Billing.BillingWorker.Start"],
        "{i}"
    );
    let flow = &i["messaging"];
    assert!(
        flow["declarations"][0]["entity"] == "Orders.OrderService.Setup",
        "{flow}"
    );
    let sources: std::collections::BTreeSet<&str> = flow["publishers"]
        .as_array()
        .unwrap()
        .iter()
        .chain(flow["consumers"].as_array().unwrap())
        .map(|h| h["source_id"].as_str().unwrap())
        .collect();
    assert_eq!(sources.len(), 2, "two sources: {flow}");

    // From the queue: its consumer and the publisher that reaches it.
    let q = json(&home, &["impact", "billing", "--json"]);
    assert_eq!(
        names(&q["consumers"]),
        ["Billing.BillingWorker.Start"],
        "{q}"
    );
    assert_eq!(
        names(&q["publishers"]),
        ["Orders.OrderService.PlaceOrder"],
        "{q}"
    );

    // MassTransit: the contract type is defined, published and consumed.
    let t = json(&home, &["impact", "OrderPlaced", "--json"]);
    assert_eq!(
        t["defined"][0]["qualified_name"], "Shop.Contracts.OrderPlaced",
        "{t}"
    );
    assert_eq!(
        names(&t["publishers"]),
        ["Orders.OrderService.AnnounceAsync"],
        "{t}"
    );
    assert_eq!(
        names(&t["consumers"]),
        ["Billing.OrderPlacedConsumer"],
        "{t}"
    );

    // Kafka: a C# producer (topic constant), a kafkajs and a Spring
    // consumer (constant).
    let k = json(&home, &["impact", "orders.events", "--json"]);
    assert_eq!(
        names(&k["publishers"]),
        ["Orders.OrderService.StreamAsync"],
        "{k}"
    );
    let consumers = names(&k["consumers"]);
    assert_eq!(consumers.len(), 2, "{k}");
    assert!(consumers.iter().any(|c| c.ends_with("onEvent")), "{k}");
    assert!(consumers.iter().any(|c| c.ends_with("run")), "{k}");
    assert!(k["blast_radius_score"].as_u64().unwrap() >= 3, "{k}");

    // Text view and explore carry the same sites.
    let text = out(&docmonk(&home, &["impact", "orders.events"]));
    assert!(
        text.contains("Publishers:") && text.contains("StreamAsync"),
        "{text}"
    );
    assert!(
        text.contains("Consumers:") && text.contains("onEvent"),
        "{text}"
    );
    let e: serde_json::Value = serde_json::from_str(&out(&docmonk(
        &home,
        &["explore", "orders.events", "--json"],
    )))
    .unwrap();
    let rels: Vec<&str> = e["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["relationship"].as_str())
        .collect();
    assert!(
        rels.contains(&"publishes") && rels.contains(&"consumes"),
        "{e}"
    );

    // Code without messaging keeps its impact shape.
    let plain = json(&home, &["impact", "Setup", "--json"]);
    assert!(plain.get("publishers").is_none(), "{plain}");

    // Deterministic: a second fresh index answers byte-identically.
    let again = tempfile::tempdir().unwrap();
    let home2 = indexed(again.path());
    for q in ["orders", "OrderPlaced", "orders.events"] {
        let strip = |mut v: serde_json::Value| {
            // Source ids and paths name the temp directories.
            let s = v.to_string();
            v = serde_json::from_str(
                &s.replace(dir.path().to_str().unwrap(), "<DIR>")
                    .replace(again.path().to_str().unwrap(), "<DIR>"),
            )
            .unwrap();
            if let Some(o) = v.as_object_mut() {
                o.remove("graph");
            }
            for list in ["publishers", "consumers", "bindings", "declarations"] {
                if let Some(a) = v["messaging"][list].as_array_mut() {
                    for h in a {
                        h["source_id"] = serde_json::Value::Null;
                    }
                }
            }
            // Entity ids hash the source id too.
            if let Some(a) = v["defined"].as_array_mut() {
                for d in a {
                    d["source_id"] = serde_json::Value::Null;
                    d["entity_id"] = serde_json::Value::Null;
                }
            }
            v
        };
        assert_eq!(
            strip(json(&home, &["impact", q, "--json"])),
            strip(json(&home2, &["impact", q, "--json"])),
            "{q}"
        );
    }
}
