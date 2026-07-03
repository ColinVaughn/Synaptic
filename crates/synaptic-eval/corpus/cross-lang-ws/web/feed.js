// Raw-ws client: subscribe() sends the "subscribe" command the C# service
// handles; sendPing() sends "ping", which no handler case matches.
const socket = new WebSocket('ws://svc/feed');

export function subscribe(topic) {
  socket.send({ type: 'subscribe', value: topic });
}

export function sendPing() {
  socket.send({ type: 'ping' });
}
