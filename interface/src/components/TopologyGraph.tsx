import { useCallback, useMemo, useRef, useState } from "react";
import {
	ReactFlow,
	Background,
	Controls,
	type Node,
	type Edge,
	type Connection,
	type NodeTypes,
	type EdgeTypes,
	type NodeProps,
	type EdgeProps,
	useNodesState,
	useEdgesState,
	MarkerType,
	Handle,
	Position,
	BaseEdge,
	getSmoothStepPath,
	ReactFlowProvider,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { AnimatePresence, motion } from "framer-motion";
import {
	api,
	type AgentSummary,
	type TopologyResponse,
	type LinkDirection,
	type LinkRelationship,
} from "@/api/client";
import { Button } from "@/ui";
import { Link } from "@tanstack/react-router";

// -- Colors --

const RELATIONSHIP_COLORS: Record<string, string> = {
	peer: "#6366f1",
	superior: "#f59e0b",
	subordinate: "#22c55e",
};

const RELATIONSHIP_LABELS: Record<string, string> = {
	peer: "Peer",
	superior: "Superior",
	subordinate: "Subordinate",
};

/** Deterministic gradient from a seed string. */
function seedGradient(seed: string): [string, string] {
	let hash = 0;
	for (let i = 0; i < seed.length; i++) {
		hash = seed.charCodeAt(i) + ((hash << 5) - hash);
		hash |= 0;
	}
	const hue1 = (hash >>> 0) % 360;
	const hue2 = (hue1 + 40 + ((hash >>> 8) % 60)) % 360;
	return [`hsl(${hue1}, 70%, 55%)`, `hsl(${hue2}, 60%, 45%)`];
}

// -- Custom Node: Agent Profile Card --

const NODE_WIDTH = 240;

function AgentNode({ data, selected }: NodeProps) {
	const avatarSeed = (data.avatarSeed as string) ?? (data.agentId as string);
	const [c1, c2] = seedGradient(avatarSeed);
	const displayName = (data.displayName as string) ?? (data.agentId as string);
	const status = data.status as string | null;
	const bio = data.bio as string | null;
	const isOnline = data.isOnline as boolean;
	const channelCount = data.channelCount as number;
	const memoryCount = data.memoryCount as number;
	const agentId = data.agentId as string;

	return (
		<div
			className={`relative rounded-xl border bg-app-darkBox transition-all ${
				selected
					? "border-accent shadow-lg shadow-accent/20"
					: "border-app-line hover:border-app-line/80"
			}`}
			style={{ width: NODE_WIDTH }}
		>
			{/* Gradient banner */}
			<div className="relative h-12 overflow-hidden rounded-t-xl">
				<svg
					className="absolute inset-0 h-full w-full"
					preserveAspectRatio="none"
					viewBox="0 0 240 48"
				>
					<defs>
						<linearGradient
							id={`node-grad-${agentId}`}
							x1="0%"
							y1="0%"
							x2="100%"
							y2="100%"
						>
							<stop offset="0%" stopColor={c1} stopOpacity={0.35} />
							<stop offset="100%" stopColor={c2} stopOpacity={0.2} />
						</linearGradient>
					</defs>
					<rect width="240" height="48" fill={`url(#node-grad-${agentId})`} />
				</svg>
			</div>

			{/* Avatar overlapping banner */}
			<div className="relative -mt-6 px-4">
				<div className="relative inline-block">
					<svg
						width={48}
						height={48}
						viewBox="0 0 64 64"
						className="rounded-full border-[3px] border-app-darkBox"
					>
						<defs>
							<linearGradient
								id={`av-${agentId}`}
								x1="0%"
								y1="0%"
								x2="100%"
								y2="100%"
							>
								<stop offset="0%" stopColor={c1} />
								<stop offset="100%" stopColor={c2} />
							</linearGradient>
						</defs>
						<circle cx="32" cy="32" r="32" fill={`url(#av-${agentId})`} />
					</svg>
					<div
						className={`absolute bottom-0 right-0 h-3.5 w-3.5 rounded-full border-[2px] border-app-darkBox ${
							isOnline ? "bg-green-500" : "bg-gray-500"
						}`}
					/>
				</div>
			</div>

			{/* Profile content */}
			<div className="px-4 pt-1.5 pb-3">
				{/* Name + link to agent */}
				<Link
					to="/agents/$agentId"
					params={{ agentId }}
					className="font-plex text-sm font-semibold text-ink hover:text-accent transition-colors truncate block"
					onClick={(e) => e.stopPropagation()}
				>
					{displayName}
				</Link>

				{/* Status line */}
				{status && (
					<p className="mt-0.5 text-[11px] text-ink-dull italic truncate">
						{status}
					</p>
				)}

				{/* Bio */}
				{bio && (
					<p className="mt-2 text-[11px] leading-relaxed text-ink-faint line-clamp-3">
						{bio}
					</p>
				)}

				{/* Stats */}
				<div className="mt-2 flex items-center gap-3 border-t border-app-line/40 pt-2 text-[10px] text-ink-faint">
					<span>
						<span className="font-medium text-ink-dull">{channelCount}</span> channels
					</span>
					<span>
						<span className="font-medium text-ink-dull">
							{memoryCount >= 1000 ? `${(memoryCount / 1000).toFixed(1)}k` : memoryCount}
						</span> memories
					</span>
				</div>
			</div>

			{/* Handles */}
			<Handle
				type="source"
				position={Position.Right}
				className="!h-3 !w-3 !border-2 !border-app-darkBox !bg-accent"
			/>
			<Handle
				type="target"
				position={Position.Left}
				className="!h-3 !w-3 !border-2 !border-app-darkBox !bg-accent"
			/>
		</div>
	);
}

// -- Custom Edge --

function LinkEdge({
	id,
	sourceX,
	sourceY,
	targetX,
	targetY,
	sourcePosition,
	targetPosition,
	data,
	selected,
}: EdgeProps) {
	const [edgePath, labelX, labelY] = getSmoothStepPath({
		sourceX,
		sourceY,
		sourcePosition,
		targetX,
		targetY,
		targetPosition,
		borderRadius: 16,
	});

	const relationship = (data?.relationship as string) ?? "peer";
	const color = RELATIONSHIP_COLORS[relationship] ?? "#6366f1";
	const isActive = data?.active as boolean;

	return (
		<>
			<BaseEdge
				id={id}
				path={edgePath}
				style={{
					stroke: selected ? "#fff" : color,
					strokeWidth: selected ? 2.5 : isActive ? 2.5 : 1.5,
					opacity: selected ? 1 : isActive ? 1 : 0.6,
					filter: isActive ? `drop-shadow(0 0 4px ${color})` : undefined,
				}}
			/>
			{/* Edge label */}
			<foreignObject
				x={labelX - 40}
				y={labelY - 10}
				width={80}
				height={20}
				className="pointer-events-none overflow-visible"
			>
				<div className="flex items-center justify-center">
					<span
						className="rounded-full px-2 py-0.5 text-[10px] font-medium backdrop-blur-sm"
						style={{
							backgroundColor: `${color}22`,
							color,
						}}
					>
						{RELATIONSHIP_LABELS[relationship] ?? relationship}
					</span>
				</div>
			</foreignObject>
			{/* Activity pulse */}
			{isActive && (
				<circle r="4" fill={color} className="animate-pulse">
					<animateMotion dur="1.5s" repeatCount="indefinite" path={edgePath} />
				</circle>
			)}
		</>
	);
}

const nodeTypes: NodeTypes = {
	agent: AgentNode,
};

const edgeTypes: EdgeTypes = {
	link: LinkEdge,
};

// -- Edge Config Panel --

interface EdgeConfigPanelProps {
	edge: Edge;
	onUpdate: (direction: LinkDirection, relationship: LinkRelationship) => void;
	onDelete: () => void;
	onClose: () => void;
}

function EdgeConfigPanel({
	edge,
	onUpdate,
	onDelete,
	onClose,
}: EdgeConfigPanelProps) {
	const [direction, setDirection] = useState<LinkDirection>(
		(edge.data?.direction as LinkDirection) ?? "two_way",
	);
	const [relationship, setRelationship] = useState<LinkRelationship>(
		(edge.data?.relationship as LinkRelationship) ?? "peer",
	);

	return (
		<motion.div
			initial={{ opacity: 0, y: 8 }}
			animate={{ opacity: 1, y: 0 }}
			exit={{ opacity: 0, y: 8 }}
			transition={{ duration: 0.15 }}
			className="absolute right-4 top-4 z-20 w-64 rounded-lg border border-app-line/50 bg-app-darkBox/95 p-4 shadow-xl backdrop-blur-sm"
		>
			<div className="mb-3 flex items-center justify-between">
				<span className="text-sm font-medium text-ink">Link Settings</span>
				<button
					onClick={onClose}
					className="text-ink-faint hover:text-ink transition-colors text-sm"
				>
					Close
				</button>
			</div>

			<div className="mb-2 text-tiny text-ink-faint">
				{edge.source} → {edge.target}
			</div>

			{/* Direction */}
			<div className="mb-3">
				<label className="mb-1 block text-tiny font-medium text-ink-dull">
					Direction
				</label>
				<div className="flex gap-1.5">
					{(["one_way", "two_way"] as const).map((d) => (
						<button
							key={d}
							onClick={() => setDirection(d)}
							className={`flex-1 rounded px-2 py-1.5 text-tiny transition-colors ${
								direction === d
									? "bg-accent/20 text-accent"
									: "bg-app-box text-ink-faint hover:text-ink-dull"
							}`}
						>
							{d === "one_way" ? "One Way" : "Two Way"}
						</button>
					))}
				</div>
			</div>

			{/* Relationship */}
			<div className="mb-4">
				<label className="mb-1 block text-tiny font-medium text-ink-dull">
					Relationship
				</label>
				<div className="flex gap-1.5">
					{(["peer", "superior", "subordinate"] as const).map((r) => (
						<button
							key={r}
							onClick={() => setRelationship(r)}
							className={`flex-1 rounded px-2 py-1.5 text-tiny transition-colors ${
								relationship === r
									? "bg-accent/20 text-accent"
									: "bg-app-box text-ink-faint hover:text-ink-dull"
							}`}
						>
							{r.charAt(0).toUpperCase() + r.slice(1)}
						</button>
					))}
				</div>
			</div>

			<div className="flex gap-2">
				<Button
					onClick={() => onUpdate(direction, relationship)}
					size="sm"
					className="flex-1 bg-accent/15 text-tiny text-accent hover:bg-accent/25"
				>
					Save
				</Button>
				<Button
					onClick={onDelete}
					size="sm"
					variant="destructive"
					className="text-tiny"
				>
					Delete
				</Button>
			</div>
		</motion.div>
	);
}

// -- Main Component (inner, needs ReactFlowProvider) --

interface TopologyGraphInnerProps {
	activeEdges: Set<string>;
	agents: AgentSummary[];
}

function TopologyGraphInner({ activeEdges, agents }: TopologyGraphInnerProps) {
	const queryClient = useQueryClient();
	const [selectedEdge, setSelectedEdge] = useState<Edge | null>(null);

	const { data, isLoading, error } = useQuery({
		queryKey: ["topology"],
		queryFn: api.topology,
		refetchInterval: 10_000,
	});

	// Build agent profile lookup
	const agentProfiles = useMemo(() => {
		const map = new Map<string, AgentSummary>();
		for (const agent of agents) {
			map.set(agent.id, agent);
		}
		return map;
	}, [agents]);

	// Build nodes and edges from topology data
	const { initialNodes, initialEdges } = useMemo(() => {
		if (!data) return { initialNodes: [], initialEdges: [] };
		return buildGraph(data, activeEdges, agentProfiles);
	}, [data, activeEdges, agentProfiles]);

	const [nodes, setNodes, onNodesChange] = useNodesState(initialNodes);
	const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);

	// Sync when topology data or agent profiles change
	const prevDataRef = useRef(data);
	const prevProfilesRef = useRef(agentProfiles);
	if (data !== prevDataRef.current || agentProfiles !== prevProfilesRef.current) {
		prevDataRef.current = data;
		prevProfilesRef.current = agentProfiles;
		if (data) {
			const { initialNodes: newNodes, initialEdges: newEdges } = buildGraph(
				data,
				activeEdges,
				agentProfiles,
			);

			// Preserve existing node positions
			const positionMap = new Map(
				nodes.map((n) => [n.id, n.position]),
			);
			const mergedNodes = newNodes.map((n) => ({
				...n,
				position: positionMap.get(n.id) ?? n.position,
			}));

			setNodes(mergedNodes);
			setEdges(newEdges);
		}
	}

	// Update edge activity state when activeEdges changes
	const prevActiveRef = useRef(activeEdges);
	if (activeEdges !== prevActiveRef.current) {
		prevActiveRef.current = activeEdges;
		setEdges((eds) =>
			eds.map((e) => ({
				...e,
				data: { ...e.data, active: activeEdges.has(e.id) },
			})),
		);
	}

	// Mutations
	const createLink = useMutation({
		mutationFn: (params: {
			from: string;
			to: string;
			direction: LinkDirection;
			relationship: LinkRelationship;
		}) =>
			api.createLink({
				from: params.from,
				to: params.to,
				direction: params.direction,
				relationship: params.relationship,
			}),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["topology"] });
		},
	});

	const updateLink = useMutation({
		mutationFn: (params: {
			from: string;
			to: string;
			direction: LinkDirection;
			relationship: LinkRelationship;
		}) =>
			api.updateLink(params.from, params.to, {
				direction: params.direction,
				relationship: params.relationship,
			}),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["topology"] });
			setSelectedEdge(null);
		},
	});

	const deleteLink = useMutation({
		mutationFn: (params: { from: string; to: string }) =>
			api.deleteLink(params.from, params.to),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["topology"] });
			setSelectedEdge(null);
		},
	});

	// Handle new connection (drag from handle to handle)
	const onConnect = useCallback(
		(connection: Connection) => {
			if (!connection.source || !connection.target) return;
			if (connection.source === connection.target) return;

			// Check if link already exists
			const exists = edges.some(
				(e) =>
					e.source === connection.source && e.target === connection.target,
			);
			if (exists) return;

			createLink.mutate({
				from: connection.source,
				to: connection.target,
				direction: "two_way",
				relationship: "peer",
			});
		},
		[edges, createLink],
	);

	const onEdgeClick = useCallback(
		(_: React.MouseEvent, edge: Edge) => {
			setSelectedEdge(edge);
		},
		[],
	);

	const onPaneClick = useCallback(() => {
		setSelectedEdge(null);
	}, []);

	if (isLoading) {
		return (
			<div className="flex h-full items-center justify-center">
				<div className="flex items-center gap-2 text-ink-dull">
					<div className="h-2 w-2 animate-pulse rounded-full bg-accent" />
					Loading topology...
				</div>
			</div>
		);
	}

	if (error) {
		return (
			<div className="flex h-full items-center justify-center">
				<p className="text-sm text-red-400">
					Failed to load topology
				</p>
			</div>
		);
	}

	if (!data || data.agents.length === 0) {
		return (
			<div className="flex h-full items-center justify-center">
				<p className="text-sm text-ink-faint">
					No agents configured
				</p>
			</div>
		);
	}

	return (
		<div className="relative h-full w-full">
			<ReactFlow
				nodes={nodes}
				edges={edges}
				onNodesChange={onNodesChange}
				onEdgesChange={onEdgesChange}
				onConnect={onConnect}
				onEdgeClick={onEdgeClick}
				onPaneClick={onPaneClick}
				nodeTypes={nodeTypes}
				edgeTypes={edgeTypes}
				defaultEdgeOptions={{
					type: "link",
					markerEnd: {
						type: MarkerType.ArrowClosed,
						width: 16,
						height: 16,
					},
				}}
				fitView
				fitViewOptions={{ padding: 0.3 }}
				proOptions={{ hideAttribution: true }}
				className="topology-graph"
			>
				<Background
					color="hsla(230, 8%, 14%, 0.5)"
					gap={20}
					size={1}
				/>
				<Controls
					showInteractive={false}
					className="!bg-app-darkBox/80 !border-app-line !backdrop-blur-sm [&>button]:!bg-transparent [&>button]:!border-app-line [&>button]:!text-ink-dull [&>button:hover]:!bg-app-hover"
				/>
			</ReactFlow>

			{/* Legend */}
			<div className="absolute bottom-4 left-4 z-10 rounded-md bg-app-darkBox/80 p-3 backdrop-blur-sm">
				<div className="mb-2 text-tiny font-medium text-ink-faint">
					Relationships
				</div>
				<div className="flex flex-col gap-1">
					{Object.entries(RELATIONSHIP_COLORS).map(
						([type, color]) => (
							<div key={type} className="flex items-center gap-1.5">
								<span
									className="inline-block h-0.5 w-4 rounded"
									style={{ backgroundColor: color }}
								/>
								<span className="text-tiny text-ink-faint capitalize">
									{type}
								</span>
							</div>
						),
					)}
				</div>
				<div className="mt-2 border-t border-app-line/30 pt-2 text-tiny text-ink-faint">
					Drag between nodes to create links
				</div>
			</div>

			{/* Edge config panel */}
			<AnimatePresence>
				{selectedEdge && (
					<EdgeConfigPanel
						key={selectedEdge.id}
						edge={selectedEdge}
						onUpdate={(direction, relationship) =>
							updateLink.mutate({
								from: selectedEdge.source,
								to: selectedEdge.target,
								direction,
								relationship,
							})
						}
						onDelete={() =>
							deleteLink.mutate({
								from: selectedEdge.source,
								to: selectedEdge.target,
							})
						}
						onClose={() => setSelectedEdge(null)}
					/>
				)}
			</AnimatePresence>
		</div>
	);
}

