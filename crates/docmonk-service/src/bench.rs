//! Shared pieces of DocMonk's benchmark binaries (`docmonk-graph-bench`,
//! `docmonk-search-bench`): the synthetic corpus and process resource
//! readings. Not a stable API.

/// The synthetic ASP.NET-style C# repository plus Markdown design notes
/// (no real source is used).
pub mod corpus {
    use std::path::Path;

    pub fn controller(i: usize, n: usize) -> String {
        let dep = (i + 1) % n;
        format!(
            r#"using System;
using Microsoft.AspNetCore.Mvc;
using Shop.Services;

namespace Shop.Controllers.Area{area}
{{
    [ApiController]
    public class Orders{i}Controller : ControllerBase
    {{
        private readonly IOrderService{i} _service;

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
            Guard.NotNull(order);
            _service.Save(order);
            return Created("", order);
        }}
    }}
}}
"#,
            area = i % 25
        )
    }

    pub fn service(i: usize, find: &str, body: &str) -> String {
        format!(
            r#"using System.Collections.Generic;
using Shop.Models;

namespace Shop.Services
{{
    public interface IOrderService{i}
    {{
        Order{i} {find}(int id);
        void Save(Order{i} order);
    }}

    public class OrderService{i} : IOrderService{i}
    {{
        private readonly List<Order{i}> _items = new List<Order{i}>();

        public Order{i} {find}(int id) {{ {body} }}
        public void Save(Order{i} order) {{ _items.Add(order); Audit.Log("save"); }}
    }}

    public static class Mapper{i}
    {{
        public static object ToDto(Order{i} order) {{ return order; }}
    }}
}}
"#
        )
    }

    pub const FIND_BODY: &str = "return _items.Find(o => o.Id == id);";

    pub fn model(i: usize) -> String {
        format!(
            r#"namespace Shop.Models
{{
    public class Order{i} : EntityBase
    {{
        public int Id {{ get; set; }}
        public decimal Total;
    }}
}}
"#
        )
    }

    pub fn note(d: usize, groups: usize, extra: &str) -> String {
        let mut s = format!("# Design note {d}\n\n{extra}");
        for k in 0..12 {
            let i = (d * 37 + k * 101) % groups.max(1);
            s.push_str(&format!(
                "## Orders {i}\n\nThe Orders{i}Controller delegates to OrderService{i} \
             (see OrderService{i}.cs); Shop.Services.OrderService{i}.Save audits every write, \
             and OrderService{i}.Find serves GET api/orders{i}/{{id}}.\n\n"
            ));
        }
        s
    }

    pub fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, content).expect("write");
    }

    /// `files` code files in groups of three plus one shared file, and `docs`
    /// notes.
    pub fn emit(root: &Path, files: usize, docs: usize) -> usize {
        let groups = (files.saturating_sub(1) / 3).max(2);
        for i in 0..groups {
            write(
                root,
                &format!("src/Controllers/Area{}/Orders{i}Controller.cs", i % 25),
                &controller(i, groups),
            );
            write(
                root,
                &format!("src/Services/OrderService{i}.cs"),
                &service(i, "Find", FIND_BODY),
            );
            write(root, &format!("src/Models/Order{i}.cs"), &model(i));
        }
        write(
        root,
        "src/Common/Common.cs",
        "namespace Shop.Common\n{\n    public abstract class EntityBase { }\n    public static class Guard { public static void NotNull(object o) { } }\n    public static class Audit { public static void Log(string m) { } }\n}\n",
    );
        for d in 0..docs {
            write(root, &format!("docs/note_{d:05}.md"), &note(d, groups, ""));
        }
        groups
    }
}

/// User+system CPU seconds of this process (`/proc/self/stat`, 100 Hz).
pub fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

/// Peak resident set since the last [`reset_peak_rss`], in MB.
pub fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

pub fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// Nearest-rank percentile of an ascending slice (`p` in 0..=1).
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}
