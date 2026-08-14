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
import { PERMISSION_PRESETS, type PermissionPreset } from "@/lib/protocol";

/** The fun name carries the identity; the line under it carries the meaning. */
const PRESET_INFO: Record<
  PermissionPreset,
  { label: string; description: string; Icon: typeof Eye }
> = {
  explore: {
    label: "Explore",
    description: "Read, search and list. Everything else asks first.",
    Icon: Eye,
  },
  accept_edits: {
    label: "Accept edits",
    description: "Also writes and edits files without asking.",
    Icon: FilePen,
  },
  yolo: {
    label: "Yolo",
    description: "Runs everything unattended, shell included.",
    Icon: Zap,
  },
};

export function PermissionSelector({
  preset,
  disabled,
  onSetPreset,
}: {
  preset: PermissionPreset;
  disabled: boolean;
  onSetPreset: (preset: PermissionPreset) => void;
}) {
  // Yolo hands over the shell unattended, so it is never something a stray
  // click (or a restored selection) can turn on — it takes a deliberate yes.
  const [confirmingYolo, setConfirmingYolo] = useState(false);
  const { label, Icon } = PRESET_INFO[preset];
  const danger = preset === "yolo";

  return (
    <>
      <Select
        value={preset}
        onValueChange={(next) => {
          if (next === "yolo" && preset !== "yolo") {
            setConfirmingYolo(true);
            return;
          }
          onSetPreset(next as PermissionPreset);
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
          <SelectValue placeholder="Permissions">{label}</SelectValue>
        </SelectTrigger>
        <SelectContent position="popper" side="top" className="max-w-72">
          {PERMISSION_PRESETS.map((item) => {
            const info = PRESET_INFO[item];
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
              Every tool runs without asking, including shell commands that write files, install
              packages or reach the network. Only the workspace&rsquo;s deny rules still apply.
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
                onSetPreset("yolo");
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
