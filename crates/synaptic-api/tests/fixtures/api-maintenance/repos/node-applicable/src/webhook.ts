export function acceptCustomerWebhook(signature: string, body: string) {
  if (!signature.startsWith("fixture-signature")) throw new Error("invalid signature");
  return JSON.parse(body);
}
