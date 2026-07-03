# Kafka producer. publish_order sends to the "orders" topic the JS worker
# consumes; publish_audit sends to "audit", which nothing consumes.
from kafka import KafkaProducer

producer = KafkaProducer()


def publish_order(order):
    producer.send("orders", order)


def publish_audit(entry):
    producer.send("audit", entry)
