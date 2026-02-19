export function formatUptime(seconds: number): string {
	const hours = Math.floor(seconds / 3600);
	const minutes = Math.floor((seconds % 3600) / 60);
	const secs = seconds % 60;
	if (hours > 0) return `${hours}h ${minutes}m`;
	if (minutes > 0) return `${minutes}m ${secs}s`;
	return `${secs}s`;
}

export function formatTimeAgo(dateStr: string): string {
	const seconds = Math.floor((Date.now() - new Date(dateStr).getTime()) / 1000);
	if (seconds < 60) return "just now";
	if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
	if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
	return `${Math.floor(seconds / 86400)}d ago`;
}

export function formatTimestamp(ts: number): string {
	return new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function formatDuration(seconds: number): string {
	if (seconds < 60) return `${seconds}s`;
	return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

export function platformIcon(platform: string): string {
	switch (platform) {
		case "discord": return "Discord";
		case "slack": return "Slack";
		case "telegram": return "Telegram";
		case "twitch": return "Twitch";
		case "webhook": return "Webhook";
		case "cron": return "Cron";
		default: return platform;
	}
}

export function platformColor(platform: string): string {
	switch (platform) {
		case "discord": return "bg-indigo-500/20 text-indigo-400";
		case "slack": return "bg-green-500/20 text-green-400";
		case "telegram": return "bg-blue-500/20 text-blue-400";
		case "twitch": return "bg-purple-500/20 text-purple-400";
		case "cron": return "bg-amber-500/20 text-amber-400";
		default: return "bg-gray-500/20 text-gray-400";
	}
}
