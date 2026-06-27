import { CircleStop, CornerDownLeft, ImagePlus, X } from "lucide-react";
import { LayoutGroup, motion } from "motion/react";
import { memo, useCallback, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import type { ConnectionStatus, ProviderInfo, ReasoningEffort, UsageRecord } from "@/store/session";
import { ModelSelector } from "@/components/model-selector";
import { ContextUsage } from "@/components/context-usage";
import {
  ImageLightbox,
  IMAGE_LIGHTBOX_TRANSITION,
  imageLightboxLayoutId,
} from "@/components/image-lightbox";

const MAX_IMAGES = 5;
const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
const ACCEPTED_TYPES = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);

function toDataUri(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(new Error("Failed to read file"));
    reader.readAsDataURL(file);
  });
}

export const Composer = memo(function Composer({
  status,
  running,
  workspace,
  selectingTarget,
  providers,
  providerId,
  reasoningEffort,
  usage,
  sessionHasImages,
  onSetModel,
  onSend,
  onAbort,
}: {
  status: ConnectionStatus;
  running: boolean;
  workspace?: string;
  /** New-session mode: the send target is still being picked in the header. */
  selectingTarget: boolean;
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  usage: UsageRecord[];
  /** The active session's history already carries image attachments, so a
   * text-only model can no longer serve this conversation. */
  sessionHasImages: boolean;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
  onSend: (task: string, images: string[]) => void;
  onAbort: () => void;
}) {
  const [task, setTask] = useState("");
  const [images, setImages] = useState<string[]>([]);
  const [lightboxIndex, setLightboxIndex] = useState<number | null>(null);
  const layoutGroupId = useId();
  const getImageLayoutId = useCallback(
    (index: number) => imageLightboxLayoutId(index, images[index]),
    [images],
  );
  const [dragOver, setDragOver] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const connected = status === "connected";
  const acceptsImages =
    Boolean(providerId) &&
    (providers.find((p) => p.id === providerId)?.input_modalities?.includes("image") ?? false);
  const canAddImages = acceptsImages && images.length < MAX_IMAGES;
  const imagesBlockSend = !acceptsImages && images.length > 0;
  // Once images are in play — staged in the draft or already in history — only a
  // vision-capable model can serve the turn, so text-only models are locked out.
  const requireImageModel = images.length > 0 || sessionHasImages;
  const canSend =
    connected &&
    Boolean(workspace) &&
    !running &&
    !imagesBlockSend &&
    (Boolean(task.trim()) || images.length > 0);
  const showControls = selectingTarget || Boolean(workspace);
  const contextWindow = providers.find((provider) => provider.id === providerId)?.context_window;

  const addFiles = useCallback(
    async (files: FileList | File[]) => {
      const fileArr = Array.from(files);
      const available = MAX_IMAGES - images.length;
      if (available <= 0) return;

      const accepted = fileArr
        .filter((f) => ACCEPTED_TYPES.has(f.type))
        .filter((f) => f.size <= MAX_IMAGE_BYTES)
        .slice(0, available);

      // allSettled so one unreadable file doesn't drop the rest or surface as
      // an unhandled rejection (callers fire this without awaiting).
      const results = await Promise.allSettled(accepted.map(toDataUri));
      const dataUris = results
        .filter((r): r is PromiseFulfilledResult<string> => r.status === "fulfilled")
        .map((r) => r.value);
      if (dataUris.length === 0) return;
      setImages((prev) => [...prev, ...dataUris].slice(0, MAX_IMAGES));
    },
    [images.length],
  );

  const removeImage = useCallback((index: number) => {
    setImages((prev) => prev.filter((_, i) => i !== index));
  }, []);

  const handlePaste = useCallback(
    (event: React.ClipboardEvent) => {
      if (!acceptsImages) return;
      const files = Array.from(event.clipboardData.items)
        .filter((item) => item.kind === "file" && ACCEPTED_TYPES.has(item.type))
        .map((item) => item.getAsFile())
        .filter((f): f is File => f !== null);
      if (files.length > 0) {
        event.preventDefault();
        void addFiles(files);
      }
    },
    [acceptsImages, addFiles],
  );

  const handleDrop = useCallback(
    (event: React.DragEvent) => {
      event.preventDefault();
      setDragOver(false);
      if (!acceptsImages) return;
      void addFiles(event.dataTransfer.files);
    },
    [acceptsImages, addFiles],
  );

  function submit() {
    if (!canSend) return;
    onSend(task.trim(), images);
    setTask("");
    setImages([]);
  }

  return (
    <form
      className="bg-background p-3"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <LayoutGroup id={layoutGroupId}>
        <div
          className="relative mx-auto max-w-4xl"
          onDragOver={(e) => {
            if (acceptsImages) {
              e.preventDefault();
              setDragOver(true);
            }
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={handleDrop}
        >
          {images.length > 0 && (
            <div className="mb-1.5 flex flex-wrap gap-2">
              {images.map((src, index) => (
                <div key={index} className="group relative">
                  <button
                    type="button"
                    className="block"
                    title="View full size"
                    aria-label={`View attachment ${index + 1} full size`}
                    onClick={() => setLightboxIndex(index)}
                  >
                    <motion.img
                      layoutId={getImageLayoutId(index)}
                      transition={IMAGE_LIGHTBOX_TRANSITION}
                      src={src}
                      alt={`Attachment ${index + 1}`}
                      className="h-16 w-16 rounded-md border border-border object-cover shadow-sm"
                    />
                  </button>
                  <button
                    type="button"
                    className="absolute -right-1.5 -top-1.5 flex size-4 items-center justify-center rounded-full bg-muted text-muted-foreground opacity-0 transition-opacity hover:bg-foreground hover:text-background group-hover:opacity-100"
                    title="Remove image"
                    aria-label={`Remove attachment ${index + 1}`}
                    onClick={() => removeImage(index)}
                  >
                    <X className="size-2.5" />
                  </button>
                </div>
              ))}
            </div>
          )}
          <Textarea
            value={task}
            onChange={(event) => setTask(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                submit();
              }
            }}
            onPaste={handlePaste}
            placeholder="Ask Coda to edit, inspect, test, or explain...  (Enter to send, Shift+Enter for newline)"
            className={[
              "min-h-[80px] pb-10 pr-3",
              dragOver ? "border-primary ring-1 ring-primary" : "",
            ]
              .filter(Boolean)
              .join(" ")}
          />
          <input
            ref={fileInputRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif"
            multiple
            className="hidden"
            onChange={(e) => {
              if (e.target.files) {
                void addFiles(e.target.files);
              }
              e.target.value = "";
            }}
          />
          <div className="absolute bottom-2 left-2 right-2 flex items-center justify-end gap-1">
            {showControls && contextWindow ? (
              <ContextUsage contextWindow={contextWindow} records={usage} />
            ) : null}
            {showControls ? (
              <ModelSelector
                providers={providers}
                providerId={providerId}
                reasoningEffort={reasoningEffort}
                disabled={!connected || running}
                requireImageModel={requireImageModel}
                onSetModel={onSetModel}
              />
            ) : null}
            {acceptsImages && (
              <Button
                size="icon"
                variant="ghost"
                className="size-8 rounded-md"
                type="button"
                title={
                  images.length >= MAX_IMAGES ? `Maximum ${MAX_IMAGES} images` : "Attach images"
                }
                disabled={!canAddImages}
                onClick={() => fileInputRef.current?.click()}
              >
                <ImagePlus className="size-4" />
              </Button>
            )}
            {running ? (
              <Button
                size="icon"
                variant="secondary"
                className="size-8 rounded-md"
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
                className="size-8 rounded-md"
                type="submit"
                disabled={!canSend}
                title={imagesBlockSend ? "Selected model does not support images" : "Send"}
              >
                <CornerDownLeft />
              </Button>
            )}
          </div>
        </div>
        {imagesBlockSend && (
          <p className="mx-auto mt-1 max-w-4xl text-xs text-destructive">
            The selected model does not support images. Switch to a vision-capable model or remove
            the attached images.
          </p>
        )}
        {lightboxIndex !== null && (
          <ImageLightbox
            images={images}
            initialIndex={lightboxIndex}
            getLayoutId={getImageLayoutId}
            onClose={() => setLightboxIndex(null)}
          />
        )}
      </LayoutGroup>
    </form>
  );
});
