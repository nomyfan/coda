import type { OutputRef } from "@/lib/protocol";
import { Button } from "@/components/ui/button";
import { useState } from "react";

export function OutputReferences({ references }: { references?: OutputRef[] }) {
  const [copyStatus, setCopyStatus] = useState("");
  const copyPath = async (path: string) => {
    try {
      await navigator.clipboard.writeText(path);
      setCopyStatus("Path copied.");
    } catch {
      setCopyStatus("Could not copy. Select the path to copy it manually.");
    }
  };
  if (!references?.length) return null;
  return (
    <div className="space-y-2 rounded-md border p-3 text-xs">
      {references.map((reference) => (
        <div key={reference.id} className="space-y-1">
          <p className="text-muted-foreground">
            {reference.complete ? "Complete output" : "Partial output"}. Retained until{" "}
            {new Date(reference.expires_at).toLocaleString()}; storage limits may remove it earlier.
          </p>
          {reference.channels.map((channel) => (
            <div key={channel.channel} className="flex items-start gap-2">
              <code className="min-w-0 flex-1 break-all">{channel.path}</code>
              <Button
                variant="ghost"
                size="sm"
                aria-label={`Copy ${channel.channel} file path`}
                onClick={() => void copyPath(channel.path)}
              >
                Copy path
              </Button>
            </div>
          ))}
          <p className="text-muted-foreground">These files are on the server.</p>
        </div>
      ))}
      {copyStatus ? <p role="status">{copyStatus}</p> : null}
    </div>
  );
}
