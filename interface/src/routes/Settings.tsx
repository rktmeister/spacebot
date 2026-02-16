import {useState, useEffect} from "react";
import {useQuery, useMutation, useQueryClient} from "@tanstack/react-query";
import {api, type PlatformStatus} from "@/api/client";
import {Button, Input, SettingSidebarButton, Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter, Select, SelectTrigger, SelectValue, SelectContent, SelectItem} from "@/ui";
import {useSearch, useNavigate} from "@tanstack/react-router";

type SectionId = "providers" | "channels";

const SECTIONS = [
	{
		id: "providers" as const,
		label: "Providers",
		group: "general" as const,
		description: "LLM provider API keys",
	},
	{
		id: "channels" as const,
		label: "Channels",
		group: "messaging" as const,
		description: "Messaging platforms and bindings",
	},
] satisfies {
	id: SectionId;
	label: string;
	group: string;
	description: string;
}[];

const PROVIDERS = [
	{
		id: "anthropic",
		name: "Anthropic",
		description: "Claude models (Sonnet, Opus, Haiku)",
		placeholder: "sk-ant-...",
		envVar: "ANTHROPIC_API_KEY",
	},
	{
		id: "openrouter",
		name: "OpenRouter",
		description: "Multi-provider gateway with unified API",
		placeholder: "sk-or-...",
		envVar: "OPENROUTER_API_KEY",
	},
	{
		id: "openai",
		name: "OpenAI",
		description: "GPT models",
		placeholder: "sk-...",
		envVar: "OPENAI_API_KEY",
	},
	{
		id: "zhipu",
		name: "Z.ai (GLM)",
		description: "GLM models (GLM-4, GLM-4-Flash)",
		placeholder: "...",
		envVar: "ZHIPU_API_KEY",
	},
] as const;

