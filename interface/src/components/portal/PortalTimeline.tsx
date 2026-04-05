import {useEffect, useRef} from "react";
import {useQuery} from "@tanstack/react-query";
import {MessageBubble} from "@spacedrive/ai";
import {api, type TimelineItem, type WorkerListItem} from "@/api/client";
import {PortalWorkerCard} from "./PortalWorkerCard";

interface PortalTimelineProps {
	agentId: string;
	conversationId: string;
	timeline: TimelineItem[];
	isTyping: boolean;
}

function ThinkingIndicator() {
	return (
		<div className="flex items-center gap-1.5 py-1">
			<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-ink-faint" />
			<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-ink-faint [animation-delay:0.2s]" />
			<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-ink-faint [animation-delay:0.4s]" />
		</div>
	);
}

/** Synthesize a minimal WorkerListItem from a timeline item when the workers
 * list hasn't caught up yet. */
function synthesizeWorker(
	item: Extract<TimelineItem, {type: "worker_run"}>,
	channelId: string,
): WorkerListItem {
	return {
		id: item.id,
		task: item.task,
		status: item.status,
		started_at: item.started_at,
		completed_at: item.completed_at ?? null,
		channel_id: channelId,
		channel_name: null,
		has_transcript: true,
		worker_type: "builtin",
		tool_calls: 0,
		live_status: null,
		interactive: false,
		directory: null,
		opencode_port: null,
		opencode_session_id: null,
	};
}

export function PortalTimeline({
	agentId,
	conversationId,
	timeline,
	isTyping,
}: PortalTimelineProps) {
	const scrollRef = useRef<HTMLDivElement>(null);
	const previousLengthRef = useRef(0);

	// Fetch workers for this channel to resolve worker_run items.
	const workersQuery = useQuery({
		queryKey: ["portal-workers", agentId, conversationId],
		queryFn: () => api.workersList(agentId, {limit: 20}),
		enabled: Boolean(conversationId),
		refetchInterval: 2000,
	});

	// Filter to built-in workers for this conversation. OpenCode workers get
	// rendered in their own surface, not inline.
	const builtInWorkers = (workersQuery.data?.workers ?? []).filter(
		(w) => w.channel_id === conversationId && w.worker_type !== "opencode",
	);
	const builtInWorkerIds = new Set(builtInWorkers.map((w) => w.id));

	// Filter worker_run items to only those matching built-in workers we've seen.
	// Messages and other item types always render.
	const visibleItems = timeline.filter((item) => {
		if (item.type !== "worker_run") return true;
		return builtInWorkerIds.has(item.id);
	});

	// Smart auto-scroll: only when near bottom
	useEffect(() => {
		const element = scrollRef.current;
		if (!element) return;

		const previousLength = previousLengthRef.current;
		const currentLength = visibleItems.length;
		const distanceFromBottom =
			element.scrollHeight - element.scrollTop - element.clientHeight;
		const isNearBottom = distanceFromBottom < 160;
		const shouldAutoScroll =
			(currentLength > previousLength || isTyping) &&
			(previousLength === 0 || isNearBottom);

		if (shouldAutoScroll) {
			requestAnimationFrame(() => {
				element.scrollTo({top: element.scrollHeight, behavior: "auto"});
			});
		}

		previousLengthRef.current = currentLength;
	}, [visibleItems.length, isTyping]);

	const copyMessage = async (content: string) => {
		await navigator.clipboard.writeText(content);
	};

	return (
		<div ref={scrollRef} className="flex-1 overflow-x-hidden overflow-y-auto">
			<div className="mx-auto flex max-w-3xl flex-col gap-2 px-4 py-6 pb-[180px]">
				{visibleItems.map((item) => {
					if (item.type === "message") {
						return (
							<MessageBubble
								key={item.id}
								content={item.content}
								isUser={item.role === "user"}
								onCopy={(content) => void copyMessage(content)}
							/>
						);
					}
					if (item.type === "worker_run") {
						const worker =
							builtInWorkers.find((w) => w.id === item.id) ??
							synthesizeWorker(item, conversationId);
						return (
							<div key={item.id} className="py-2">
								<PortalWorkerCard agentId={agentId} worker={worker} />
							</div>
						);
					}
					return null;
				})}
				{isTyping && <ThinkingIndicator />}
			</div>
		</div>
	);
}
