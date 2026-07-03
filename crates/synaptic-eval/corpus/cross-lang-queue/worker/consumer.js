// kafkajs consumer for the "orders" topic.
const { Kafka } = require('kafkajs');
const kafka = new Kafka({ brokers: ['b:9092'] });
const consumer = kafka.consumer({ groupId: 'workers' });

async function run() {
  await consumer.subscribe({ topic: 'orders' });
  await consumer.run({ eachMessage: async ({ message }) => message });
}

run();
