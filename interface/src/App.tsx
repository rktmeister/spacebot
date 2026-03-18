import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ReactQueryDevtools } from "@tanstack/react-query-devtools";
import { RouterProvider } from "@tanstack/react-router";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { ConnectionScreen } from "@/components/ConnectionScreen";
import { LiveContextProvider } from "@/hooks/useLiveContext";
import { ServerProvider, useServer } from "@/hooks/useServer";
import { router } from "@/router";

const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			staleTime: 30_000,
			retry: 1,
			refetchOnWindowFocus: true,
		},
	},
});

/**
 * Inner shell: shows the connection screen until the server is
 * reachable, then renders the main app with live data.
 */
function AppShell() {
	const { state, hasConnected } = useServer();

	// Show connection screen if we've never connected, or if we lost
	// connection before any data was loaded.
	if (state !== "connected" && !hasConnected) {
		return <ConnectionScreen />;
	}

	return (
		<LiveContextProvider>
			<RouterProvider router={router} />
		</LiveContextProvider>
	);
}

export function App() {
	return (
		<ErrorBoundary>
			<QueryClientProvider client={queryClient}>
				<ServerProvider>
					<AppShell />
				</ServerProvider>
				{import.meta.env.DEV && (
					<ReactQueryDevtools initialIsOpen={false} buttonPosition="bottom-right" />
				)}
			</QueryClientProvider>
		</ErrorBoundary>
	);
}
