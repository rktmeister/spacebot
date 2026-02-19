import { WebChatPanel } from "@/components/WebChatPanel";

export function AgentChat({ agentId }: { agentId: string }) {
	return <WebChatPanel agentId={agentId} />;
}