export function Settings() {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const search = useSearch({from: "/settings"}) as {tab?: string};
	const [activeSection, setActiveSection] = useState<SectionId>("providers");

	// Sync activeSection with URL search param
	useEffect(() => {
		if (search.tab && (search.tab === "providers" || search.tab === "channels")) {
			setActiveSection(search.tab as SectionId);
		}
	}, [search.tab]);

	const handleSectionChange = (section: SectionId) => {
		setActiveSection(section);
		navigate({to: "/settings", search: {tab: section}});
	};
	const [editingProvider, setEditingProvider] = useState<string | null>(null);
	const [keyInput, setKeyInput] = useState("");
	const [message, setMessage] = useState<{
		text: string;
		type: "success" | "error";
	} | null>(null);

	const {data, isLoading} = useQuery({
		queryKey: ["providers"],
		queryFn: api.providers,
		staleTime: 5_000,
	});

	const updateMutation = useMutation({
		mutationFn: ({provider, apiKey}: {provider: string; apiKey: string}) =>
			api.updateProvider(provider, apiKey),
		onSuccess: (result) => {
			if (result.success) {
				setEditingProvider(null);
				setKeyInput("");
				setMessage({text: result.message, type: "success"});
				queryClient.invalidateQueries({queryKey: ["providers"]});
				// Agents will auto-start on the backend, refetch agent list after a short delay
				setTimeout(() => {
					queryClient.invalidateQueries({queryKey: ["agents"]});
					queryClient.invalidateQueries({queryKey: ["overview"]});
				}, 3000);
			} else {
				setMessage({text: result.message, type: "error"});
			}
		},
		onError: (error) => {
			setMessage({text: `Failed: ${error.message}`, type: "error"});
		},
	});

	const removeMutation = useMutation({
		mutationFn: (provider: string) => api.removeProvider(provider),
		onSuccess: (result) => {
			if (result.success) {
				setMessage({text: result.message, type: "success"});
				queryClient.invalidateQueries({queryKey: ["providers"]});
			} else {
				setMessage({text: result.message, type: "error"});
			}
		},
		onError: (error) => {
			setMessage({text: `Failed: ${error.message}`, type: "error"});
		},
	});

	const editingProviderData = PROVIDERS.find((p) => p.id === editingProvider);

	const handleSave = () => {
		if (!keyInput.trim() || !editingProvider) return;
		updateMutation.mutate({provider: editingProvider, apiKey: keyInput.trim()});
	};

	const handleClose = () => {
		setEditingProvider(null);
		setKeyInput("");
	};

	const isConfigured = (providerId: string): boolean => {
		if (!data) return false;
		return data.providers[providerId as keyof typeof data.providers] ?? false;
	};

	return (
		<div className="flex h-full">
			{/* Sidebar */}
			<div className="flex w-52 flex-shrink-0 flex-col border-r border-app-line/50 bg-app-darkBox/20 overflow-y-auto">
				<div className="px-3 pb-1 pt-4">
					<span className="text-tiny font-medium uppercase tracking-wider text-ink-faint">
						Settings
					</span>
				</div>
				<div className="flex flex-col gap-0.5 px-2">
					{SECTIONS.map((section) => (
						<SettingSidebarButton
							key={section.id}
							onClick={() => handleSectionChange(section.id)}
							active={activeSection === section.id}
						>
							<span className="flex-1">{section.label}</span>
						</SettingSidebarButton>
					))}
				</div>
			</div>

			{/* Content */}
			<div className="flex flex-1 flex-col overflow-hidden">
				<header className="flex h-12 items-center border-b border-app-line bg-app-darkBox/50 px-6">
					<h1 className="font-plex text-sm font-medium text-ink">
						{SECTIONS.find((s) => s.id === activeSection)?.label}
					</h1>
				</header>
				<div className="flex-1 overflow-y-auto">
					{activeSection === "providers" ? (
					<div className="mx-auto max-w-2xl px-6 py-6">
						{/* Section header */}
						<div className="mb-6">
							<h2 className="font-plex text-sm font-semibold text-ink">
								LLM Providers
							</h2>
							<p className="mt-1 text-sm text-ink-dull">
								Configure API keys for LLM providers. At least one provider is
								required for agents to function.
							</p>
						</div>

						{isLoading ? (
							<div className="flex items-center gap-2 text-ink-dull">
								<div className="h-2 w-2 animate-pulse rounded-full bg-accent" />
								Loading providers...
							</div>
						) : (
							<div className="flex flex-col gap-3">
								{PROVIDERS.map((provider) => {
									const configured = isConfigured(provider.id);

									return (
										<div
											key={provider.id}
											className="rounded-lg border border-app-line bg-app-box p-4"
										>
											<div className="flex items-center justify-between">
												<div className="flex-1">
													<span className="text-sm font-medium text-ink">
														{provider.name}
													</span>
													<p className="mt-0.5 text-sm text-ink-dull">
														{provider.description}
													</p>
												</div>
												<div className="flex gap-2">
													<Button
														onClick={() => {
															setEditingProvider(provider.id);
															setKeyInput("");
															setMessage(null);
														}}
														variant="outline"
														size="sm"
													>
														{configured ? "Update" : "Add key"}
													</Button>
													{configured && (
														<Button
															onClick={() =>
																removeMutation.mutate(provider.id)
															}
															variant="outline"
															size="sm"
															loading={removeMutation.isPending}
														>
															Remove
														</Button>
													)}
												</div>
											</div>
										</div>
									);
								})}
							</div>
						)}

						{/* Info note */}
						<div className="mt-6 rounded-md border border-app-line bg-app-darkBox/20 px-4 py-3">
							<p className="text-sm text-ink-faint">
								Keys are written to{" "}
								<code className="rounded bg-app-box px-1 py-0.5 text-tiny text-ink-dull">
									config.toml
								</code>{" "}
								in your instance directory. You can also set them via
								environment variables (
								<code className="rounded bg-app-box px-1 py-0.5 text-tiny text-ink-dull">
									ANTHROPIC_API_KEY
								</code>
								, etc.).
							</p>
						</div>
					</div>
					) : activeSection === "channels" ? (
						<ChannelsSection />
					) : null}
				</div>
			</div>

			<Dialog open={!!editingProvider} onOpenChange={(open) => { if (!open) handleClose(); }}>
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>{isConfigured(editingProvider ?? "") ? "Update" : "Add"} API Key</DialogTitle>
						<DialogDescription>
							Enter your {editingProviderData?.name} API key. It will be saved to your instance config.
						</DialogDescription>
					</DialogHeader>
					<Input
						type="password"
						value={keyInput}
						onChange={(e) => setKeyInput(e.target.value)}
						placeholder={editingProviderData?.placeholder}
						autoFocus
						onKeyDown={(e) => {
							if (e.key === "Enter") handleSave();
						}}
					/>
					{message && (
						<div
							className={`rounded-md border px-3 py-2 text-sm ${
								message.type === "success"
									? "border-green-500/20 bg-green-500/10 text-green-400"
									: "border-red-500/20 bg-red-500/10 text-red-400"
							}`}
						>
							{message.text}
						</div>
					)}
					<DialogFooter>
						<Button onClick={handleClose} variant="ghost" size="sm">
							Cancel
						</Button>
						<Button
							onClick={handleSave}
							disabled={!keyInput.trim()}
							loading={updateMutation.isPending}
							size="sm"
						>
							Save
						</Button>
					</DialogFooter>
				</DialogContent>
			</Dialog>
		</div>
	);
}

