import { useEffect, useRef, useState } from "react";
import { useWebChat, type ToolActivity } from "@/hooks/useWebChat";
import { Markdown } from "@/components/Markdown";
import { Button } from "@/ui";

interface WebChatPanelProps {
	agentId: string;
}

function ToolActivityIndicator({ activity }: { activity: ToolActivity[] }) {
	if (activity.length === 0) return null;

	return (
		<div className="flex flex-col gap-1 px-3 py-2">
			{activity.map((tool, index) => (
				<div
					key={`${tool.tool}-${index}`}
					className="flex items-center gap-2 rounded bg-app-darkBox/40 px-2 py-1"
				>
					{tool.status === "running" ? (
						<span className="h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />
					) : (
						<span className="h-1.5 w-1.5 rounded-full bg-green-400" />
					)}
					<span className="font-mono text-tiny text-ink-faint">{tool.tool}</span>
				</div>
			))}
		</div>
	);
}

export function WebChatPanel({ agentId }: WebChatPanelProps) {
	const { messages, isStreaming, error, toolActivity, sendMessage } = useWebChat(agentId);
	const [input, setInput] = useState("");
	const messagesEndRef = useRef<HTMLDivElement>(null);
	const inputRef = useRef<HTMLInputElement>(null);

	useEffect(() => {
		messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
	}, [messages.length, isStreaming, toolActivity.length]);

	useEffect(() => {
		inputRef.current?.focus();
	}, []);

	const handleSubmit = (event: React.FormEvent) => {
		event.preventDefault();
		const trimmed = input.trim();
		if (!trimmed || isStreaming) return;
		setInput("");
		sendMessage(trimmed);
	};

	return (
		<div className="flex h-full w-full flex-col">
			{/* Header */}
			<div className="flex h-12 items-center border-b border-app-line/50 px-4">
				<span className="text-sm font-medium text-ink">Chat</span>
			</div>

			{/* Messages */}
			<div className="flex-1 overflow-y-auto">
				<div className="flex flex-col gap-3 p-4">
					{messages.length === 0 && !isStreaming && (
						<p className="py-8 text-center text-sm text-ink-faint">
							Chat with {agentId}
						</p>
					)}
					{messages.map((message) => (
						<div
							key={message.id}
							className={`rounded-md px-3 py-2 ${
								message.role === "user"
									? "ml-8 bg-accent/10"
									: "mr-2 bg-app-darkBox/50"
							}`}
						>
							<span className={`text-tiny font-medium ${
								message.role === "user" ? "text-accent-faint" : "text-emerald-400"
							}`}>
								{message.role === "user" ? "you" : agentId}
							</span>
							<div className="mt-0.5 text-sm text-ink-dull">
								{message.role === "assistant" ? (
									<Markdown>{message.content}</Markdown>
								) : (
									<p>{message.content}</p>
								)}
							</div>
						</div>
					))}
					{isStreaming && messages[messages.length - 1]?.role !== "assistant" && (
						<div className="mr-2 rounded-md bg-app-darkBox/50 px-3 py-2">
							<span className="text-tiny font-medium text-emerald-400">{agentId}</span>
							<ToolActivityIndicator activity={toolActivity} />
							{toolActivity.length === 0 && (
								<div className="mt-1 flex items-center gap-1">
									<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400" />
									<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400 [animation-delay:0.2s]" />
									<span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-emerald-400 [animation-delay:0.4s]" />
									<span className="ml-1 text-tiny text-ink-faint">thinking...</span>
								</div>
							)}
						</div>
					)}
					{error && (
						<div className="rounded-md border border-red-500/20 bg-red-500/10 px-3 py-2 text-sm text-red-400">
							{error}
						</div>
					)}
					<div ref={messagesEndRef} />
				</div>
			</div>

			{/* Input */}
			<form onSubmit={handleSubmit} className="border-t border-app-line/50 p-3">
				<div className="flex gap-2">
					<input
						ref={inputRef}
						type="text"
						value={input}
						onChange={(event) => setInput(event.target.value)}
						placeholder={isStreaming ? "Waiting for response..." : `Message ${agentId}...`}
						disabled={isStreaming}
						className="flex-1 rounded-md border border-app-line bg-app-darkBox px-3 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:border-emerald-500/50 focus:outline-none disabled:opacity-50"
					/>
					<Button
						type="submit"
						disabled={isStreaming || !input.trim()}
						size="sm"
						className="bg-emerald-500/20 text-emerald-400 hover:bg-emerald-500/30"
					>
						Send
					</Button>
				</div>
			</form>
		</div>
	);
}
