// TS frontend client. createSession calls POST /session (served by the backend).
// getSessions calls /sessions (note the plural) which the backend does NOT serve,
// so it must remain unconnected -- a distractor for cross-language precision.

export async function createSession(): Promise<Response> {
    return fetch("/session", { method: "POST" });
}

export async function getSessions(): Promise<Response> {
    return fetch("/sessions", { method: "GET" });
}
