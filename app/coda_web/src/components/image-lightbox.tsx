import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { ChevronLeft, ChevronRight, XIcon } from "lucide-react";
import { Dialog as DialogPrimitive } from "radix-ui";

export const IMAGE_LIGHTBOX_TRANSITION = {
  type: "spring",
  stiffness: 360,
  damping: 38,
  mass: 0.9,
} as const;

const OVERLAY_TRANSITION = { duration: 0.28, ease: "easeOut" } as const;

export function imageLightboxLayoutId(index: number, src: string | undefined): string {
  const value = src ?? "";
  let hash = 5381;
  for (let i = 0; i < value.length; i += 1) {
    hash = (hash * 33) ^ value.charCodeAt(i);
  }
  return `image-${index}-${(hash >>> 0).toString(36)}`;
}

export function ImageLightbox({
  images,
  initialIndex,
  onClose,
  getLayoutId,
}: {
  images: string[];
  /** Index to open at. Seeds the initial view; navigation is self-managed. */
  initialIndex: number;
  onClose: () => void;
  getLayoutId?: (i: number) => string;
}) {
  const count = images.length;
  const [current, setCurrent] = useState(initialIndex);
  const [closing, setClosing] = useState(false);
  const closingRef = useRef(false);

  const requestClose = () => {
    if (closingRef.current) return;
    closingRef.current = true;
    setClosing(true);
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

  const layoutId = getLayoutId?.(current);

  return (
    <DialogPrimitive.Root open onOpenChange={(v) => !v && requestClose()}>
      <DialogPrimitive.Portal>
        <AnimatePresence onExitComplete={onClose}>
          {!closing
            ? [
                <DialogPrimitive.Overlay key="overlay" asChild>
                  <motion.div
                    className="fixed inset-0 z-50 bg-black/80"
                    initial={{ opacity: 0 }}
                    animate={{ opacity: 1 }}
                    exit={{ opacity: 0 }}
                    transition={OVERLAY_TRANSITION}
                  />
                </DialogPrimitive.Overlay>,
                /* Full-viewport flex container with no transform, so the fixed controls
                  resolve against the viewport. Clicking the backdrop around the image
                  closes the lightbox. */
                <DialogPrimitive.Content key="content" asChild>
                  <motion.div
                    className="fixed inset-0 z-50 flex select-none items-center justify-center p-8 outline-none"
                    onClick={requestClose}
                    initial={false}
                    exit={{ opacity: 1 }}
                    transition={{ duration: 0.44 }}
                  >
                    <DialogPrimitive.Title className="sr-only">Image preview</DialogPrimitive.Title>
                    <motion.img
                      key={current}
                      layoutId={layoutId}
                      src={src}
                      alt={`Image ${current + 1} of ${count}`}
                      draggable={false}
                      className="max-h-[60vh] max-w-[60vw] rounded-md object-contain"
                      initial={layoutId ? false : { opacity: 0, scale: 0.96 }}
                      animate={{ opacity: 1, scale: 1 }}
                      exit={layoutId ? undefined : { opacity: 0, scale: 0.96 }}
                      transition={IMAGE_LIGHTBOX_TRANSITION}
                      onClick={(e) => e.stopPropagation()}
                    />
                  </motion.div>
                </DialogPrimitive.Content>,
                <motion.div
                  key="controls"
                  className="fixed inset-0 z-50 pointer-events-none select-none"
                  initial={{ opacity: 0 }}
                  animate={{ opacity: 1 }}
                  exit={{ opacity: 0 }}
                  transition={{ duration: 0.1 }}
                >
                  <DialogPrimitive.Close className="pointer-events-auto fixed top-4 right-4 z-50 flex size-9 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white">
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
                        className="pointer-events-auto fixed top-1/2 left-4 z-50 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white"
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
                        className="pointer-events-auto fixed top-1/2 right-4 z-50 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-black/60 text-white transition-colors hover:bg-black/80 focus:outline-none focus-visible:ring-2 focus-visible:ring-white"
                      >
                        <ChevronRight className="size-6" />
                      </button>
                      <div className="fixed bottom-4 left-1/2 z-50 -translate-x-1/2 rounded-full bg-black/60 px-3 py-1 text-sm text-white">
                        {current + 1} / {count}
                      </div>
                    </>
                  )}
                </motion.div>,
              ]
            : null}
        </AnimatePresence>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
