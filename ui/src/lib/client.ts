import { createApiClient } from './api';
import { tokenStore } from './auth';

type Listener = () => void;
const unauthorizedListeners = new Set<Listener>();

/** Notified when any admin API call comes back 401 (session expired or token revoked). */
export function onUnauthorized(fn: Listener): () => void {
  unauthorizedListeners.add(fn);
  return () => unauthorizedListeners.delete(fn);
}

export const api = createApiClient({
  getToken: () => tokenStore.get(),
  onUnauthorized: () => unauthorizedListeners.forEach((fn) => fn()),
});
