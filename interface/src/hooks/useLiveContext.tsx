import { createContext, useContext, useCallback, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type ChannelInfo } from "@/api/client";
import { useEventSource, type ConnectionState } from "@/hooks/useEventSource";
import { useChannelLiveState, type ChannelLiveState } from "@/hooks/useChannelLiveState";

interface LiveContextValue {
	liveStates: Record<string, ChannelLiveState>;
	channels: ChannelInfo[];
	connectionState: ConnectionState;
	hasData: boolean;
	loadOlderMessages: (channelId: string) => void;
}

const LiveContext = createContext<LiveContextValue>({
	liveStates: {},
	channels: [],
	connectionState: "connecting",
	hasData: false,
	loadOlderMessages: () => {},
});

export function useLiveContext() {
	return useContext(LiveContext);
}

export function LiveContextProvider({ children }: { children: ReactNode }) {
	const queryClient = useQueryClient();

	const { data: channelsData } = useQuery({
		queryKey: ["channels"],
		queryFn: api.channels,
		refetchInterval: 10_000,
	});

	const channels = channelsData?.channels ?? [];
	const { liveStates, handlers, syncStatusSnapshot, loadOlderMessages } = useChannelLiveState(channels);

	const onReconnect = useCallback(() => {
		syncStatusSnapshot();
		queryClient.invalidateQueries({ queryKey: ["channels"] });
		queryClient.invalidateQueries({ queryKey: ["status"] });
		queryClient.invalidateQueries({ queryKey: ["agents"] });
	}, [syncStatusSnapshot, queryClient]);

	const { connectionState } = useEventSource(api.eventsUrl, {
		handlers,
		onReconnect,
	});

	// Consider app "ready" once we have any data loaded
	const hasData = channels.length > 0 || channelsData !== undefined;

	return (
		<LiveContext.Provider value={{ liveStates, channels, connectionState, hasData, loadOlderMessages }}>
			{children}
		</LiveContext.Provider>
	);
}
