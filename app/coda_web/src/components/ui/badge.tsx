import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center rounded-md border px-2 py-0.5 text-xs font-medium transition-colors",
  {
    variants: {
      variant: {
        default: "border-transparent bg-primary text-primary-foreground",
        secondary: "border-transparent bg-secondary text-secondary-foreground",
        outline: "text-foreground",
        success: "border-emerald-500/30 bg-emerald-500/12 text-emerald-700 dark:text-emerald-300",
        warning: "border-amber-500/30 bg-amber-500/12 text-amber-700 dark:text-amber-300",
        danger: "border-rose-500/30 bg-rose-500/12 text-rose-700 dark:text-rose-300",
        cyan: "border-cyan-500/30 bg-cyan-500/12 text-cyan-700 dark:text-cyan-300",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>, VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant }), className)} {...props} />;
}

export { Badge, badgeVariants };
