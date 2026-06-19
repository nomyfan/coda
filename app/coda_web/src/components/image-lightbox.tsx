import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, XIcon } from "lucide-react";
import { Dialog as DialogPrimitive } from "radix-ui";

const OPEN_EASING = "cubic-bezier(0.22, 1, 0.36, 1)";
const OVERLAY_OPEN_DURATION = 300;
const OVERLAY_CLOSE_DURATION = 380;
const OPEN_DURATION = 520;
const CLOSE_DURATION = 440;

function rectKeyframe(rect: DOMRect): Keyframe {
  return {
    left: `${rect.left}px`,
    top: `${rect.top}px`,
    width: `${rect.width}px`,
    height: `${rect.height}px`,
  };
}

function thumbRadius(el: HTMLElement): string {
  const image = el.querySelector("img");
  return getComputedStyle(image ?? el).borderRadius;
}

function createTransitionImage(src: string, rect: DOMRect, radius: string): HTMLImageElement {
  const image = document.createElement("img");
  image.src = src;
  Object.assign(image.style, {
    ...rectKeyframe(rect),
    borderRadius: radius,
    margin: "0",
    maxHeight: "none",
    maxWidth: "none",
    objectFit: "cover",
    pointerEvents: "none",
    position: "fixed",
    zIndex: "60",
  });
  document.body.appendChild(image);
  return image;
}

class LightboxAnimator {
  private openAnim: Animation | null = null;
  private transitionImage: HTMLImageElement | null = null;
  private opened = false;

  cleanup() {
    this.cancelOpenAnimation();
    this.removeTransitionImage();
  }

  openOverlay(overlay: HTMLElement | null) {
    overlay?.animate([{ opacity: 0 }, { opacity: 1 }], {
      duration: OVERLAY_OPEN_DURATION,
      easing: "ease-out",
    });
  }

  open(image: HTMLImageElement, src: string, thumb: HTMLElement | null) {
    if (this.opened || this.openAnim) return;
    this.opened = true;

    const to = image.getBoundingClientRect();
    const from = thumb?.getBoundingClientRect();

    if (!from || !thumb) {
      image.style.opacity = "1";
      this.openAnim = image.animate(
        [
          { transform: "scale(0.96)", opacity: 0 },
          { transform: "none", opacity: 1 },
        ],
        { duration: 240, easing: "ease-out", fill: "both" },
      );
      return;
    }

    this.removeTransitionImage();
    const transitionImage = createTransitionImage(src, from, thumbRadius(thumb));
    this.transitionImage = transitionImage;
    this.openAnim = transitionImage.animate(
      [
        { ...rectKeyframe(from), opacity: 1, borderRadius: thumbRadius(thumb), offset: 0 },
        {
          ...rectKeyframe(to),
          opacity: 1,
          borderRadius: getComputedStyle(image).borderRadius,
          offset: 1,
        },
      ],
      { duration: OPEN_DURATION, easing: OPEN_EASING, fill: "forwards" },
    );
    this.openAnim.onfinish = () => {
      image.style.opacity = "1";
      requestAnimationFrame(() => this.removeTransitionImageIfCurrent(transitionImage));
      this.openAnim = null;
    };
    this.openAnim.oncancel = () => {
      this.removeTransitionImage();
      this.openAnim = null;
    };
  }

  close(image: HTMLImageElement, src: string, thumb: HTMLElement | null, onFinish: () => void) {
    this.cancelOpenAnimation();

    const from = image.getBoundingClientRect();
    const dest = thumb?.getBoundingClientRect();

    if (!dest || !thumb) {
      const anim = image.animate(
        [
          { transform: "none", opacity: 1 },
          { transform: "scale(0.96)", opacity: 0 },
        ],
        { duration: 240, easing: "ease-in", fill: "forwards" },
      );
      anim.onfinish = onFinish;
      anim.oncancel = onFinish;
      return;
    }

    image.style.opacity = "0";
    this.removeTransitionImage();
    const transitionImage = createTransitionImage(src, from, getComputedStyle(image).borderRadius);
    this.transitionImage = transitionImage;
    const anim = transitionImage.animate(
      [
        {
          ...rectKeyframe(from),
          opacity: 1,
          borderRadius: getComputedStyle(image).borderRadius,
          offset: 0,
        },
        { ...rectKeyframe(dest), opacity: 1, borderRadius: thumbRadius(thumb), offset: 1 },
      ],
      { duration: CLOSE_DURATION, easing: OPEN_EASING, fill: "forwards" },
    );
    const finish = () => {
      this.removeTransitionImage();
      onFinish();
    };
    anim.onfinish = finish;
    anim.oncancel = finish;
  }

  closeOverlay(overlay: HTMLElement | null) {
    overlay?.animate([{ opacity: 1 }, { opacity: 0 }], {
      duration: OVERLAY_CLOSE_DURATION,
      easing: "ease-in",
      fill: "forwards",
    });
  }

  private cancelOpenAnimation() {
    if (!this.openAnim) return;
    this.openAnim.oncancel = null;
    this.openAnim.onfinish = null;
    this.openAnim.cancel();
    this.openAnim = null;
  }

  private removeTransitionImage() {
    this.transitionImage?.remove();
    this.transitionImage = null;
  }

  private removeTransitionImageIfCurrent(image: HTMLImageElement) {
    if (this.transitionImage === image) {
      this.removeTransitionImage();
    } else {
      image.remove();
    }
  }
}

export function ImageLightbox({
  images,
  initialIndex,
  onClose,
  getThumbEl,
}: {
  images: string[];
  /** Index to open at. Seeds the initial view; navigation is self-managed. */
  initialIndex: number;
  onClose: () => void;
  /** The thumbnail element at `i`, used as the zoom origin/target. */
  getThumbEl?: (i: number) => HTMLElement | null;
}) {
  const count = images.length;
  const [current, setCurrent] = useState(initialIndex);
  const [closing, setClosing] = useState(false);
  const imgRef = useRef<HTMLImageElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const animatorRef = useRef(new LightboxAnimator());

  // Zoom a temporary image from the clicked thumbnail into the measured lightbox
  // image slot. The real image appears after the transition image lands.
  const playOpen = () => {
    const img = imgRef.current;
    if (img)
      animatorRef.current.open(img, images[initialIndex], getThumbEl?.(initialIndex) ?? null);
  };

  useLayoutEffect(() => {
    animatorRef.current.openOverlay(overlayRef.current);
    // Cached images are already complete and won't fire `onLoad`.
    const img = imgRef.current;
    if (img?.complete && img.naturalWidth > 0) playOpen();
    // Run once for the opening transition.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const animator = animatorRef.current;
    return () => {
      animator.cleanup();
    };
  }, []);

  const requestClose = () => {
    if (closing) return;
    setClosing(true);
    animatorRef.current.closeOverlay(overlayRef.current);
    const img = imgRef.current;
    if (!img) {
      onClose();
      return;
    }
    animatorRef.current.close(img, src, getThumbEl?.(current) ?? null, onClose);
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
            className="max-h-[60vh] max-w-[60vw] rounded-md object-contain opacity-0"
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
