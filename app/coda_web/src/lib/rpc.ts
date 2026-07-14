import {
  JSONRPCClient,
  JSONRPCErrorException,
  JSONRPCServer,
  JSONRPCServerAndClient,
} from "json-rpc-2.0";

/**
 * A thin adapter over `json-rpc-2.0` bound to one WebSocket. The library owns id
 * allocation, the pending-request map, and response correlation — we write no
 * id-mapping code. This adapter only shapes two behaviors the store needs:
 * `notify` returns a boolean the caller can gate an optimistic update on, and
 * inbound frames are fed in through `receive`.
 */
export type RpcClient = {
  /**
   * Send a request and resolve with its typed result. Rejects with a
   * `JSONRPCErrorException` — carrying `.code` (an `RpcCode`), `.message`, and
   * `.data` — on a server error, or with a `CONNECTION_DROPPED_CODE` exception
   * when the socket dropped before the reply (see `isServerError`).
   */
  request<T>(method: string, params?: unknown): Promise<T>;
  /**
   * Fire-and-forget. Returns `false` (and sends nothing) when the socket is
   * absent or not `OPEN`, so the caller can withhold an optimistic update for a
   * message that never left the client; `true` once handed to the library.
   */
  notify(method: string, params?: unknown): boolean;
  /** Register a server-push handler (`event` / `snapshot` / `session_evicted`). */
  addMethod(method: string, handler: (params: unknown) => void): void;
  /** Feed one inbound frame (a response or a server push) to the library. */
  receive(payload: unknown): void;
  /** Reject every awaiting request so no caller hangs after the socket closes. */
  rejectAll(reason: string): void;
};

/**
 * The code `json-rpc-2.0` assigns when a send fails or a pending request is
 * force-rejected on socket close (its `DefaultErrorCode`). It is distinct from
 * every real server `RpcCode`, so it cleanly marks "connection dropped" versus a
 * genuine typed error — the distinction the delete-tombstone flow depends on.
 */
export const CONNECTION_DROPPED_CODE = 0;

/**
 * True when `err` is a genuine server error (a real `RpcCode`), as opposed to a
 * dropped-connection rejection. Call-site error branches should use this before
 * reading `.code`, so an ambiguous disconnect isn't mistaken for a definite
 * server verdict.
 */
export function isServerError(err: unknown): err is JSONRPCErrorException {
  return err instanceof JSONRPCErrorException && err.code !== CONNECTION_DROPPED_CODE;
}

export function createRpcClient(socket: WebSocket): RpcClient {
  const serverAndClient = new JSONRPCServerAndClient(
    new JSONRPCServer(),
    new JSONRPCClient((payload) => {
      // Rejecting the send settles an awaiting request instead of hanging it;
      // `notify` never reaches here while closed (it pre-checks readyState).
      if (socket.readyState !== WebSocket.OPEN) {
        return Promise.reject(new Error("connection is not open"));
      }
      socket.send(JSON.stringify(payload));
    }),
  );

  return {
    request: <T>(method: string, params?: unknown): Promise<T> =>
      serverAndClient.request(method, params, undefined) as Promise<T>,
    notify: (method: string, params?: unknown): boolean => {
      if (socket.readyState !== WebSocket.OPEN) {
        return false;
      }
      serverAndClient.notify(method, params, undefined);
      return true;
    },
    addMethod: (method, handler) => serverAndClient.addMethod(method, handler),
    receive: (payload) => {
      void serverAndClient.receiveAndSend(payload, undefined, undefined);
    },
    rejectAll: (reason) => serverAndClient.rejectAllPendingRequests(reason),
  };
}
