import type { ConnectionStatus, ServerState } from "@/lib/session";

export const statusCopy: Record<
  ConnectionStatus,
  { label: string; tone: "secondary" | "success" | "warning" | "danger" }
> = {
  idle: { label: "Ready", tone: "secondary" },
  connecting: { label: "Connecting", tone: "warning" },
  connected: { label: "Connected", tone: "success" },
  closed: { label: "Disconnected", tone: "secondary" },
  error: { label: "Error", tone: "danger" },
};

export function sessionTitle(session: {
  id: string;
  first_user_message?: string | null;
}) {
  return session.first_user_message?.trim() || session.id;
}

export function serverLabel(server: ServerState) {
  return server.alias || server.url;
}