function ChannelsSection() {
	const queryClient = useQueryClient();
	const [editingPlatform, setEditingPlatform] = useState<"discord" | "slack" | "webhook" | null>(null);
	const [platformInputs, setPlatformInputs] = useState<Record<string, string>>({});
	const [addingBinding, setAddingBinding] = useState(false);
	const [bindingForm, setBindingForm] = useState({
		agent_id: "main",
		channel: "discord" as "discord" | "slack" | "telegram" | "webhook",
		guild_id: "",
		workspace_id: "",
		chat_id: "",
		channel_ids: "",
		dm_allowed_users: "",
	});
	const [message, setMessage] = useState<{
		text: string;
		type: "success" | "error";
	} | null>(null);

	const {data: messagingStatus, isLoading: statusLoading} = useQuery({
		queryKey: ["messaging-status"],
		queryFn: api.messagingStatus,
		staleTime: 5_000,
	});

	const {data: bindingsData, isLoading: bindingsLoading} = useQuery({
		queryKey: ["bindings"],
		queryFn: () => api.bindings(),
		staleTime: 5_000,
	});

	const {data: agentsData} = useQuery({
		queryKey: ["agents"],
		queryFn: api.agents,
		staleTime: 10_000,
	});

	const createPlatformMutation = useMutation({
		mutationFn: api.createBinding,
		onSuccess: (result) => {
			if (result.success) {
				setEditingPlatform(null);
				setPlatformInputs({});
				setMessage({text: result.message, type: "success"});
				queryClient.invalidateQueries({queryKey: ["messaging-status"]});
				queryClient.invalidateQueries({queryKey: ["bindings"]});
			} else {
				setMessage({text: result.message, type: "error"});
			}
		},
		onError: (error) => {
			setMessage({text: `Failed: ${error.message}`, type: "error"});
		},
	});

	const addBindingMutation = useMutation({
		mutationFn: api.createBinding,
		onSuccess: (result) => {
			if (result.success) {
				setAddingBinding(false);
				setBindingForm({
					agent_id: "main",
					channel: "discord",
					guild_id: "",
					workspace_id: "",
					chat_id: "",
					channel_ids: "",
					dm_allowed_users: "",
				});
				setMessage({text: result.message, type: "success"});
				queryClient.invalidateQueries({queryKey: ["bindings"]});
			} else {
				setMessage({text: result.message, type: "error"});
			}
		},
		onError: (error) => {
			setMessage({text: `Failed: ${error.message}`, type: "error"});
		},
	});

	const deleteBindingMutation = useMutation({
		mutationFn: api.deleteBinding,
		onSuccess: (result) => {
			if (result.success) {
				setMessage({text: result.message, type: "success"});
				queryClient.invalidateQueries({queryKey: ["bindings"]});
			} else {
				setMessage({text: result.message, type: "error"});
			}
		},
		onError: (error) => {
			setMessage({text: `Failed: ${error.message}`, type: "error"});
		},
	});

	const isLoading = statusLoading || bindingsLoading;

	const handleClose = () => {
		setEditingPlatform(null);
		setPlatformInputs({});
		setMessage(null);
	};

	const handleSavePlatform = () => {
		if (!editingPlatform) return;

		const request: any = {
			agent_id: "main",
			channel: editingPlatform,
		};

		if (editingPlatform === "discord") {
			if (!platformInputs.discord_token?.trim()) return;
			request.platform_credentials = {
				discord_token: platformInputs.discord_token.trim(),
			};
		} else if (editingPlatform === "slack") {
			if (!platformInputs.slack_bot_token?.trim() || !platformInputs.slack_app_token?.trim()) return;
			request.platform_credentials = {
				slack_bot_token: platformInputs.slack_bot_token.trim(),
				slack_app_token: platformInputs.slack_app_token.trim(),
			};
		}

		createPlatformMutation.mutate(request);
	};

	const handleAddBinding = () => {
		const request: any = {
			agent_id: bindingForm.agent_id,
			channel: bindingForm.channel,
		};

		// Add platform-specific filters
		if (bindingForm.channel === "discord" && bindingForm.guild_id.trim()) {
			request.guild_id = bindingForm.guild_id.trim();
		}
		if (bindingForm.channel === "slack" && bindingForm.workspace_id.trim()) {
			request.workspace_id = bindingForm.workspace_id.trim();
		}
		if (bindingForm.channel === "telegram" && bindingForm.chat_id.trim()) {
			request.chat_id = bindingForm.chat_id.trim();
		}

		// Parse channel_ids (comma-separated)
		if (bindingForm.channel_ids.trim()) {
			request.channel_ids = bindingForm.channel_ids.split(",").map(id => id.trim()).filter(Boolean);
		}

		// Parse dm_allowed_users (comma-separated)
		if (bindingForm.dm_allowed_users.trim()) {
			request.dm_allowed_users = bindingForm.dm_allowed_users.split(",").map(id => id.trim()).filter(Boolean);
		}

		addBindingMutation.mutate(request);
	};

	const handleDeleteBinding = (binding: any) => {
		const request: any = {
			agent_id: binding.agent_id,
			channel: binding.channel,
		};

		if (binding.guild_id) request.guild_id = binding.guild_id;
		if (binding.workspace_id) request.workspace_id = binding.workspace_id;
		if (binding.chat_id) request.chat_id = binding.chat_id;

		deleteBindingMutation.mutate(request);
	};

	return (
		<div className="mx-auto max-w-2xl px-6 py-6">
			{/* Section header */}
			<div className="mb-6">
				<h2 className="font-plex text-sm font-semibold text-ink">
					Messaging Platforms
				</h2>
				<p className="mt-1 text-sm text-ink-dull">
					Configure messaging platform credentials and bindings. Bindings route conversations from specific servers/channels to agents.
				</p>
			</div>

			{isLoading ? (
				<div className="flex items-center gap-2 text-ink-dull">
					<div className="h-2 w-2 animate-pulse rounded-full bg-accent" />
					Loading channels...
				</div>
			) : (
				<>
					{/* Platform Status Cards */}
					<div className="mb-6 flex flex-col gap-3">
						<PlatformCard
							name="Discord"
							description="Discord bot integration"
							status={messagingStatus?.discord}
							onSetup={() => {
								setEditingPlatform("discord");
								setPlatformInputs({});
								setMessage(null);
							}}
						/>
						<PlatformCard
							name="Slack"
							description="Slack bot integration"
							status={messagingStatus?.slack}
							onSetup={() => {
								setEditingPlatform("slack");
								setPlatformInputs({});
								setMessage(null);
							}}
						/>
						<PlatformCard
							name="Webhook"
							description="HTTP webhook receiver"
							status={messagingStatus?.webhook}
							onSetup={() => {
								setEditingPlatform("webhook");
								setPlatformInputs({});
								setMessage(null);
							}}
						/>
					</div>

					{/* Bindings Table */}
					<div className="mt-8">
						<div className="mb-4 flex items-center justify-between">
							<h2 className="font-plex text-sm font-semibold text-ink">Bindings</h2>
							<Button
								size="sm"
								variant="outline"
								onClick={() => {
									setAddingBinding(true);
									setBindingForm({
										agent_id: agentsData?.agents?.[0]?.id ?? "main",
										channel: "discord",
										guild_id: "",
										workspace_id: "",
										chat_id: "",
										channel_ids: "",
										dm_allowed_users: "",
									});
									setMessage(null);
								}}
							>
								Add Binding
							</Button>
						</div>

						{bindingsData?.bindings && bindingsData.bindings.length > 0 ? (
							<div className="rounded-lg border border-app-line bg-app-box">
								{bindingsData.bindings.map((binding, idx) => (
									<div
										key={idx}
										className="flex items-center justify-between border-b border-app-line/50 px-4 py-3 last:border-b-0"
									>
										<div className="flex-1">
											<div className="flex items-center gap-2">
												<span className="text-sm font-medium text-ink">
													{binding.agent_id}
												</span>
												<span className="text-sm text-ink-faint">→</span>
												<span className="text-sm text-ink-dull">
													{binding.channel}
												</span>
											</div>
											<div className="mt-1 flex items-center gap-2 text-tiny text-ink-faint">
												{binding.guild_id && (
													<span>Guild: {binding.guild_id}</span>
												)}
												{binding.workspace_id && (
													<span>Workspace: {binding.workspace_id}</span>
												)}
												{binding.chat_id && (
													<span>Chat: {binding.chat_id}</span>
												)}
												{binding.channel_ids.length > 0 && (
													<span>Channels: {binding.channel_ids.length}</span>
												)}
												{binding.dm_allowed_users.length > 0 && (
													<span>DM Users: {binding.dm_allowed_users.length}</span>
												)}
											</div>
										</div>
										<Button 
											size="sm" 
											variant="ghost"
											onClick={() => handleDeleteBinding(binding)}
											loading={deleteBindingMutation.isPending}
										>
											Remove
										</Button>
									</div>
								))}
							</div>
						) : (
							<div className="flex flex-col items-center justify-center rounded-lg border border-dashed border-app-line/50 bg-app-darkBox/20 py-12">
								<p className="text-sm text-ink-faint">No bindings configured</p>
								<p className="mt-1 text-tiny text-ink-faint/70">
									Add a binding to route messages to an agent
								</p>
							</div>
						)}
					</div>
				</>
			)}

			{/* Info note */}
			<div className="mt-6 rounded-md border border-app-line bg-app-darkBox/20 px-4 py-3">
				<p className="text-sm text-ink-faint">
					Platform credentials are stored in{" "}
					<code className="rounded bg-app-box px-1 py-0.5 text-tiny text-ink-dull">
						config.toml
					</code>
					. Bindings route conversations from specific platforms/servers to agents. The first matching binding wins.
				</p>
			</div>

			{/* Platform Setup Modal */}
			<Dialog open={!!editingPlatform} onOpenChange={(open) => { if (!open) handleClose(); }}>
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>
							{editingPlatform === "discord" && "Configure Discord"}
							{editingPlatform === "slack" && "Configure Slack"}
							{editingPlatform === "webhook" && "Configure Webhook"}
						</DialogTitle>
						<DialogDescription>
							{editingPlatform === "discord" && "Enter your Discord bot token to enable Discord integration."}
							{editingPlatform === "slack" && "Enter your Slack bot and app tokens to enable Slack integration."}
							{editingPlatform === "webhook" && "Configure webhook receiver settings."}
						</DialogDescription>
					</DialogHeader>
					
					{editingPlatform === "discord" && (
						<div className="flex flex-col gap-3">
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Bot Token</label>
								<Input
									type="password"
									value={platformInputs.discord_token ?? ""}
									onChange={(e) => setPlatformInputs({...platformInputs, discord_token: e.target.value})}
									placeholder="MTk4NjIyNDgzNDcxOTI1MjQ4.D..."
									autoFocus
									onKeyDown={(e) => {
										if (e.key === "Enter") handleSavePlatform();
									}}
								/>
								<p className="mt-1 text-tiny text-ink-faint">
									Get this from the Discord Developer Portal
								</p>
							</div>
						</div>
					)}

					{editingPlatform === "slack" && (
						<div className="flex flex-col gap-3">
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Bot Token</label>
								<Input
									type="password"
									value={platformInputs.slack_bot_token ?? ""}
									onChange={(e) => setPlatformInputs({...platformInputs, slack_bot_token: e.target.value})}
									placeholder="xoxb-..."
									autoFocus
									onKeyDown={(e) => {
										if (e.key === "Enter" && platformInputs.slack_app_token?.trim()) handleSavePlatform();
									}}
								/>
							</div>
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">App Token</label>
								<Input
									type="password"
									value={platformInputs.slack_app_token ?? ""}
									onChange={(e) => setPlatformInputs({...platformInputs, slack_app_token: e.target.value})}
									placeholder="xapp-..."
									onKeyDown={(e) => {
										if (e.key === "Enter") handleSavePlatform();
									}}
								/>
							</div>
							<p className="text-tiny text-ink-faint">
								Get these from your Slack app settings
							</p>
						</div>
					)}

					{editingPlatform === "webhook" && (
						<div className="flex flex-col gap-3">
							<p className="text-sm text-ink-dull">
								Webhook receiver is configured in <code className="rounded bg-app-box px-1 py-0.5 text-tiny">config.toml</code>. 
								No additional setup required here.
							</p>
						</div>
					)}

					{message && (
						<div
							className={`rounded-md border px-3 py-2 text-sm ${
								message.type === "success"
									? "border-green-500/20 bg-green-500/10 text-green-400"
									: "border-red-500/20 bg-red-500/10 text-red-400"
							}`}
						>
							{message.text}
						</div>
					)}

					<DialogFooter>
						<Button onClick={handleClose} variant="ghost" size="sm">
							Cancel
						</Button>
						{editingPlatform !== "webhook" && (
							<Button
								onClick={handleSavePlatform}
								disabled={
									editingPlatform === "discord" ? !platformInputs.discord_token?.trim() :
									editingPlatform === "slack" ? (!platformInputs.slack_bot_token?.trim() || !platformInputs.slack_app_token?.trim()) :
									false
								}
								loading={createPlatformMutation.isPending}
								size="sm"
							>
								Save
							</Button>
						)}
					</DialogFooter>
				</DialogContent>
			</Dialog>

			{/* Add Binding Modal */}
			<Dialog open={addingBinding} onOpenChange={(open) => { 
				if (!open) {
					setAddingBinding(false);
					setMessage(null);
				}
			}}>
				<DialogContent className="max-w-md">
					<DialogHeader>
						<DialogTitle>Add Binding</DialogTitle>
						<DialogDescription>
							Route messages from a specific platform location to an agent.
						</DialogDescription>
					</DialogHeader>

					<div className="flex flex-col gap-4">
						{/* Agent Selection */}
						<div>
							<label className="mb-1.5 block text-sm font-medium text-ink">Agent</label>
							<Select
								value={bindingForm.agent_id}
								onValueChange={(value) => setBindingForm({...bindingForm, agent_id: value})}
							>
								<SelectTrigger>
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									{agentsData?.agents?.map((agent) => (
										<SelectItem key={agent.id} value={agent.id}>
											{agent.id}
										</SelectItem>
									)) ?? (
										<SelectItem value="main">main</SelectItem>
									)}
								</SelectContent>
							</Select>
						</div>

						{/* Platform Selection */}
						<div>
							<label className="mb-1.5 block text-sm font-medium text-ink">Platform</label>
							<Select
								value={bindingForm.channel}
								onValueChange={(value: any) => setBindingForm({...bindingForm, channel: value})}
							>
								<SelectTrigger>
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									<SelectItem value="discord">Discord</SelectItem>
									<SelectItem value="slack">Slack</SelectItem>
									<SelectItem value="telegram">Telegram</SelectItem>
									<SelectItem value="webhook">Webhook</SelectItem>
								</SelectContent>
							</Select>
						</div>

						{/* Platform-specific filters */}
						{bindingForm.channel === "discord" && (
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Guild ID</label>
								<Input
									value={bindingForm.guild_id}
									onChange={(e) => setBindingForm({...bindingForm, guild_id: e.target.value})}
									placeholder="123456789 (optional)"
								/>
								<p className="mt-1 text-tiny text-ink-faint">
									Leave empty to match any server
								</p>
							</div>
						)}

						{bindingForm.channel === "slack" && (
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Workspace ID</label>
								<Input
									value={bindingForm.workspace_id}
									onChange={(e) => setBindingForm({...bindingForm, workspace_id: e.target.value})}
									placeholder="T0123456789 (optional)"
								/>
								<p className="mt-1 text-tiny text-ink-faint">
									Leave empty to match any workspace
								</p>
							</div>
						)}

						{bindingForm.channel === "telegram" && (
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Chat ID</label>
								<Input
									value={bindingForm.chat_id}
									onChange={(e) => setBindingForm({...bindingForm, chat_id: e.target.value})}
									placeholder="-1001234567890 (optional)"
								/>
								<p className="mt-1 text-tiny text-ink-faint">
									Leave empty to match any chat
								</p>
							</div>
						)}

						{/* Channel IDs (for Discord/Slack) */}
						{(bindingForm.channel === "discord" || bindingForm.channel === "slack") && (
							<div>
								<label className="mb-1.5 block text-sm font-medium text-ink">Channel IDs</label>
								<Input
									value={bindingForm.channel_ids}
									onChange={(e) => setBindingForm({...bindingForm, channel_ids: e.target.value})}
									placeholder="123,456,789 (optional, comma-separated)"
								/>
								<p className="mt-1 text-tiny text-ink-faint">
									Leave empty to match all channels
								</p>
							</div>
						)}

						{/* DM Allowed Users */}
						<div>
							<label className="mb-1.5 block text-sm font-medium text-ink">DM Allowed Users</label>
							<Input
								value={bindingForm.dm_allowed_users}
								onChange={(e) => setBindingForm({...bindingForm, dm_allowed_users: e.target.value})}
								placeholder="user1,user2 (optional, comma-separated)"
							/>
							<p className="mt-1 text-tiny text-ink-faint">
								User IDs allowed to send DMs
							</p>
						</div>
					</div>

					{message && (
						<div
							className={`rounded-md border px-3 py-2 text-sm ${
								message.type === "success"
									? "border-green-500/20 bg-green-500/10 text-green-400"
									: "border-red-500/20 bg-red-500/10 text-red-400"
							}`}
						>
							{message.text}
						</div>
					)}

					<DialogFooter>
						<Button 
							onClick={() => {
								setAddingBinding(false);
								setMessage(null);
							}} 
							variant="ghost" 
							size="sm"
						>
							Cancel
						</Button>
						<Button
							onClick={handleAddBinding}
							loading={addBindingMutation.isPending}
							size="sm"
						>
							Add Binding
						</Button>
					</DialogFooter>
				</DialogContent>
			</Dialog>
		</div>
	);
}

interface PlatformCardProps {
	name: string;
	description: string;
	status?: PlatformStatus;
	onSetup: () => void;
}

function PlatformCard({ name, description, status, onSetup }: PlatformCardProps) {
	const configured = status?.configured ?? false;
	const enabled = status?.enabled ?? false;

	return (
		<div className="rounded-lg border border-app-line bg-app-box p-4">
			<div className="flex items-center justify-between">
				<div className="flex-1">
					<div className="flex items-center gap-2">
						<span className="text-sm font-medium text-ink">{name}</span>
						{configured && (
							<span className={`text-tiny ${enabled ? "text-green-400" : "text-ink-faint"}`}>
								{enabled ? "● Active" : "○ Disabled"}
							</span>
						)}
					</div>
					<p className="mt-0.5 text-sm text-ink-dull">{description}</p>
				</div>
				<div className="flex gap-2">
					<Button onClick={onSetup} variant="outline" size="sm">
						{configured ? "Configure" : "Setup"}
					</Button>
				</div>
			</div>
		</div>
	);
}
