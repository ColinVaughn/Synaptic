import Pager from "pager-sdk";
const pager = new Pager();

export function triggerWithSdk(summary: string) {
  return pager.incidents.trigger({ summary });
}

export function triggerDirect(summary: string) {
  return fetch("https://events.pager.example/incidents", {
    method: "POST",
    body: JSON.stringify({ summary }),
  });
}
