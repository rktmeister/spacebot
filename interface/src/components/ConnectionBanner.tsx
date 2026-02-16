import { Banner } from "@/ui";
import type { ConnectionState } from "@/hooks/useEventSource";

const stateConfig: Record<Exclude<ConnectionState, "connected">, { label: string; variant: "info" | "warning" | "error" }> = {
	connecting: { label: "Connecting...", variant: "info" },
	reconnecting: { label: "Reconnecting... Dashboard may show stale data.", variant: "warning" },
	disconnected: { label: "Disconnected from server.", variant: "error" },
};

export function ConnectionBanner({ state, hasData }: { state: ConnectionState; hasData: boolean }) {
	// Don't show "Connecting..." if we already have data loaded
	if (state === "connecting" && hasData) return null;
	
	if (state === "connected") return null;

	const { label, variant } = stateConfig[state];

	return (
		<Banner variant={variant} dot="pulse">
			{label}
		</Banner>
	);
}
