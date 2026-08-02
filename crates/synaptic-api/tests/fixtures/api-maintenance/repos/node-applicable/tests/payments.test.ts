import { onboardCustomer } from "../src/wrapper";

export function customerMigrationTest() {
  return onboardCustomer("fixture@example.test");
}
