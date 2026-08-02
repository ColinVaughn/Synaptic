import Stripe from "stripe";
const stripe = new Stripe("fixture-key");

export function dynamicCustomerCall(member: string, payload: unknown) {
  return stripe.customers[member](payload);
}
