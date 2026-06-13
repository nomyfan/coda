import { CircleStop, Folder, PlugZap, Send } from "lucide-react";
import { memo, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import type {
  ConnectionStatus,
  ProviderInfo,
  ReasoningEffort,
  ServerSummary,
} from "@/store/session";
import { serverLabel } from "@/components/session-utils";
import { ModelSelector } from "@/components/model-selector";

export const Composer = memo(function Composer({
  status,
  running,
  server,
  servers,
  workspace,
  workspaces,
  selectingTarget,
  providers,
  providerId,
  reasoningEffort,
  onSetModel,
  onChangeServer,
  onChangeWorkspace,
  onSend,
  onAbort,
}: {
  status: ConnectionStatus;
  running: boolean;
  server?: string;
  servers: ServerSummary[];
  workspace?: string;
  workspaces: string[];
  selectingTarget: boolean;
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  onSetModel: (
    providerId: string,
    reasoningEffort: ReasoningEffort | null
  ) => void;
  onChangeServer: (serverUrl: string) => void;
  onChangeWorkspace: (workspaceId: string) => void;
  onSend: (task: string) => void;
  onAbort: () => void;
}) {
  const [task, setTask] = useState("");
  const connected = status === "connected";
  const canSend =
    connected && Boolean(workspace) && !running && Boolean(task.trim());
  const selectableServers = servers.filter((item) => item.catalog.length > 0);

  function submit() {
    const text = task.trim();
    if (!text || !canSend) {
      return;
    }
    onSend(text);
    setTask("");
  }

  return (
    <form
      className="bg-background/95 p-3 backdrop-blur"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      {selectingTarget ? (
        <div className="mx-auto mb-2 flex max-w-4xl flex-wrap items-center gap-2">
          <Select
            value={server}
            onValueChange={onChangeServer}
            disabled={selectableServers.length === 0}
          >
            <SelectTrigger
              size="sm"
              className="w-44 gap-1.5 rounded-md text-xs"
            >
              <PlugZap className="size-3.5 text-muted-foreground" />
              <SelectValue placeholder="Server" />
            </SelectTrigger>
            <SelectContent position="popper" side="top">
              {selectableServers.map((item) => (
                <SelectItem key={item.url} value={item.url}>
                  {serverLabel(item)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={workspace}
            onValueChange={onChangeWorkspace}
            disabled={!connected || workspaces.length === 0}
          >
            <SelectTrigger
              size="sm"
              className="w-36 gap-1.5 rounded-md text-xs"
            >
              <Folder className="size-3.5 text-muted-foreground" />
              <SelectValue placeholder="Workspace" />
            </SelectTrigger>
            <SelectContent position="popper" side="top">
              {workspaces.map((id) => (
                <SelectItem key={id} value={id}>
                  {id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <ModelSelector
            providers={providers}
            providerId={providerId}
            reasoningEffort={reasoningEffort}
            disabled={!connected || running}
            onSetModel={onSetModel}
          />
        </div>
      ) : workspace ? (
        <div className="mx-auto mb-2 flex max-w-4xl flex-wrap items-center gap-2">
          <Select value={workspace} onValueChange={onChangeWorkspace}>
            <SelectTrigger
              size="sm"
              className="w-auto gap-1.5 rounded-md text-xs"
              disabled={!connected || workspaces.length === 0}
            >
              <Folder className="size-3.5 text-muted-foreground" />
              <SelectValue />
            </SelectTrigger>
            <SelectContent position="popper" side="top">
              {workspaces.map((id) => (
                <SelectItem key={id} value={id}>
                  {id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <ModelSelector
            providers={providers}
            providerId={providerId}
            reasoningEffort={reasoningEffort}
            disabled={!connected || running}
            onSetModel={onSetModel}
          />
        </div>
      ) : null}
      <div className="relative mx-auto max-w-4xl">
        <Textarea
          value={task}
          onChange={(event) => setTask(event.target.value)}
          onKeyDown={(event) => {
            if (
              event.key === "Enter" &&
              !event.shiftKey &&
              !event.nativeEvent.isComposing
            ) {
              event.preventDefault();
              submit();
            }
          }}
          placeholder="Ask Coda to edit, inspect, test, or explain...  (Enter to send, Shift+Enter for newline)"
          className="min-h-[52px] pr-12"
        />
        {running ? (
          <Button
            size="icon"
            variant="secondary"
            className="absolute bottom-2 right-2 size-8"
            type="button"
            onClick={onAbort}
            disabled={!connected}
            title="Abort"
          >
            <CircleStop />
          </Button>
        ) : (
          <Button
            size="icon"
            className="absolute bottom-2 right-2 size-8"
            type="submit"
            disabled={!canSend}
            title="Send"
          >
            <Send />
          </Button>
        )}
      </div>
    </form>
  );
});
