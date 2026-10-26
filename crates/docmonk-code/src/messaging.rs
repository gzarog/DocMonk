//! Messaging topology (ADR 0044): publish, consume, bind and declare sites
//! of message brokers, read statically from the source text. Always
//! HEURISTIC, like [`crate::framework`] routes: a call site is recorded when
//! its endpoint is a string literal or a constant defined in the same file;
//! names built at runtime, configuration values and other files'
//! constants are not seen.
//!
//! Rules (each only in files that mention the library, which keeps
//! look-alike calls of other APIs out):
//!
//! * RabbitMQ.Client (C#): `BasicPublish`, `BasicConsume`, `QueueBind`,
//!   `ExchangeDeclare` (and their `Async` forms).
//! * MassTransit (C#): `Publish<T>` / `Send<T>` / `Publish(new T ...)`,
//!   `IConsumer<T>`.
//! * Confluent.Kafka (C#): `Produce` / `ProduceAsync`, `Subscribe`.
//! * kafkajs (JavaScript / TypeScript): `send({ topic })`,
//!   `subscribe({ topic | topics })`.
//! * Spring Kafka (Java): `@KafkaListener(topics = ...)`,
//!   `KafkaTemplate.send(topic, ...)`.

use std::collections::HashMap;
use std::sync::OnceLock;

use docmonk_core::messaging::{self as msg, Role};
use docmonk_core::models::EntityType;
use regex::Regex;

use crate::extract::ExtractedEntity;

/// Message sites kept per file (bounds graph rows on generated code).
pub const MAX_MESSAGE_SITES_PER_FILE: usize = 256;

/// One message site.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MessageSite {
    pub line: i64,
    /// The innermost function, method or type around the site.
    pub owner_local_id: Option<usize>,
    pub role: Role,
    /// The endpoint key (`kafka/orders`, ...; see [`docmonk_core::messaging`]).
    pub key: String,
    pub framework: &'static str,
    pub routing_key: Option<String>,
    /// Bind sites: the other endpoint.
    pub bound: Option<String>,
}

/// A string literal (`"x"`, `'x'`, `` `x` ``) or a dotted identifier.
const ARG: &str = r#"(?:"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'|`[^`$\n]*`|[A-Za-z_][\w.]*)"#;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&pattern.replace("ARG", ARG)).expect("regex"))
}

macro_rules! rule {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            re(&R, $pattern)
        }
    };
}

