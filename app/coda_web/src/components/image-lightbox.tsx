import { Dialog, DialogContent } from "@/components/ui/dialog";

export function ImageLightbox({
  src,
  open,
  onClose,
}: {
  src: string;
  open: boolean;
  onClose: () => void;
}) {
  return (
    <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="flex max-h-[90vh] max-w-[90vw] items-center justify-center border-0 bg-black/90 p-2">
        <img
          src={src}
          alt="Full size preview"
          className="max-h-[86vh] max-w-[88vw] object-contain"
        />
      </DialogContent>
    </Dialog>
  );
}
