const { Kafka } = require('kafkajs');

const kafka = new Kafka({ clientId: 'web', brokers: ['localhost:9092'] });

async function run() {
  const consumer = kafka.consumer({ groupId: 'web' });
  await consumer.subscribe({ topics: ['orders.events'], fromBeginning: true });
}

module.exports = { run };
