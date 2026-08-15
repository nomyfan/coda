import { CircleStop, CornerDownLeft, ImagePlus, Pencil, X } from "lucide-react";
import { LayoutGroup, motion } from "motion/react";
import { memo, useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import {
  applyMention,
  detectTrigger,
  emptyMentionLabel,
  type MentionItem,
  type MentionTrigger,
} from "@/lib/composer-mentions";
import { MentionMenu, useMentionItems } from "@/components/mention-menu";
import type {
  ConnectionStatus,
  OpenedSession,
  PermissionMode,
  ProviderInfo,
  ReasoningEffort,
  UsageRecord,
} from "@/store/session";
import { ModelSelector } from "@/components/model-selector";
import { PermissionSelector } from "@/components/permission-selector";
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
  approvalPending,
  starting,
  evicted,
  workspace,
  selectingTarget,
  permissionMode,
  providers,
  providerId,
  reasoningEffort,
  usage,
  sessionHasImages,
  serverUrl,
  workspaceId,
  editing,
  forkDraft,
  onForkDraftChange,
  onSetModel,
  onSetPermissionMode,
  onSend,
  onAbort,
  onCancelEdit,
}: {
  status: ConnectionStatus;
  running: boolean;
  /** A suspended turn is idle computationally but still owns the session. */
  approvalPending: boolean;
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
  /** How much this session may do unattended, and the control to change it. */
  permissionMode: PermissionMode;
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
  /** The prompt a fork branched away from, persisted with the copy while the
   * composer owns it. */
  forkDraft?: NonNullable<OpenedSession["forkDraft"]>;
  onForkDraftChange: (text: string, images: string[]) => void;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
  onSetPermissionMode: (mode: PermissionMode) => void;
  onSend: (task: string, images: string[]) => void;
  onAbort: () => void;
  onCancelEdit: () => void;
}) {
  const [task, setTask] = useState(editing?.text ?? forkDraft?.text ?? "");
  const [images, setImages] = useState<string[]>(editing?.images ?? forkDraft?.images ?? []);
  const [lightboxIndex, setLightboxIndex] = useState<number | null>(null);
  const layoutGroupId = useId();
  const getImageLayoutId = useCallback(
    (index: number) => imageLightboxLayoutId(index, images[index]),
    [images],
  );
  const [dragOver, setDragOver] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const hasForkDraft = forkDraft !== undefined;
  // The `@` / `/` token the caret is in, if any, and which entry of its menu is
  // selected. `dismissedStart` remembers the one Escape closed, so the menu
  // stays shut while the caret is still in that token.
  const [trigger, setTrigger] = useState<MentionTrigger | null>(null);
  const [highlighted, setHighlighted] = useState(0);
  const [dismissedStart, setDismissedStart] = useState<number | null>(null);
  // Where the caret goes once React has rendered an accepted completion; the DOM
  // still holds the pre-insertion text at the moment the item is picked.
  const pendingCaret = useRef<number | null>(null);
  const mentionListId = useId();
  const mentionOptionId = useCallback(
    (index: number) => `${mentionListId}-option-${index}`,
    [mentionListId],
  );

  useEffect(() => {
    if (hasForkDraft) {
      onForkDraftChange(task, images);
    }
  }, [hasForkDraft, images, onForkDraftChange, task]);

  const connected = status === "connected";
  const busy = running || approvalPending;
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
    !busy &&
    !starting &&
    !evicted &&
    !editing?.submitting &&
    !imagesBlockSend &&
    (Boolean(task.trim()) || images.length > 0);
  const showControls = selectingTarget || Boolean(workspace);
  const contextWindow = providers.find((provider) => provider.id === providerId)?.context_window;

  // The pickers search a workspace over a live connection, and they stay out of
  // a frozen draft entirely — there is nothing useful to insert into text the
  // user cannot edit.
  const mentionsEnabled = connected && !frozen && Boolean(workspaceId);
  const mentionResults = useMentionItems({
    trigger,
    serverUrl,
    workspaceId,
    enabled: mentionsEnabled,
  });
  const mentionItems = mentionResults.items;
  const mentionOpen = trigger !== null && trigger.start !== dismissedStart;
  // The highlight follows a list that changes under it, so it is clamped rather
  // than reset: a shrinking result set keeps a valid selection.
  const highlightedIndex =
    mentionItems.length === 0 ? -1 : Math.min(highlighted, mentionItems.length - 1);
  const highlightedItem = highlightedIndex >= 0 ? mentionItems[highlightedIndex] : undefined;

  /** Re-read the caret's surroundings after anything that could move it. */
  const syncTrigger = useCallback(
    (element: HTMLTextAreaElement) => {
      const next = mentionsEnabled
        ? detectTrigger(element.value, element.selectionStart ?? element.value.length)
        : null;
      setTrigger(next);
      // Leaving the token clears the dismissal, so deleting a mention and
      // typing a fresh one opens the menu again.
      if (!next) {
        setDismissedStart(null);
      }
    },
    [mentionsEnabled],
  );

  // A new token, or a new query within it, means a new result set — start at the
  // top of it.
  useEffect(() => {
    setHighlighted(0);
  }, [trigger?.kind, trigger?.start, trigger?.query]);

  const acceptMention = useCallback(
    (item: MentionItem) => {
      if (!trigger) {
        return;
      }
      const applied = applyMention(task, trigger, item);
      setTask(applied.text);
      pendingCaret.current = applied.caret;
      setTrigger(null);
    },
    [task, trigger],
  );

  // Placing the caret has to wait for the inserted text to be in the DOM. The
  // re-sync afterwards is what keeps a directory's menu open on its new query
  // while a file's menu closes on the space that follows it.
  useLayoutEffect(() => {
    const caret = pendingCaret.current;
    const element = textareaRef.current;
    if (caret === null || !element) {
      return;
    }
    pendingCaret.current = null;
    element.focus();
    element.setSelectionRange(caret, caret);
    syncTrigger(element);
  });

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
    // The draft the menu was completing is on its way out; nothing about the
    // text that follows will move the caret, so close it here.
    setTrigger(null);
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
          {mentionOpen && trigger ? (
            <MentionMenu
              results={mentionResults}
              activeIndex={highlightedIndex}
              listId={mentionListId}
              optionId={mentionOptionId}
              emptyLabel={emptyMentionLabel(trigger)}
              onSelect={acceptMention}
              onHover={setHighlighted}
            />
          ) : null}
          <Textarea
            ref={textareaRef}
            value={task}
            onChange={(event) => {
              setTask(event.target.value);
              syncTrigger(event.target);
            }}
            // Arrow keys, Home/End and clicks move the caret without changing
            // the text, and the menu follows the caret.
            onKeyUp={(event) => syncTrigger(event.currentTarget)}
            onClick={(event) => syncTrigger(event.currentTarget)}
            onBlur={() => setTrigger(null)}
            onKeyDown={(event) => {
              // While the menu is open it owns the navigation keys — but never
              // mid-composition, where Enter is the IME committing a candidate.
              if (mentionOpen && !event.nativeEvent.isComposing) {
                if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                  event.preventDefault();
                  if (mentionItems.length > 0) {
                    const step = event.key === "ArrowDown" ? 1 : -1;
                    const from = highlightedIndex < 0 ? 0 : highlightedIndex;
                    setHighlighted((from + step + mentionItems.length) % mentionItems.length);
                  }
                  return;
                }
                if ((event.key === "Enter" || event.key === "Tab") && highlightedItem) {
                  event.preventDefault();
                  acceptMention(highlightedItem);
                  return;
                }
                if (event.key === "Escape") {
                  // Dismiss the menu only; an Escape meant for the edit banner
                  // is the *second* one, once the menu is gone.
                  event.preventDefault();
                  setDismissedStart(trigger?.start ?? null);
                  return;
                }
              }
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
            aria-autocomplete="list"
            aria-expanded={mentionOpen}
            aria-controls={mentionOpen ? mentionListId : undefined}
            aria-activedescendant={
              mentionOpen && highlightedIndex >= 0 ? mentionOptionId(highlightedIndex) : undefined
            }
            placeholder={
              evicted
                ? "Session opened in another window — take over to continue"
                : "Enter to send, Shift+Enter for newline, @ for files, / for skills"
            }
            className={[
              "min-h-[104px] pb-10 pr-3 sm:min-h-[80px]",
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
          <div className="absolute bottom-2 left-2 right-2 flex flex-wrap items-center gap-1">
            <div className="flex min-w-0 items-center gap-1">
              {showControls ? (
                <PermissionSelector
                  mode={permissionMode}
                  // Switchable whenever the session can hear it: the server
                  // rebuilds nothing, so mid-turn and awaiting-approval are
                  // both fine — and are exactly when the user wants it.
                  disabled={!connected || evicted}
                  onSetMode={onSetPermissionMode}
                />
              ) : null}
            </div>
            <div className="ml-auto flex min-w-0 items-center gap-1">
              {showControls && contextWindow ? (
                <ContextUsage contextWindow={contextWindow} records={usage} />
              ) : null}
              {showControls ? (
                <ModelSelector
                  providers={providers}
                  providerId={providerId}
                  reasoningEffort={reasoningEffort}
                  disabled={!connected || busy}
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
              {busy ? (
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
