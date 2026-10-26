# ADR 0044: Messaging topology in code intelligence

Status: accepted and implemented (improvement roadmap P7).

## Context

Impact analysis followed calls, references and HTTP routes. Services that
talk through a message broker showed no connection at all: the publisher
of an event and its consumers live in different repositories, call no
common function, and share only a topic, queue or message type name.

## Decision

**Message endpoints are reference targets, like routes.** The graph
derivation `graph-4` records every broker site it recognises as a
`references` edge from the enclosing function, method or type to
`message:<key>` (`docmonk_core::messaging`):

| Key | Endpoint |
|---|---|
| `kafka/<topic>` | Kafka topic |
| `amqp/queue/<queue>` | RabbitMQ queue (also a publish to the default exchange) |
| `amqp/exchange/<exchange>` | RabbitMQ exchange; the routing key is in the evidence |
| `type/<MessageType>` | MassTransit message type (bare type name) |

The resolver says the framework and the role,
`messaging:<framework>:<publish\|consume\|bind\|declare>`. Keys and edge
ids are derived only from the file's text, so graphs stay byte-identical
across runs; the confidence is always `heuristic`.

**Rules** (`docmonk-code::messaging`), each active only in files that
mention the library, so look-alike calls of other APIs (`HttpClient.Send`)
are not read as broker sites:

* RabbitMQ.Client: `BasicPublish`, `BasicConsume`, `QueueBind` (recorded
  under both the exchange and the queue), `ExchangeDeclare`, and the
  `Async` forms.
* MassTransit: `Publish<T>(...)`, `Send<T>(...)`, `Publish(new T(...))`,
  `IConsumer<T>`.
* Confluent.Kafka: `Produce` / `ProduceAsync`, `Subscribe` (one topic or a
  list).
* kafkajs: `send({ topic })`, `subscribe({ topic | topics })`.
* Spring Kafka: `@KafkaListener(topics = ...)`, `KafkaTemplate.send(topic,
  ...)`.

An endpoint argument is read when it is a string literal or a constant
defined in the same file (`const string`, `static readonly string`,
`static final String`, JavaScript `const`), by its last dotted segment.

**Bindings are resolved at query time, across sources**
(`docmonk-retrieval::messaging`), the way exact URL occurrences are: one
batched, indexed `target_symbol` lookup per source for the endpoint keys a
name can stand for (topic, queue, exchange, and the message type of every
type the name matched). AMQP bindings are followed one hop. A queue bound
to an exchange contributes its consumers when the binding key matches a
routing key published to that exchange (topic rules: `*` one word, `#`
any), and a queue's lookup brings the publishers whose routing key matches
its binding. Nothing is stored across sources, so a source indexed alone
never has to update another.

**Surfaces.** `impact <name>` adds `publishers`, `consumers` and a
`messaging` object (every site with its source, path, line, framework,
endpoint and routing key) when the name is a message contract. The
publish and consume sites count toward the blast radius, and the text
view prints `Publishers:` / `Consumers:`. `explore` adds `publishes` /
`consumes` evidence and a `message_flows` key. MCP `docmonk_impact`
carries the same fields. Code without messaging keeps its impact shape
unchanged.

## Consequences

* Endpoint names computed at runtime, read from configuration, or defined
  as constants in another file are not seen. Neither are conventional
  routing (MassTransit endpoint-name formatters, Spring topic patterns),
  so a topic configured by convention has no flow. The evidence says what
  was read; nothing is guessed from names.
* Exchange types are not modelled: a binding key is matched as a topic
  pattern, an empty key as fanout. A direct exchange with a wildcard-like
  key would be over-matched.
* `graph-4` recomputes graphs (never content) on the first index after the
  upgrade; until then a source has no message edges.
* Rollback: revert. The rules are only graph rows, and graphs rebuild
  without them.
