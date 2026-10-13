//! C# code-intelligence stress benchmark.
//!
//! ```text
//! docmonk-code-bench [--files N] [--out report.json] [--emit-only DIR] [--cache-mb MB]
//! ```
//!
//! Generates a representative ASP.NET-style C# repository (namespaces,
//! interfaces, controllers with route attributes, services, models and
//! cross-file calls), then measures cold, warm, single-edit, 1%-change and
//! delete passes through the indexing coordinator with whole-build
//! cross-file resolution. `--emit-only` writes the corpus and exits.

use std::path::{Path, PathBuf};
use std::time::Instant;

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options, SourceResult};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::json;

fn controller(i: usize, n: usize) -> String {
    let dep = (i + 1) % n;
    format!(
        r#"using System;
using System.Collections.Generic;
using Microsoft.AspNetCore.Mvc;
using Shop.Services;
using Shop.Models;

namespace Shop.Controllers.Area{area}
{{
    [ApiController]
    public class Orders{i}Controller : ControllerBase, IDisposable
    {{
        private readonly IOrderService{i} _service;
        public int Count {{ get; set; }}

        public Orders{i}Controller(IOrderService{i} service)
        {{
            _service = service;
        }}

        [HttpGet("api/orders{i}/{{id}}")]
        public IActionResult Get(int id)
        {{
            var order = _service.Find(id);
            return Ok(Mapper{dep}.ToDto(order));
        }}

        [HttpPost("api/orders{i}")]
        public IActionResult Create(Order{i} order)
        {{
            Validate(order);
            _service.Save(order);
            return Created("", order);
        }}

        [Route("api/orders{i}/all")]
        public IEnumerable<Order{i}> List() {{ return _service.All(); }}

        private void Validate(Order{i} order) {{ Guard.NotNull(order); }}

        public void Dispose() {{ }}
    }}
}}
"#,
        area = i % 25
    )
}

fn service(i: usize) -> String {
    format!(
        r#"using System.Collections.Generic;
using Shop.Models;

namespace Shop.Services
{{
    public interface IOrderService{i}
    {{
        Order{i} Find(int id);
        void Save(Order{i} order);
        IEnumerable<Order{i}> All();
    }}

    public class OrderService{i} : IOrderService{i}
    {{
        private readonly List<Order{i}> _items = new List<Order{i}>();

        public Order{i} Find(int id) {{ return _items.Find(o => o.Id == id); }}
        public void Save(Order{i} order) {{ _items.Add(order); Audit.Log("save"); }}
        public IEnumerable<Order{i}> All() {{ return _items; }}
    }}

    public static class Mapper{i}
    {{
        public static object ToDto(Order{i} order) {{ return new {{ order.Id }}; }}
    }}
}}
"#
    )
}

fn model(i: usize) -> String {
    format!(
        r#"namespace Shop.Models
{{
    public class Order{i} : EntityBase
    {{
        public int Id {{ get; set; }}
        public string Name {{ get; set; }}
        public decimal Total;
    }}

    public enum Status{i} {{ New, Paid, Shipped }}

    public struct Money{i} {{ public decimal Amount; }}
}}
"#
    )
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, content).expect("write");
}

fn emit(root: &Path, n: usize) {
    let groups = n.div_ceil(3);
    for i in 0..groups {
        write(
            root,
            &format!("src/Controllers/Area{}/Orders{i}Controller.cs", i % 25),
            &controller(i, groups),
        );
        write(
            root,
            &format!("src/Services/OrderService{i}.cs"),
            &service(i),
        );
        write(root, &format!("src/Models/Order{i}.cs"), &model(i));
    }
    write(
        root,
        "src/Common/Common.cs",
        "namespace Shop.Common\n{\n    public abstract class EntityBase { }\n    public static class Guard { public static void NotNull(object o) { } }\n    public static class Audit { public static void Log(string m) { } }\n}\n",
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n: usize = arg("--files").and_then(|v| v.parse().ok()).unwrap_or(300);
    if let Some(dir) = arg("--emit-only") {
        emit(Path::new(&dir), n);
        return;
    }
    let out = arg("--out");
    let tmp: PathBuf =
        std::env::temp_dir().join(format!("docmonk-code-bench-{}", std::process::id()));
    let root = tmp.join("source");
    emit(&root, n);

    let home = Home::new(tmp.join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 64).expect("control plane");
    let canonical = docmonk_core::paths::resolve(&root)
        .expect("resolve")
        .to_string_lossy()
        .into_owned();
    let (src, _) = cp
        .add_source(&NewSource {
            canonical_path: canonical.clone(),
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .expect("add source");
    let reg = docmonk_code::registry();
    let mut opts = Options::from_config(&docmonk_config::DocMonkConfig::default());
    if let Some(mb) = arg("--cache-mb").and_then(|v| v.parse().ok()) {
        opts.cache_size_mb = mb;
    }
    let mut results = Vec::new();
    let mut run = |name: &str, cp: &mut ControlPlane| {
        let started = Instant::now();
        let r: SourceResult =
            run_source(&layout, cp, &src, &reg, &opts, &mut NoProgress).expect("run");
        let wall = started.elapsed().as_secs_f64();
        let (entities, relationships) = match &r.build_id {
            Some(b) => {
                let store =
                    ProjectStore::open(&layout, &project_id_for_canonical(&canonical), &src.id, 64)
                        .expect("store");
                (
                    store.count("entities", b).expect("count"),
                    store.count("relationships", b).expect("count"),
                )
            }
            None => (0, 0),
        };
        eprintln!(
            "{name:<20} wall={wall:>8.4}s scanned={} indexed={} failed={} entities={entities} relationships={relationships}",
            r.counts.scanned, r.indexed, r.failed
        );
        results.push(json!({
            "scenario": name, "wall_time_s": wall, "counts": r.counts, "timings": r.timings,
            "indexed": r.indexed, "failed": r.failed,
            "entities": entities, "relationships": relationships,
        }));
    };
    run("cold_index", &mut cp);
    run("warm_unchanged", &mut cp);
    let groups = n.div_ceil(3);
    write(
        &root,
        "src/Services/OrderService0.cs",
        &format!("{}// edited\n", service(0)),
    );
    run("single_edit", &mut cp);
    for i in 0..(groups / 33).max(1) {
        write(
            &root,
            &format!("src/Models/Order{i}.cs"),
            &format!("{}// one percent\n", model(i)),
        );
    }
    run("one_percent_change", &mut cp);
    std::fs::remove_file(root.join("src/Services/OrderService1.cs")).expect("delete");
    run("delete", &mut cp);

    let report = json!({
        "plan_id": "docmonk-1.0-clean-slate",

        "language": "csharp",
        "files": groups * 3 + 1,
        "scenarios": results,
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    match out {
        Some(p) => std::fs::write(p, text + "\n").expect("write report"),
        None => println!("{text}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
