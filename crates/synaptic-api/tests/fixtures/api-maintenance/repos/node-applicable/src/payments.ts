import Stripe from "stripe";

const stripe = new Stripe("fixture-key");

export function createCustomerWithSdk(email: string) {
  return stripe.customers.create({ email });
}

export function createCustomerDirect(email: string) {
  return fetch("https://api.stripe.com/v1/customers", {
    method: "POST",
    body: JSON.stringify({ email }),
  });
}
