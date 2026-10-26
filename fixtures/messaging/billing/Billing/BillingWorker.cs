using System.Threading.Tasks;
using MassTransit;
using RabbitMQ.Client;
using Shop.Contracts;

namespace Billing
{
    public class BillingWorker
    {
        private readonly IModel _channel;

        public BillingWorker(IModel channel)
        {
            _channel = channel;
        }

        public void Start(IBasicConsumer consumer)
        {
            _channel.QueueBind(queue: "billing", exchange: "orders", routingKey: "order.*");
            _channel.BasicConsume(queue: "billing", autoAck: true, consumer: consumer);
        }

        public void StartAudit(IBasicConsumer consumer)
        {
            _channel.QueueBind(queue: "audit", exchange: "orders", routingKey: "invoice.#");
            _channel.BasicConsume(queue: "audit", autoAck: true, consumer: consumer);
        }
    }

    public class OrderPlacedConsumer : IConsumer<OrderPlaced>
    {
        public Task Consume(ConsumeContext<OrderPlaced> context)
        {
            return Task.CompletedTask;
        }
    }
}
