import Anthropic from "@lobehub/icons/es/Anthropic";
import OpenAI from "@lobehub/icons/es/OpenAI";
import OpenRouter from "@lobehub/icons/es/OpenRouter";
import Groq from "@lobehub/icons/es/Groq";
import Mistral from "@lobehub/icons/es/Mistral";
import DeepSeek from "@lobehub/icons/es/DeepSeek";
import Fireworks from "@lobehub/icons/es/Fireworks";
import Together from "@lobehub/icons/es/Together";
import XAI from "@lobehub/icons/es/XAI";
import Zhipu from "@lobehub/icons/es/Zhipu";

interface IconProps {
	size?: number;
	className?: string;
}

interface ProviderIconProps {
	provider: string;
	className?: string;
	size?: number;
}

export function ProviderIcon({ provider, className = "text-ink-faint", size = 24 }: ProviderIconProps) {
	const iconProps: Partial<IconProps> = {
		size,
		className,
	};

	const iconMap: Record<string, React.ComponentType<IconProps>> = {
		anthropic: Anthropic,
		openai: OpenAI,
		openrouter: OpenRouter,
		groq: Groq,
		mistral: Mistral,
		deepseek: DeepSeek,
		fireworks: Fireworks,
		together: Together,
		xai: XAI,
		zhipu: Zhipu,
		"opencode-zen": OpenAI,
	};

	const IconComponent = iconMap[provider.toLowerCase()];

	if (!IconComponent) {
		return <OpenAI {...iconProps} />;
	}

	return <IconComponent {...iconProps} />;
}
