import { useState } from "react";
import { Eye, FilePen, Zap } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { PERMISSION_MODES, type PermissionMode } from "@/lib/protocol";

/** The fun name carries the identity; the line under it carries the meaning. */
const MODE_INFO: Record<PermissionMode, { label: string; description: string; Icon: typeof Eye }> =
  {
    explore: {
      label: "Explore",
      description: "Investigate and plan changes.",
      Icon: Eye,
    },
    accept_edits: {
      label: "Accept edits",
      description: "Automatically accept all file edits.",
      Icon: FilePen,
    },
    yolo: {
      label: "Yolo",
      description: "Maximum autonomy with minimal interruptions.",
      Icon: Zap,
    },
  };

export function PermissionSelector({
  mode,
  disabled,
  onSetMode,
}: {
  mode: PermissionMode;
  disabled: boolean;
  onSetMode: (mode: PermissionMode) => void;
}) {
  // Yolo hands over the shell unattended, so it is never something a stray
  // click (or a restored selection) can turn on — it takes a deliberate yes.
  const [confirmingYolo, setConfirmingYolo] = useState(false);
  const { label, Icon } = MODE_INFO[mode];
  const danger = mode === "yolo";

  return (
    <>
      <Select
        value={mode}
        onValueChange={(next) => {
          if (next === "yolo" && mode !== "yolo") {
            setConfirmingYolo(true);
            return;
          }
          onSetMode(next as PermissionMode);
        }}
        disabled={disabled}
      >
        <SelectTrigger
          size="sm"
          title="How much this session may do without asking"
          className={[
            "h-7 max-w-36 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70 sm:max-w-44 dark:bg-transparent dark:hover:bg-muted/70",
            danger ? "text-destructive" : "",
          ]
            .filter(Boolean)
            .join(" ")}
        >
          <Icon className="size-3.5 shrink-0" />
          <SelectValue placeholder="Permissions">
            {/* Hidden visually on narrow screens, never from a screen reader:
                `display: none` would drop the *current mode* from the trigger's
                accessible name, leaving the icon as the only cue that a session
                is in Yolo. */}
            <span className="sr-only sm:not-sr-only sm:inline">{label}</span>
          </SelectValue>
        </SelectTrigger>
        <SelectContent position="popper" side="top" className="max-w-72">
          {PERMISSION_MODES.map((item) => {
            const info = MODE_INFO[item];
            return (
              <SelectItem key={item} value={item}>
                <span className="flex items-center gap-2">
                  <info.Icon
                    className={`size-3.5 shrink-0 ${item === "yolo" ? "text-destructive" : ""}`}
                  />
                  <span className="min-w-0">
                    <span className="block">{info.label}</span>
                    <span className="block text-xs text-muted-foreground">{info.description}</span>
                  </span>
                </span>
              </SelectItem>
            );
          })}
        </SelectContent>
      </Select>
      <Dialog open={confirmingYolo} onOpenChange={setConfirmingYolo}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>Switch to Yolo?</DialogTitle>
            <DialogDescription>
              Coda can edit files and run commands without asking, including installing packages or
              accessing the network. Workspace approval rules still apply, but complex commands may
              bypass shell deny rules.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setConfirmingYolo(false)}>
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={() => {
                setConfirmingYolo(false);
                onSetMode("yolo");
              }}
            >
              <Zap className="size-4" />
              Enable Yolo
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
