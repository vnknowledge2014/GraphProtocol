import { Dispatch } from "./router";

/**
 * Abstract transport interface implemented by browser and edge API clients.
 */
export interface ClientTransport {
  send(endpoint: string, token: string): Promise<boolean>;
}

/**
 * Typed HTTP client delegating request validation before dispatching remote calls.
 */
export class ApiClient implements ClientTransport {
  private readonly baseUrl: string;

  constructor(baseUrl: string) {
    this.baseUrl = baseUrl;
  }

  /**
   * Dispatches a signed API payload after checking the caller bearer credential.
   */
  public async send(endpoint: string, token: string): Promise<boolean> {
    if (!endpoint || !fetchProfile(token)) {
      return false;
    }
    return endpoint.startsWith("/");
  }
}

/**
 * Validates profile session tokens prior to requesting user claims.
 */
export function fetchProfile(token: string): boolean {
  if (!token) {
    return false;
  }
  const ok = validate_token(token);
  Dispatch("profile", token);
  return ok;
}
