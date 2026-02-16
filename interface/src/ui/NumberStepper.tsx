import * as React from "react";
import { Button } from "./Button";
import { PlusSignIcon, MinusSignIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

export interface NumberStepperProps {
	label?: string;
	description?: string;
	value: number;
	onChange: (value: number) => void;
	min?: number;
	max?: number;
	step?: number;
	suffix?: string;
	type?: "integer" | "float";
	showProgress?: boolean;
	variant?: "default" | "compact";
}

export const NumberStepper = React.forwardRef<HTMLDivElement, NumberStepperProps>(
	(
		{
			label,
			description,
			value,
			onChange,
			min,
			max,
			step = 1,
			suffix,
			type = "integer",
			showProgress = false,
			variant = "default",
		},
		ref
	) => {
		const safeValue = value ?? 0;

		const clamp = (v: number) => {
			let clamped = v;
			if (min !== undefined && clamped < min) clamped = min;
			if (max !== undefined && clamped > max) clamped = max;
			if (type === "float") {
				clamped = Math.round(clamped / step) * step;
			}
			return clamped;
		};

		const increment = () => onChange(clamp(safeValue + step));
		const decrement = () => onChange(clamp(safeValue - step));

		const handleInput = (e: React.ChangeEvent<HTMLInputElement>) => {
			const raw = e.target.value;
			if (type === "integer" && (raw === "" || raw === "-")) return;
			if (type === "float" && (raw === "" || raw === "0." || raw === ".")) return;
			const parsed = Number(raw);
			if (!Number.isNaN(parsed)) onChange(clamp(parsed));
		};

		const displayValue =
			type === "float" ? safeValue.toFixed(2) : safeValue.toString();
		const inputWidth = variant === "compact" 
			? (type === "float" ? "w-12" : "w-14")
			: (type === "float" ? "w-16" : "w-20");

		const progress =
			showProgress && min !== undefined && max !== undefined
				? ((safeValue - min) / (max - min)) * 100
				: 0;

		return (
			<div ref={ref} className="flex flex-col gap-1.5">
				{label && <label className="text-sm font-medium text-ink">{label}</label>}
				{description && <p className="text-tiny text-ink-faint">{description}</p>}
				<div className={`flex items-center gap-2.5 ${(label || description) ? 'mt-1' : ''}`}>
					<div className="flex items-stretch rounded-md border border-app-line/50 bg-app-darkBox/30 overflow-hidden">
						<Button
							type="button"
							onClick={decrement}
							variant="ghost"
							size="icon"
							className="h-8 w-8 rounded-none"
						>
							<HugeiconsIcon icon={MinusSignIcon} className="h-3 w-3" />
						</Button>
						<input
							type="text"
							inputMode={type === "float" ? "decimal" : "numeric"}
							value={displayValue}
							onChange={handleInput}
							className={`${inputWidth} border-x border-app-line/50 bg-transparent px-2 py-1.5 text-center font-mono text-sm text-ink-dull focus:outline-none`}
						/>
						<Button
							type="button"
							onClick={increment}
							variant="ghost"
							size="icon"
							className="h-8 w-8 rounded-none"
						>
							<HugeiconsIcon icon={PlusSignIcon} className="h-3 w-3" />
						</Button>
					</div>
					{showProgress && (
						<div className="h-1.5 w-32 overflow-hidden rounded-full bg-app-darkBox">
							<div
								className="h-full rounded-full bg-accent/50 transition-all"
								style={{ width: `${progress}%` }}
							/>
						</div>
					)}
					{suffix && <span className="text-tiny text-ink-faint">{suffix}</span>}
				</div>
			</div>
		);
	}
);

NumberStepper.displayName = "NumberStepper";
