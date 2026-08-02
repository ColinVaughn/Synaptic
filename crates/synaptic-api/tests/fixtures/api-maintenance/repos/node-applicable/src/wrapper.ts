import { createCustomerWithSdk } from "./payments";

export function onboardCustomer(email: string) {
  return createCustomerWithSdk(email);
}
