import { CircleStop, CornerDownLeft, ImagePlus, Pencil, X } from "lucide-react";
import { LayoutGroup, motion } from "motion/react";
import { memo, useCallback, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import type {
  ConnectionStatus,
  OpenedSession,
  ProviderInfo,
  ReasoningEffort,
  UsageRecord,
} from "@/store/session";
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
  starting,
  evicted,
  workspace,
  selectingTarget,
  providers,
  providerId,
  reasoningEffort,
  usage,
  sessionHasImages,
  serverUrl,
  workspaceId,
  editing,
  seed,
  onSetModel,
  onSend,
  onAbort,
  onCancelEdit,
}: {
  status: ConnectionStatus;
  running: boolean;
  /** The session is being opened for its first task. Blocks send without
   * offering Abort — there is no turn to abort yet. An Enter landing in this
   * window returns before the draft is cleared, so the text survives for the
   * user to send again. */
  starting: boolean;
  /** Another client took over this session. The takeover mask covers pointer
   * access; these guards also close the keyboard/paste paths. */
  evicted: boolean;
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
  serverUrl: string;
  workspaceId: string;
  /** A historical message pulled back in to be rewritten. The parent remounts
   * this component whenever it changes, so these are read once as the initial
   * draft and owned locally from then on — except while `submitting`, when the
   * store's copy is the one that survives a remount and the local draft is
   * frozen to match. `target === null` means the rewind already happened and
   * this is now an ordinary draft. */
  editing?: NonNullable<OpenedSession["editing"]>;
  /** The prompt a fork branched away from, for the copy it landed in. Read once
   * at mount, like `editing` — the composer owns the draft from there. */
  seed?: NonNullable<OpenedSession["seed"]>;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
  onSend: (task: string, images: string[]) => void;
  onAbort: () => void;
  onCancelEdit: () => void;
}) {
  const [task, setTask] = useState(editing?.text ?? seed?.text ?? "");
  const [images, setImages] = useState<string[]>(editing?.images ?? seed?.images ?? []);
  const [lightboxIndex, setLightboxIndex] = useState<number | null>(null);
  const layoutGroupId = useId();
  const getImageLayoutId = useCallback(
    (index: number) => imageLightboxLayoutId(index, images[index]),
    [images],
  );
  const [dragOver, setDragOver] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const connected = status === "connected";
  // A submit in flight owns the draft: `editing.text`/`images` were frozen when
  // the request went out, and a reconnect can remount us from them at any
  // moment. Anything typed past that point would vanish without trace, so the
  // draft goes read-only until the request settles.
  const frozen = evicted || editing?.submitting === true;
  // Reading a file is asynchronous, so a paste or drop begun a moment before
  // the submit finishes after it — with `frozen` captured as it was at the
  // start. The guards below open the door; this is what checks it is still open
  // by the time the file is ready.
  const frozenRef = useRef(frozen);
  frozenRef.current = frozen;
  const acceptsImages =
    Boolean(providerId) &&
    (providers.find((p) => p.id === providerId)?.input_modalities?.includes("image") ?? false);
  const canAddImages = acceptsImages && !frozen && images.length < MAX_IMAGES;
  const imagesBlockSend = !acceptsImages && images.length > 0;
  // Once images are in play — staged in the draft or already in history — only a
  // vision-capable model can serve the turn, so text-only models are locked out.
  const requireImageModel = images.length > 0 || sessionHasImages;
  const rewriting = editing?.target != null;
  const canSend =
    connected &&
    Boolean(workspace) &&
    !running &&
    !starting &&
    !evicted &&
    !editing?.submitting &&
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
      if (dataUris.length === 0 || frozenRef.current) return;
      setImages((prev) => [...prev, ...dataUris].slice(0, MAX_IMAGES));
    },
    [images.length],
  );

  const removeImage = useCallback((index: number) => {
    setImages((prev) => prev.filter((_, i) => i !== index));
  }, []);

  const handlePaste = useCallback(
    (event: React.ClipboardEvent) => {
      if (!acceptsImages || frozen) return;
      const files = Array.from(event.clipboardData.items)
        .filter((item) => item.kind === "file" && ACCEPTED_TYPES.has(item.type))
        .map((item) => item.getAsFile())
        .filter((f): f is File => f !== null);
      if (files.length > 0) {
        event.preventDefault();
        void addFiles(files);
      }
    },
    [acceptsImages, frozen, addFiles],
  );

  const handleDrop = useCallback(
    (event: React.DragEvent) => {
      event.preventDefault();
      setDragOver(false);
      if (!acceptsImages || frozen) return;
      void addFiles(event.dataTransfer.files);
    },
    [acceptsImages, frozen, addFiles],
  );

  function submit() {
    if (!canSend) return;
    onSend(task.trim(), images);
    // While editing, clearing is the parent's job: dropping `editing` changes
    // our key and remounts us empty. Clearing here too would wipe the draft on
    // a *failed* submit — the one case where the user needs it back, since the
    // message it named may already be gone.
    if (!editing) {
      setTask("");
      setImages([]);
    }
  }

  return (
    <form
      className="bg-background p-2 sm:p-3"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <LayoutGroup id={layoutGroupId}>
        <div
          className="relative mx-auto max-w-4xl"
          onDragOver={(e) => {
            if (acceptsImages && !frozen) {
              e.preventDefault();
              setDragOver(true);
            }
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={handleDrop}
        >
          {editing && (
            <div className="mb-1.5 flex items-center gap-2 rounded-md border border-primary/40 bg-primary/5 px-2.5 py-1.5 text-xs">
              <Pencil className="size-3.5 shrink-0 text-primary" />
              <span className="min-w-0 flex-1 text-muted-foreground">
                {rewriting
                  ? "Editing an earlier message — sending discards everything after it."
                  : "That message is already gone; sending starts a new turn from here."}
              </span>
              <Button
                size="sm"
                variant="ghost"
                type="button"
                className="h-6 px-2"
                disabled={editing.submitting}
                onClick={onCancelEdit}
              >
                Cancel
              </Button>
            </div>
          )}
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
                    disabled={frozen}
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
              if (event.key === "Escape" && editing && !editing.submitting) {
                event.preventDefault();
                onCancelEdit();
              }
            }}
            onPaste={handlePaste}
            disabled={frozen}
            placeholder={
              evicted
                ? "Session opened in another window — take over to continue"
                : "Enter to send, Shift+Enter for newline"
            }
            className={[
              "min-h-[104px] pb-20 pr-3 sm:min-h-[80px] sm:pb-10",
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
          <div className="absolute bottom-2 left-2 right-2 flex flex-wrap items-center justify-end gap-1">
            {showControls && contextWindow ? (
              <ContextUsage contextWindow={contextWindow} records={usage} />
            ) : null}
            {showControls ? (
              <ModelSelector
                providers={providers}
                providerId={providerId}
                reasoningEffort={reasoningEffort}
                disabled={!connected || running}
                modelLocked={!selectingTarget}
                requireImageModel={requireImageModel}
                serverUrl={serverUrl}
                workspaceId={workspaceId}
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
