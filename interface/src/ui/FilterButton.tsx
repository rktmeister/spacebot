import * as React from "react";
import { cx } from "./utils";

export interface FilterButtonProps
	extends React.ButtonHTMLAttributes<HTMLButtonElement> {
	active?: boolean;
	colorClass?: string;
	children: React.ReactNode;
}

export const FilterButton = React.forwardRef<
	HTMLButtonElement,
	FilterButtonProps
>(({ active, colorClass, className, children, ...props }, ref) => (
	<button
		ref={ref}
		className={cx(
			"h-6 rounded-md px-2 text-tiny font-medium transition-colors",
			active
				? colorClass || "bg-app-selected text-ink"
				: "text-ink-faint hover:text-ink-dull",
			className
		)}
		{...props}
	>
		{children}
	</button>
));

FilterButton.displayName = "FilterButton";
