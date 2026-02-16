import * as React from "react";
import { cx } from "./utils";

export type BannerVariant = "info" | "warning" | "error" | "success" | "cyan";

interface BannerProps extends React.HTMLAttributes<HTMLDivElement> {
  variant?: BannerVariant;
  dot?: "pulse" | "static" | "none";
  children: React.ReactNode;
}

const variantStyles: Record<BannerVariant, string> = {
  info: "bg-blue-500/10 text-blue-400 border-blue-500/20",
  warning: "bg-amber-500/10 text-amber-400 border-amber-500/20",
  error: "bg-red-500/10 text-red-400 border-red-500/20",
  success: "bg-green-500/10 text-green-400 border-green-500/20",
  cyan: "bg-cyan-500/10 text-cyan-400 border-cyan-500/20",
};

export function Banner({
  variant = "info",
  dot = "static",
  className,
  children,
  ...props
}: BannerProps) {
  return (
    <div
      className={cx(
        "border-b px-4 py-2 text-sm",
        variantStyles[variant],
        className
      )}
      {...props}
    >
      <div className="flex items-center gap-2">
        {dot !== "none" && (
          <div
            className={cx(
              "h-1.5 w-1.5 rounded-full bg-current",
              dot === "pulse" && "animate-pulse"
            )}
          />
        )}
        {children}
      </div>
    </div>
  );
}

export function BannerActions({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cx("ml-auto flex items-center gap-2", className)}>
      {children}
    </div>
  );
}
