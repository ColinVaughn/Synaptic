// WebSocketSharp service dispatching on the message type.
using WebSocketSharp.Server;

public class FeedService : WebSocketBehavior {
    protected void OnMessage(string type, string payload) {
        switch (type) {
            case "subscribe":
                Subscribe(payload);
                break;
        }
    }

    private void Subscribe(string payload) {}
}
