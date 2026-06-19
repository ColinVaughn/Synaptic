import { route } from "./router";

export function validate(path: string): boolean {
    return path.length > 0;
}

export function handleRequest(path: string): number {
    if (validate(path)) {
        return route(path);
    }
    return 0;
}
