using System.Threading.Tasks;
using Confluent.Kafka;
using MassTransit;
using RabbitMQ.Client;
using Shop.Contracts;

namespace Orders
{
    public static class Topics
    {
        public const string OrderEvents = "orders.events";
    }

    public class OrderService
    {
        private const string Exchange = "orders";
        private readonly IModel _channel;
        private readonly IPublishEndpoint _bus;
        private readonly IProducer<string, string> _producer;

        public OrderService(IModel channel, IPublishEndpoint bus, IProducer<string, string> producer)
        {
            _channel = channel;
            _bus = bus;
            _producer = producer;
        }

        public void Setup()
        {
            _channel.ExchangeDeclare(Exchange, "topic");
        }

        public void PlaceOrder(byte[] body)
        {
            _channel.BasicPublish(exchange: Exchange, routingKey: "order.created", basicProperties: null, body: body);
        }

        public Task AnnounceAsync(string id)
        {
            return _bus.Publish(new OrderPlaced(id));
        }

        public Task StreamAsync(string payload)
        {
            return _producer.ProduceAsync(Topics.OrderEvents, new Message<string, string> { Value = payload });
        }
    }
}