// -- Graph Builder --

function buildGraph(
	data: TopologyResponse,
	activeEdges: Set<string>,
	agentProfiles: Map<string, AgentSummary>,
): { initialNodes: Node[]; initialEdges: Edge[] } {
	const agentCount = data.agents.length;

	// Arrange agents in a circle for initial layout
	const radius = Math.max(200, agentCount * 80);
	const centerX = 400;
	const centerY = 300;

	const initialNodes: Node[] = data.agents.map((agent, index) => {
		const angle = (2 * Math.PI * index) / agentCount - Math.PI / 2;
		const summary = agentProfiles.get(agent.id);
		const profile = summary?.profile;
		const isOnline =
			summary?.last_activity_at != null &&
			new Date(summary.last_activity_at).getTime() > Date.now() - 5 * 60 * 1000;

		return {
			id: agent.id,
			type: "agent",
			position: {
				x: centerX + radius * Math.cos(angle),
				y: centerY + radius * Math.sin(angle),
			},
			data: {
				agentId: agent.id,
				displayName: profile?.display_name ?? agent.name,
				avatarSeed: profile?.avatar_seed ?? agent.id,
				status: profile?.status ?? null,
				bio: profile?.bio ?? null,
				isOnline,
				channelCount: summary?.channel_count ?? 0,
				memoryCount: summary?.memory_total ?? 0,
			},
		};
	});

	const initialEdges: Edge[] = data.links.map((link) => {
		const edgeId = `${link.from}->${link.to}`;
		return {
			id: edgeId,
			source: link.from,
			target: link.to,
			type: "link",
			data: {
				direction: link.direction,
				relationship: link.relationship,
				active: activeEdges.has(edgeId),
			},
			markerEnd:
				link.direction === "one_way"
					? {
							type: MarkerType.ArrowClosed,
							width: 16,
							height: 16,
							color:
								RELATIONSHIP_COLORS[link.relationship] ??
								"#6366f1",
						}
					: undefined,
			style: {
				stroke:
					RELATIONSHIP_COLORS[link.relationship] ?? "#6366f1",
			},
		};
	});

	return { initialNodes, initialEdges };
}

// -- Exported component with provider wrapper --

export interface TopologyGraphProps {
	activeEdges?: Set<string>;
	agents?: AgentSummary[];
}

export function TopologyGraph({ activeEdges, agents }: TopologyGraphProps) {
	const edges = activeEdges ?? new Set<string>();
	const agentList = agents ?? [];
	return (
		<ReactFlowProvider>
			<TopologyGraphInner activeEdges={edges} agents={agentList} />
		</ReactFlowProvider>
	);
}