rule!(
    cs_const,
    r#"\bconst\s+string\s+([A-Za-z_]\w*)\s*=\s*"([^"\n]*)"|\bstatic\s+readonly\s+string\s+([A-Za-z_]\w*)\s*=\s*"([^"\n]*)""#
);
rule!(
    java_const,
    r#"\bstatic\s+final\s+String\s+([A-Za-z_]\w*)\s*=\s*"([^"\n]*)""#
);
rule!(
    js_const,
    r#"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:'([^'\n]*)'|"([^"\n]*)"|`([^`$\n]*)`)"#
);
rule!(
    rabbit_publish,
    r#"\bBasicPublish(?:Async)?\s*(?:<[^>]*>)?\s*\(\s*(?:exchange\s*:\s*)?(ARG)\s*,\s*(?:routingKey\s*:\s*)?(ARG)"#
);
rule!(
    rabbit_consume,
    r#"\bBasicConsume(?:Async)?\s*\(\s*(?:queue\s*:\s*)?(ARG)"#
);
rule!(
    rabbit_bind,
    r#"\bQueueBind(?:Async)?\s*\(\s*(?:queue\s*:\s*)?(ARG)\s*,\s*(?:exchange\s*:\s*)?(ARG)\s*,\s*(?:routingKey\s*:\s*)?(ARG)"#
);
rule!(
    rabbit_declare,
    r#"\bExchangeDeclare(?:Async)?\s*\(\s*(?:exchange\s*:\s*)?(ARG)"#
);
rule!(
    mt_publish_generic,
    r#"\.(?:Publish|Send)\s*<\s*([A-Za-z_][\w.]*)\s*>\s*\("#
);
rule!(
    mt_publish_new,
    r#"\.(?:Publish|Send)\s*\(\s*new\s+([A-Za-z_][\w.]*)\s*[({]"#
);
rule!(mt_consumer, r#"\bIConsumer\s*<\s*([A-Za-z_][\w.]*)\s*>"#);
rule!(
    confluent_produce,
    r#"\.Produce(?:Async)?\s*\(\s*(?:topic\s*:\s*)?(ARG)"#
);
rule!(
    confluent_subscribe,
    r#"\.Subscribe\s*\(\s*(?:new\s*(?:\[\]|List<string>\s*(?:\(\s*\))?)\s*\{([^}]*)\}|(ARG))"#
);
rule!(kafkajs_send, r#"\.send\s*\(\s*\{[^}]*?\btopic\s*:\s*(ARG)"#);
rule!(
    kafkajs_subscribe,
    r#"\.subscribe\s*\(\s*\{[^}]*?\btopics?\s*:\s*(?:\[([^\]]*)\]|(ARG))"#
);
rule!(
    spring_listener,
    r#"@KafkaListener\s*\([^)]*?\btopics\s*=\s*(?:\{([^}]*)\}|(ARG))"#
);
rule!(
    spring_send,
    r#"\b\w*[Tt]emplate\s*\.\s*send(?:Default)?\s*\(\s*(ARG)"#
);
rule!(list_item, r#"ARG"#);

/// Same-file string constants by name.
fn constants(language: &str, text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let r = match language {
        "csharp" => cs_const(),
        "java" => java_const(),
        "javascript" | "typescript" => js_const(),
        _ => return out,
    };
    for c in r.captures_iter(text) {
        let mut groups = (1..c.len()).filter_map(|i| c.get(i));
        if let (Some(name), Some(value)) = (groups.next(), groups.next()) {
            out.entry(name.as_str().to_owned())
                .or_insert_with(|| value.as_str().to_owned());
        }
    }
    out
}

/// A call argument's value: a literal's content, or a same-file constant
/// (by its last dotted segment).
fn value(arg: &str, consts: &HashMap<String, String>) -> Option<String> {
    let a = arg.trim();
    let quoted = ['"', '\'', '`']
        .iter()
        .find(|q| a.len() >= 2 && a.starts_with(**q) && a.ends_with(**q));
    match quoted {
        Some(_) => Some(a[1..a.len() - 1].to_owned()),
        None => consts.get(a.rsplit('.').next()?).cloned(),
    }
}

fn values(list: &str, consts: &HashMap<String, String>) -> Vec<String> {
    list_item()
        .find_iter(list)
        .filter_map(|m| value(m.as_str(), consts))
        .collect()
}

/// 1-based line of byte offset `at`.
fn line_of(text: &str, at: usize) -> i64 {
    text.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as i64
        + 1
}

/// The innermost entity spanning `line` (functions and methods before
/// types: a type's span contains its methods').
fn owner(entities: &[ExtractedEntity], line: i64) -> Option<usize> {
    entities
        .iter()
        .filter(|e| e.kind != EntityType::Namespace && e.start_line <= line && line <= e.end_line)
        .min_by_key(|e| (e.end_line - e.start_line, e.local_id))
        .map(|e| e.local_id)
}

/// Every message site of one file, in a stable order (line, role, key).
pub fn detect(language: &str, text: &str, entities: &[ExtractedEntity]) -> Vec<MessageSite> {
    let mut out = Vec::new();
    let consts = constants(language, text);
    let mut site = |at: usize,
                    role: Role,
                    key: String,
                    framework: &'static str,
                    routing_key: Option<String>,
                    bound: Option<String>| {
        let line = line_of(text, at);
        out.push(MessageSite {
            line,
            owner_local_id: owner(entities, line),
            role,
            key,
            framework,
            routing_key,
            bound,
        });
    };
    let arg = |c: &regex::Captures<'_>, i: usize| c.get(i).and_then(|m| value(m.as_str(), &consts));
    match language {
        "csharp" => {
            if text.contains("RabbitMQ") {
                for c in rabbit_publish().captures_iter(text) {
                    let (Some(exchange), Some(rk)) = (arg(&c, 1), arg(&c, 2)) else {
                        continue;
                    };
                    let at = c.get(0).map_or(0, |m| m.start());
                    if exchange.is_empty() {
                        // The default exchange routes to the queue named
                        // by the routing key.
                        site(
                            at,
                            Role::Publish,
                            msg::amqp_queue(&rk),
                            "rabbitmq",
                            None,
                            None,
                        );
                    } else {
                        site(
                            at,
                            Role::Publish,
                            msg::amqp_exchange(&exchange),
                            "rabbitmq",
                            Some(rk),
                            None,
                        );
                    }
                }
                for c in rabbit_consume().captures_iter(text) {
                    if let Some(q) = arg(&c, 1) {
                        let at = c.get(0).map_or(0, |m| m.start());
                        site(
                            at,
                            Role::Consume,
                            msg::amqp_queue(&q),
                            "rabbitmq",
                            None,
                            None,
                        );
                    }
                }
                for c in rabbit_bind().captures_iter(text) {
                    let (Some(q), Some(x), Some(rk)) = (arg(&c, 1), arg(&c, 2), arg(&c, 3)) else {
                        continue;
                    };
                    let at = c.get(0).map_or(0, |m| m.start());
                    let (queue, exchange) = (msg::amqp_queue(&q), msg::amqp_exchange(&x));
                    site(
                        at,
                        Role::Bind,
                        exchange.clone(),
                        "rabbitmq",
                        Some(rk.clone()),
                        Some(queue.clone()),
                    );
                    site(at, Role::Bind, queue, "rabbitmq", Some(rk), Some(exchange));
                }
                for c in rabbit_declare().captures_iter(text) {
                    if let Some(x) = arg(&c, 1).filter(|x| !x.is_empty()) {
                        let at = c.get(0).map_or(0, |m| m.start());
                        site(
                            at,
                            Role::Declare,
                            msg::amqp_exchange(&x),
                            "rabbitmq",
                            None,
                            None,
                        );
                    }
                }
            }
            if text.contains("MassTransit") {
                for r in [mt_publish_generic(), mt_publish_new()] {
                    for c in r.captures_iter(text) {
                        let at = c.get(0).map_or(0, |m| m.start());
                        site(
                            at,
                            Role::Publish,
                            msg::message_type(&c[1]),
                            "masstransit",
                            None,
                            None,
                        );
                    }
                }
                for c in mt_consumer().captures_iter(text) {
                    let at = c.get(0).map_or(0, |m| m.start());
                    site(
                        at,
                        Role::Consume,
                        msg::message_type(&c[1]),
                        "masstransit",
                        None,
                        None,
                    );
                }
            }
            if text.contains("Confluent.Kafka") {
                for c in confluent_produce().captures_iter(text) {
                    if let Some(t) = arg(&c, 1) {
                        let at = c.get(0).map_or(0, |m| m.start());
                        site(at, Role::Publish, msg::kafka_topic(&t), "kafka", None, None);
                    }
                }
                for c in confluent_subscribe().captures_iter(text) {
                    let at = c.get(0).map_or(0, |m| m.start());
                    let topics = match (c.get(1), arg(&c, 2)) {
                        (Some(list), _) => values(list.as_str(), &consts),
                        (None, Some(t)) => vec![t],
                        _ => Vec::new(),
                    };
                    for t in topics {
                        site(at, Role::Consume, msg::kafka_topic(&t), "kafka", None, None);
                    }
                }
            }
        }
        "javascript" | "typescript" if text.contains("kafkajs") => {
            for c in kafkajs_send().captures_iter(text) {
                if let Some(t) = arg(&c, 1) {
                    let at = c.get(0).map_or(0, |m| m.start());
                    site(
                        at,
                        Role::Publish,
                        msg::kafka_topic(&t),
                        "kafkajs",
                        None,
                        None,
                    );
                }
            }
            for c in kafkajs_subscribe().captures_iter(text) {
                let at = c.get(0).map_or(0, |m| m.start());
                let topics = match (c.get(1), arg(&c, 2)) {
                    (Some(list), _) => values(list.as_str(), &consts),
                    (None, Some(t)) => vec![t],
                    _ => Vec::new(),
                };
                for t in topics {
                    site(
                        at,
                        Role::Consume,
                        msg::kafka_topic(&t),
                        "kafkajs",
                        None,
                        None,
                    );
                }
            }
        }
        "java" if text.contains("KafkaListener") || text.contains("KafkaTemplate") => {
            for c in spring_listener().captures_iter(text) {
                let at = c.get(0).map_or(0, |m| m.start());
                let topics = match (c.get(1), arg(&c, 2)) {
                    (Some(list), _) => values(list.as_str(), &consts),
                    (None, Some(t)) => vec![t],
                    _ => Vec::new(),
                };
                for t in topics {
                    site(
                        at,
                        Role::Consume,
                        msg::kafka_topic(&t),
                        "spring_kafka",
                        None,
                        None,
                    );
                }
            }
            for c in spring_send().captures_iter(text) {
                if let Some(t) = arg(&c, 1) {
                    let at = c.get(0).map_or(0, |m| m.start());
                    site(
                        at,
                        Role::Publish,
                        msg::kafka_topic(&t),
                        "spring_kafka",
                        None,
                        None,
                    );
                }
            }
        }
        _ => {}
    }
    out.retain(|s| !s.key.ends_with('/'));
    out.sort();
    out.dedup();
    out.truncate(MAX_MESSAGE_SITES_PER_FILE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites(
        lang: &str,
        path: &str,
        src: &str,
    ) -> Vec<(String, &'static str, String, Option<String>)> {
        let crate::process::PreparedCode::Parsed { extraction, .. } =
            crate::process::prepare_source(src.as_bytes(), path).unwrap()
        else {
            panic!("not parsed");
        };
        assert_eq!(detect(lang, src, &extraction.entities), extraction.messages);
        extraction
            .messages
            .iter()
            .map(|m| {
                (
                    m.owner_local_id
                        .map_or_else(String::new, |o| extraction.entities[o].name.clone()),
                    m.role.as_str(),
                    m.key.clone(),
                    m.routing_key.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn rabbitmq_sites_with_constants_and_named_arguments() {
        let src = r#"using RabbitMQ.Client;
namespace Shop {
  public class Orders {
    private const string Exchange = "orders";
    public void Setup(IModel ch) {
      ch.ExchangeDeclare(Exchange, "topic");
      ch.QueueBind(queue: "billing", exchange: Exchange, routingKey: "order.*");
    }
    public void Place(IModel ch, byte[] b) {
      ch.BasicPublish(exchange: Exchange, routingKey: "order.created", basicProperties: null, body: b);
      ch.BasicPublish("", "direct-queue", null, b);
      ch.BasicPublish(exchange: computed, routingKey: "x", basicProperties: null, body: b);
    }
    public void Listen(IModel ch, IBasicConsumer c) {
      ch.BasicConsume(queue: "billing", autoAck: true, consumer: c);
    }
  }
}
"#;
        let got = sites("csharp", "Orders.cs", src);
        let s = |o: &str, r: &'static str, k: &str, rk: Option<&str>| {
            (o.to_owned(), r, k.to_owned(), rk.map(str::to_owned))
        };
        assert_eq!(
            got,
            [
                s("Setup", "declare", "amqp/exchange/orders", None),
                s("Setup", "bind", "amqp/exchange/orders", Some("order.*")),
                s("Setup", "bind", "amqp/queue/billing", Some("order.*")),
                s(
                    "Place",
                    "publish",
                    "amqp/exchange/orders",
                    Some("order.created")
                ),
                s("Place", "publish", "amqp/queue/direct-queue", None),
                s("Listen", "consume", "amqp/queue/billing", None),
            ]
        );
    }

    #[test]
    fn masstransit_and_confluent_sites() {
        let src = r#"using MassTransit;
using Confluent.Kafka;
namespace Shop {
  public static class Topics { public const string Events = "orders.events"; }
  public class Api {
    public Task A(IPublishEndpoint bus) { return bus.Publish(new Contracts.OrderPlaced(1)); }
    public Task B(ISendEndpoint ep) { return ep.Send<RefundRequested>(new { Id = 1 }); }
    public Task C(IProducer<string, string> p) { return p.ProduceAsync(Topics.Events, null); }
    public void D(IConsumer<string, string> c) { c.Subscribe(new[] { "a", Topics.Events }); }
  }
  public class PlacedConsumer : IConsumer<Contracts.OrderPlaced> {
    public Task Consume(ConsumeContext<Contracts.OrderPlaced> x) { return Task.CompletedTask; }
  }
}
"#;
        let got: Vec<(String, &str, String)> = sites("csharp", "Api.cs", src)
            .into_iter()
            .map(|(o, r, k, _)| (o, r, k))
            .collect();
        let s = |o: &str, r: &'static str, k: &str| (o.to_owned(), r, k.to_owned());
        assert_eq!(
            got,
            [
                s("A", "publish", "type/OrderPlaced"),
                s("B", "publish", "type/RefundRequested"),
                s("C", "publish", "kafka/orders.events"),
                s("D", "consume", "kafka/a"),
                s("D", "consume", "kafka/orders.events"),
                s("PlacedConsumer", "consume", "type/OrderPlaced"),
            ]
        );
    }

    #[test]
    fn kafkajs_and_spring_sites() {
        let js = r#"const { Kafka } = require('kafkajs');
const TOPIC = 'orders.events';
async function produce(producer) {
  await producer.send({ topic: TOPIC, messages: [] });
}
async function consume(consumer) {
  await consumer.subscribe({ topics: ['orders.events', "audit"], fromBeginning: true });
}
"#;
        let got: Vec<(String, &str, String)> = sites("javascript", "k.js", js)
            .into_iter()
            .map(|(o, r, k, _)| (o, r, k))
            .collect();
        let s = |o: &str, r: &'static str, k: &str| (o.to_owned(), r, k.to_owned());
        assert_eq!(
            got,
            [
                s("produce", "publish", "kafka/orders.events"),
                s("consume", "consume", "kafka/audit"),
                s("consume", "consume", "kafka/orders.events"),
            ]
        );
        let java = r#"package shop;
import org.springframework.kafka.annotation.KafkaListener;
public class Listener {
    static final String TOPIC = "orders.events";
    @KafkaListener(topics = TOPIC, groupId = "g")
    public void onEvent(String payload) {
    }
    @KafkaListener(topics = {"a", "b"})
    public void onMany(String payload) {
    }
    public void forward(KafkaTemplate<String, String> kafkaTemplate) {
        kafkaTemplate.send("audit", "x");
    }
}
"#;
        let got: Vec<(String, &str, String)> = sites("java", "Listener.java", java)
            .into_iter()
            .map(|(o, r, k, _)| (o, r, k))
            .collect();
        assert_eq!(
            got,
            [
                s("onEvent", "consume", "kafka/orders.events"),
                s("onMany", "consume", "kafka/a"),
                s("onMany", "consume", "kafka/b"),
                s("forward", "publish", "kafka/audit"),
            ]
        );
    }

    #[test]
    fn message_edges_are_deterministic() {
        let src = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/messaging/orders/Orders/OrderService.cs"),
        )
        .unwrap();
        let rows =
            || crate::process::file_relationships("Orders/OrderService.cs", "f1", &src).unwrap();
        let (a, b) = (rows(), rows());
        assert_eq!(a, b);
        let message: Vec<_> = a
            .iter()
            .filter_map(|r| r.target_symbol.as_deref())
            .filter(|t| t.starts_with("message:"))
            .collect();
        assert_eq!(
            message,
            [
                "message:amqp/exchange/orders",
                "message:amqp/exchange/orders",
                "message:type/OrderPlaced",
                "message:kafka/orders.events",
            ]
        );
    }

    #[test]
    fn look_alike_calls_without_the_library_are_ignored() {
        let src = r#"namespace Web {
  public class Client {
    public void Go(HttpClient c) { c.Send(new HttpRequestMessage()); }
    public void Pub(Bus b) { b.BasicPublish("x", "y"); }
  }
}
"#;
        assert!(sites("csharp", "Client.cs", src).is_empty());
    }
}
