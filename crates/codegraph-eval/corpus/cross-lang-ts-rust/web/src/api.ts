// TS frontend client: calls the POST /session route the Rust backend serves.

export async function createSession(): Promise<Response> {
    return fetch("/session", { method: "POST" });
}
