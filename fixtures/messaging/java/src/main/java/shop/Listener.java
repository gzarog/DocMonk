package shop;

import org.springframework.kafka.annotation.KafkaListener;

public class Listener {
    static final String TOPIC = "orders.events";

    @KafkaListener(topics = TOPIC, groupId = "java")
    public void onEvent(String payload) {
    }
}
