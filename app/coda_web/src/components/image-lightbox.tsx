import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, XIcon } from "lucide-react";
import { Dialog as DialogPrimitive } from "radix-ui";

const OPEN_EASING = "cubic-bezier(0.22, 1, 0.36, 1)";

// Transform that maps an element currently laid out at `to` so it visually
// covers `from` (FLIP). transform-origin is the element center.
function flipTransform(from: DOMRect, to: DOMRect): string {
  const sx = from.width / to.width;
  const sy = from.height / to.height;
  const tx = from.left + from.width / 2 - (to.left + to.width / 2);
  const ty = from.top + from.height / 2 - (to.top + to.height / 2);
  return `translate(${tx}px, ${ty}px) scale(${sx}, ${sy})`;
}

export function ImageLightbox({
  images,
  initialIndex,
  onClose,
  getThumbRect,
}: {
  images: string[];
  /** Index to open at. Seeds the initial view; navigation is self-managed. */
  initialIndex: number;
  onClose: () => void;
  /** Screen rect of the thumbnail at `i`, used as the zoom origin/target. */
  getThumbRect?: (i: number) => DOMRect | null;
}) {
  const count = images.length;
  const [current, setCurrent] = useState(initialIndex);
  const [closing, setClosing] = useState(false);
  const imgRef = useRef<HTMLImageElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const openAnimRef = useRef<Animation | null>(null);

  // Zoom the image up from the clicked thumbnail. Deferred until the image has
  // loaded — before that the element has no layout size to measure against, and
  // the image is kept transparent so no full-size frame flashes first. `fill`
  // holds the keyframes outside the active phase to drive that reveal.
  const playOpen = () => {
    const img = imgRef.current;
    if (openAnimRef.current || !img) return;
    const to = img.getBoundingClientRect();
    const from = getThumbRect?.(initialIndex);
    openAnimRef.current = img.animate(
      from
        ? [
            { transform: flipTransform(from, to), opacity: 0.5 },
            { transform: "none", opacity: 1 },
          ]
        : [
            { transform: "scale(0.9)", opacity: 0 },
            { transform: "none", opacity: 1 },
          ],
      { duration: 520, easing: OPEN_EASING, fill: "both" },
    );
  };

  useLayoutEffect(() => {
    overlayRef.current?.animate([{ opacity: 0 }, { opacity: 1 }], {
      duration: 300,
      easing: "ease-out",
    });
    // Cached images are already complete and won't fire `onLoad`.
    const img = imgRef.current;
    if (img?.complete && img.naturalWidth > 0) playOpen();
    // Run once for the opening transition.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const requestClose = () => {
    if (closing) return;
    setClosing(true);
    overlayRef.current?.animate([{ opacity: 1 }, { opacity: 0 }], {
      duration: 300,
      easing: "ease-in",
      fill: "forwards",
    });
    const img = imgRef.current;
    if (!img) {
      onClose();
      return;
    }
    // Drop the held open animation so it doesn't fight the closing one.
    openAnimRef.current?.cancel();
    openAnimRef.current = null;
    // Zoom back to the thumbnail of the image currently being viewed.
    const to = img.getBoundingClientRect();
    const dest = getThumbRect?.(current);
    const anim = img.animate(
      dest
        ? [
            { transform: "none", opacity: 1 },
            { transform: flipTransform(dest, to), opacity: 0.5 },
          ]
        : [
            { transform: "none", opacity: 1 },
            { transform: "scale(0.9)", opacity: 0 },
          ],
      { duration: 340, easing: OPEN_EASING, fill: "forwards" },
    );
    anim.onfinish = onClose;
    anim.oncancel = onClose;
  };

  const goPrev = () => setCurrent((c) => (c - 1 + count) % count);
  const goNext = () => setCurrent((c) => (c + 1) % count);

  useEffect(() => {
    if (count <= 1) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowLeft") setCurrent((c) => (c - 1 + count) % count);
      else if (e.key === "ArrowRight") setCurrent((c) => (c + 1) % count);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [count]);

  const src = images[current];
  if (!src) return null;

  return (
    <DialogPrimitive.Root open onOpenChange={(v) => !v && requestClose()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay ref={overlayRef} className="fixed inset-0 z-50 bg-black/80" />
        {/* Full-viewport flex container with no transform, so the fixed controls
            resolve against the viewport (not a transformed box).
            Clicking the backdrop around the image closes the lightbox. */}
        <DialogPrimitive.Content
          className="fixed inset-0 z-50 flex select-none items-center justify-center p-8 outline-none"
          onClick={requestClose}
        >
          <DialogPrimitive.Title className="sr-only">Image preview</DialogPrimitive.Title>
          <img
            ref={imgRef}
            src={src}
            alt={`Image ${current + 1} of ${count}`}
            draggable={false}
            className="max-h-[60vh] max-w-[60vw] rounded-md object-contain"
            // Hidden until the open animation reveals it (see playOpen), so no
            // full-size frame flashes before the zoom-from-thumbnail begins.
            style={{ opacity: 0 }}
            onLoad={playOpen}
            onClick={(e) => e.stopPropagation()}
          />
          {!closing && (
            <>
              <DialogPrimitive.Close className="fixed top-4 right-4 z-50 flex size-9 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white">
                <XIcon className="size-5" />
                <span className="sr-only">Close</span>
              </DialogPrimitive.Close>
              {count > 1 && (
                <>
                  <button
                    type="button"
                    aria-label="Previous image"
                    onClick={(e) => {
                      e.stopPropagation();
                      goPrev();
                    }}
                    className="fixed top-1/2 left-4 z-50 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white"
                  >
                    <ChevronLeft className="size-6" />
                  </button>
                  <button
                    type="button"
                    aria-label="Next image"
                    onClick={(e) => {
                      e.stopPropagation();
                      goNext();
                    }}
                    className="fixed top-1/2 right-4 z-50 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white"
                  >
                    <ChevronRight className="size-6" />
                  </button>
                  <div className="fixed bottom-4 left-1/2 z-50 -translate-x-1/2 rounded-full bg-black/60 px-3 py-1 text-sm text-white">
                    {current + 1} / {count}
                  </div>
                </>
              )}
            </>
          )}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
