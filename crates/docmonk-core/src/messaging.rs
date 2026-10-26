//! Message endpoints (ADR 0044): the broker topics, queues, exchanges and
//! message types that code publishes to and consumes from, written as
//! `message:<key>` reference targets so every backend finds them with one
//! indexed exact lookup (like `route:<key>` for HTTP routes).
//!
//! Keys, all deterministic and case-preserving:
//!
//! * `kafka/<topic>`
//! * `amqp/queue/<queue>`
//! * `amqp/exchange/<exchange>` (the routing key is in the evidence)
//! * `type/<MessageType>` (MassTransit: the message's bare type name)
//!
//! The role is the last segment of the resolver
//! (`messaging:<framework>:<role>`).

/// `target_symbol` prefix of a message endpoint reference.
pub const MESSAGE_PREFIX: &str = "message:";

/// What a code site does with an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Publish,
    Consume,
    /// A queue bound to an exchange (recorded under both endpoints).
    Bind,
    /// An exchange declared.
    Declare,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Publish => "publish",
            Role::Consume => "consume",
            Role::Bind => "bind",
            Role::Declare => "declare",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "publish" => Role::Publish,
            "consume" => Role::Consume,
            "bind" => Role::Bind,
            "declare" => Role::Declare,
            _ => return None,
        })
    }
}

/// The resolver label of a message reference.
pub fn resolver(framework: &str, role: Role) -> String {
    format!("messaging:{framework}:{}", role.as_str())
}

/// `(framework, role)` of a message reference's resolver.
pub fn parse_resolver(resolver: &str) -> Option<(&str, Role)> {
    let rest = resolver.strip_prefix("messaging:")?;
    let (framework, role) = rest.rsplit_once(':')?;
    Some((framework, Role::parse(role)?))
}

pub fn kafka_topic(topic: &str) -> String {
    format!("kafka/{topic}")
}
pub fn amqp_queue(queue: &str) -> String {
    format!("amqp/queue/{queue}")
}
pub fn amqp_exchange(exchange: &str) -> String {
    format!("amqp/exchange/{exchange}")
}
pub fn message_type(name: &str) -> String {
    let bare = name.rsplit(['.', ':']).next().unwrap_or(name);
    format!("type/{bare}")
}

/// The endpoint keys a free-text name may stand for (`orders.created`,
/// `OrderCreated`, or a full `message:` / bare key as printed by impact).
pub fn candidate_keys(name: &str) -> Vec<String> {
    let n = name.trim();
    if n.is_empty() {
        return Vec::new();
    }
    if let Some(k) = n.strip_prefix(MESSAGE_PREFIX) {
        return vec![k.to_owned()];
    }
    if ["kafka/", "amqp/queue/", "amqp/exchange/", "type/"]
        .iter()
        .any(|p| n.starts_with(p))
    {
        return vec![n.to_owned()];
    }
    vec![
        kafka_topic(n),
        amqp_queue(n),
        amqp_exchange(n),
        message_type(n),
    ]
}

/// Evidence of a message site, with the routing key and bound endpoint
/// when there are any: `publish amqp/exchange/orders routing key
/// "order.created"`. [`routing_key_of`] and [`bound_of`] read them back.
pub fn evidence(role: Role, key: &str, routing_key: Option<&str>, bound: Option<&str>) -> String {
    let mut e = format!("{} {key}", role.as_str());
    if let Some(rk) = routing_key {
        e += &format!(" routing key \"{rk}\"");
    }
    if let Some(b) = bound {
        e += &format!(" bound to {b}");
    }
    e
}

pub fn routing_key_of(evidence: &str) -> Option<&str> {
    let rest = evidence.split_once(" routing key \"")?.1;
    rest.split_once('"').map(|(rk, _)| rk)
}

pub fn bound_of(evidence: &str) -> Option<&str> {
    evidence
        .split_once(" bound to ")
        .map(|(_, b)| b.split_whitespace().next().unwrap_or(b))
}

/// AMQP topic-exchange matching: `*` is one word, `#` zero or more,
/// words separated by `.`. A binding without wildcards matches only the
/// same key; an empty binding key matches everything (fanout).
pub fn routing_key_matches(binding: &str, key: &str) -> bool {
    if binding.is_empty() || binding == "#" {
        return true;
    }
    fn go(b: &[&str], k: &[&str]) -> bool {
        match (b.first(), k.first()) {
            (None, None) => true,
            (Some(&"#"), _) => go(&b[1..], k) || (!k.is_empty() && go(b, &k[1..])),
            (Some(&"*"), Some(_)) => go(&b[1..], &k[1..]),
            (Some(x), Some(y)) if x == y => go(&b[1..], &k[1..]),
            _ => false,
        }
    }
    let b: Vec<&str> = binding.split('.').collect();
    let k: Vec<&str> = key.split('.').collect();
    go(&b, &k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolvers_and_evidence_round_trip() {
        let r = resolver("rabbitmq", Role::Bind);
        assert_eq!(r, "messaging:rabbitmq:bind");
        assert_eq!(parse_resolver(&r), Some(("rabbitmq", Role::Bind)));
        assert_eq!(parse_resolver("framework:x"), None);
        let e = evidence(
            Role::Bind,
            "amqp/exchange/orders",
            Some("order.*"),
            Some("amqp/queue/billing"),
        );
        assert_eq!(routing_key_of(&e), Some("order.*"));
        assert_eq!(bound_of(&e), Some("amqp/queue/billing"));
        assert_eq!(routing_key_of("publish kafka/x"), None);
    }

    #[test]
    fn topic_exchange_matching() {
        assert!(routing_key_matches("order.created", "order.created"));
        assert!(!routing_key_matches("order.created", "order.paid"));
        assert!(routing_key_matches("order.*", "order.paid"));
        assert!(!routing_key_matches("order.*", "order.paid.late"));
        assert!(routing_key_matches("order.#", "order.paid.late"));
        assert!(routing_key_matches("#.late", "order.paid.late"));
        assert!(routing_key_matches("", "anything"));
    }

    #[test]
    fn names_map_to_candidate_keys() {
        assert_eq!(
            candidate_keys("orders"),
            [
                "kafka/orders",
                "amqp/queue/orders",
                "amqp/exchange/orders",
                "type/orders"
            ]
        );
        assert_eq!(candidate_keys("message:kafka/x"), ["kafka/x"]);
        assert_eq!(
            candidate_keys("type/Shop.OrderCreated"),
            ["type/Shop.OrderCreated"]
        );
        assert_eq!(
            message_type("Shop.Contracts.OrderCreated"),
            "type/OrderCreated"
        );
    }
}
