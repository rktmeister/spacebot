import { Fragment, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
	api,
	type CalendarEvent,
	type CalendarEventDraft,
	type CalendarOccurrence,
	type CalendarOverview,
	type CalendarProposal,
} from "@/api/client";
import { formatTimeAgo } from "@/lib/format";
import { HugeiconsIcon } from "@hugeicons/react";
import {
	Delete02Icon,
	FlashIcon,
	PencilEdit02Icon,
} from "@hugeicons/core-free-icons";
import {
	Badge,
	Button,
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
	Input,
	TextArea,
	buttonStyles,
	cx,
	Toggle,
} from "@/ui";

type ViewMode = "month" | "week";
type EditorMode = "create" | "edit";

interface AgentCalendarProps {
	agentId: string;
}

interface EventFormState {
	summary: string;
	description: string;
	location: string;
	start_at: string;
	end_at: string;
	timezone: string;
	all_day: boolean;
	recurrence_rule: string;
}

const WEEKDAY_LABELS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const URL_PATTERN = /https?:\/\/[^\s<>"']+/gi;
const MEETING_HOST_PATTERN =
	/(?:^|\.)((?:us\d+web\.)?zoom(?:gov)?\.us|teams\.microsoft\.com|meet\.google\.com|webex\.com|gotomeeting\.com|meet\.jit\.si|whereby\.com)$/i;

function browserTimeZone(): string {
	return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
}

function resolveSupportedTimeZone(timezone?: string | null): string {
	const candidate = timezone?.trim();
	if (candidate) {
		try {
			new Intl.DateTimeFormat([], { timeZone: candidate });
			return candidate;
		} catch {}
	}
	return browserTimeZone();
}

function formatPartsInTimeZone(
	date: Date,
	timeZone: string,
	includeTime: boolean,
): Record<string, string> {
	const formatter = new Intl.DateTimeFormat("en-CA", {
		timeZone,
		year: "numeric",
		month: "2-digit",
		day: "2-digit",
		...(includeTime
			? {
					hour: "2-digit",
					minute: "2-digit",
					hour12: false,
			  }
			: {}),
	});

	return Object.fromEntries(
		formatter
			.formatToParts(date)
			.filter((part) => part.type !== "literal")
			.map((part) => [part.type, part.value]),
	);
}

function startOfDay(date: Date): Date {
	return new Date(date.getFullYear(), date.getMonth(), date.getDate());
}

function startOfWeek(date: Date): Date {
	const normalized = startOfDay(date);
	normalized.setDate(normalized.getDate() - normalized.getDay());
	return normalized;
}

function startOfMonth(date: Date): Date {
	return new Date(date.getFullYear(), date.getMonth(), 1);
}

function addDays(date: Date, days: number): Date {
	const next = new Date(date);
	next.setDate(next.getDate() + days);
	return next;
}

function addMonths(date: Date, months: number): Date {
	return new Date(date.getFullYear(), date.getMonth() + months, 1);
}

function dayKey(value: string | Date): string {
	const date = typeof value === "string" ? new Date(value) : value;
	return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(
		date.getDate(),
	).padStart(2, "0")}`;
}

function toCalendarInputValue(
	value: string,
	allDay: boolean,
	timezone?: string | null,
): string {
	const date = new Date(value);
	if (Number.isNaN(date.getTime())) return "";
	const resolvedTimeZone = resolveSupportedTimeZone(timezone);
	const parts = formatPartsInTimeZone(date, resolvedTimeZone, !allDay);
	const day = `${parts.year}-${parts.month}-${parts.day}`;
	if (allDay) return day;
	return `${day}T${parts.hour}:${parts.minute}`;
}

function defaultFormState(baseDate: Date, timezone?: string | null): EventFormState {
	const resolvedTimeZone = resolveSupportedTimeZone(timezone);
	const day = dayKey(baseDate);
	return {
		summary: "",
		description: "",
		location: "",
		start_at: `${day}T09:00`,
		end_at: `${day}T10:00`,
		timezone: resolvedTimeZone,
		all_day: false,
		recurrence_rule: "",
	};
}

function eventToFormState(event: CalendarEvent): EventFormState {
	return {
		summary: event.summary ?? "",
		description: event.description ?? "",
		location: event.location ?? "",
		start_at: toCalendarInputValue(event.start_at_utc, event.all_day, event.timezone),
		end_at: toCalendarInputValue(event.end_at_utc, event.all_day, event.timezone),
		timezone: resolveSupportedTimeZone(event.timezone),
		all_day: event.all_day,
		recurrence_rule: event.recurrence_rule ?? "",
	};
}

function formToDraft(form: EventFormState): CalendarEventDraft {
	return {
		summary: form.summary.trim(),
		description: form.description.trim() || null,
		location: form.location.trim() || null,
		start_at: form.start_at,
		end_at: form.end_at,
		timezone: form.timezone.trim() || null,
		all_day: form.all_day,
		recurrence_rule: form.recurrence_rule.trim() || null,
		attendees: [],
	};
}

function rangeForView(viewMode: ViewMode, cursor: Date): { start: Date; end: Date } {
	if (viewMode === "week") {
		const start = startOfWeek(cursor);
		return { start, end: addDays(start, 7) };
	}
	const monthStart = startOfMonth(cursor);
	const start = startOfWeek(monthStart);
	return { start, end: addDays(start, 42) };
}

function formatPanelTime(
	value: string,
	allDay: boolean,
	timezone?: string | null,
): string {
	const date = new Date(value);
	if (Number.isNaN(date.getTime())) return value;
	const resolvedTimeZone = resolveSupportedTimeZone(timezone);
	return allDay
		? date.toLocaleDateString([], {
				timeZone: resolvedTimeZone,
				month: "short",
				day: "numeric",
				year: "numeric",
		  })
		: date.toLocaleString([], {
				timeZone: resolvedTimeZone,
				month: "short",
				day: "numeric",
				hour: "numeric",
				minute: "2-digit",
		  });
}

function extractUrls(text?: string | null): string[] {
	if (!text) return [];
	const matches = Array.from(text.matchAll(URL_PATTERN), (match) =>
		match[0].replace(/[),.;]+$/u, "").replace(/^<|>$/gu, ""),
	).filter(Boolean);
	return Array.from(new Set(matches));
}

function unfoldIcsText(rawIcs?: string | null): string {
	if (!rawIcs) return "";
	return rawIcs.replace(/\r?\n[ \t]/g, "");
}

function extractMeetingUrl(event?: CalendarEvent | null): string | null {
	if (!event) return null;
	const candidates = [
		...extractUrls(event.location),
		...extractUrls(event.description),
		...extractUrls(unfoldIcsText(event.raw_ics)),
	];
	if (candidates.length === 0) return null;
	const meetingUrl = candidates.find((url) => {
		try {
			return MEETING_HOST_PATTERN.test(new URL(url).hostname);
		} catch {
			return false;
		}
	});
	return meetingUrl ?? candidates[0] ?? null;
}

function renderLinkifiedText(text: string) {
	const matches = Array.from(text.matchAll(URL_PATTERN));
	if (matches.length === 0) return text;

	const nodes = [];
	let cursor = 0;
	for (const match of matches) {
		const rawUrl = match[0];
		const start = match.index ?? 0;
		const normalizedUrl = rawUrl
			.replace(/[),.;]+$/u, "")
			.replace(/^<|>$/gu, "");
		const normalizedLength = normalizedUrl.length;
		if (start > cursor) {
			nodes.push(text.slice(cursor, start));
		}
		nodes.push(
			<a
				key={`${start}-${normalizedUrl}`}
				href={normalizedUrl}
				target="_blank"
				rel="noopener noreferrer"
				className="break-all text-accent hover:underline"
			>
				{normalizedUrl}
			</a>,
		);
		cursor = start + normalizedLength;
		if (rawUrl.length > normalizedLength) {
			nodes.push(rawUrl.slice(normalizedLength));
			cursor = start + rawUrl.length;
		}
	}
	if (cursor < text.length) {
		nodes.push(text.slice(cursor));
	}
	return (
		<>
			{nodes.map((node, index) => (
				<Fragment key={index}>{node}</Fragment>
			))}
		</>
	);
}

function calendarLabel(overview?: CalendarOverview): string {
	const selected = overview?.calendars.find((calendar) => calendar.is_selected);
	return selected?.display_name || selected?.href || "No calendar selected";
}

function formatViewLabel(viewMode: ViewMode, cursorDate: Date): string {
	if (viewMode === "month") {
		return cursorDate.toLocaleDateString([], {
			month: "long",
			year: "numeric",
		});
	}

	const weekStart = startOfWeek(cursorDate);
	const weekEnd = addDays(weekStart, 6);
	const sameMonth = weekStart.getMonth() === weekEnd.getMonth();
	const sameYear = weekStart.getFullYear() === weekEnd.getFullYear();
	if (sameMonth && sameYear) {
		return `${weekStart.toLocaleDateString([], { month: "long" })} ${weekStart.getDate()}-${weekEnd.getDate()}, ${weekStart.getFullYear()}`;
	}
	if (sameYear) {
		return `${weekStart.toLocaleDateString([], { month: "short", day: "numeric" })} - ${weekEnd.toLocaleDateString([], { month: "short", day: "numeric" })}, ${weekStart.getFullYear()}`;
	}
	return `${weekStart.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" })} - ${weekEnd.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" })}`;
}

export function AgentCalendar({ agentId }: AgentCalendarProps) {
	const queryClient = useQueryClient();
	const [viewMode, setViewMode] = useState<ViewMode>("month");
	const [cursorDate, setCursorDate] = useState(() => startOfDay(new Date()));
	const [selectedOccurrence, setSelectedOccurrence] = useState<CalendarOccurrence | null>(null);
	const [editorMode, setEditorMode] = useState<EditorMode>("create");
	const [editorOpen, setEditorOpen] = useState(false);
	const [proposal, setProposal] = useState<CalendarProposal | null>(null);
	const [formState, setFormState] = useState<EventFormState>(() => defaultFormState(new Date()));
	const [editorError, setEditorError] = useState<string | null>(null);
	const [copiedIcs, setCopiedIcs] = useState(false);

	const range = useMemo(() => rangeForView(viewMode, cursorDate), [cursorDate, viewMode]);

	const overviewQuery = useQuery({
		queryKey: ["calendar-overview", agentId],
		queryFn: () => api.calendarOverview(agentId),
		refetchInterval: 30_000,
	});

	const occurrencesQuery = useQuery({
		queryKey: ["calendar-occurrences", agentId, viewMode, range.start.toISOString(), range.end.toISOString()],
		queryFn: () =>
			api.calendarEvents(agentId, {
				start_at: range.start.toISOString(),
				end_at: range.end.toISOString(),
			}),
		enabled: Boolean(overviewQuery.data?.enabled && overviewQuery.data?.selected_calendar_href),
		refetchInterval: 30_000,
	});

	const selectedEventQuery = useQuery({
		queryKey: ["calendar-event", agentId, selectedOccurrence?.event_id],
		queryFn: () => api.calendarEvent(agentId, selectedOccurrence!.event_id),
		enabled: Boolean(selectedOccurrence?.event_id),
	});

	const syncMutation = useMutation({
		mutationFn: () => api.calendarSync(agentId),
		onSuccess: () => {
			queryClient.invalidateQueries({ queryKey: ["calendar-overview", agentId] });
			queryClient.invalidateQueries({ queryKey: ["calendar-occurrences", agentId] });
			if (selectedOccurrence?.event_id) {
				queryClient.invalidateQueries({ queryKey: ["calendar-event", agentId, selectedOccurrence.event_id] });
			}
		},
	});

	const createProposalMutation = useMutation({
		mutationFn: (draft: CalendarEventDraft) => api.calendarCreateProposal(agentId, draft),
		onSuccess: (result) => {
			setProposal(result.proposal);
			setEditorOpen(false);
		},
		onError: (error: Error) => setEditorError(error.message),
	});

	const updateProposalMutation = useMutation({
		mutationFn: ({ eventId, draft }: { eventId: string; draft: CalendarEventDraft }) =>
			api.calendarUpdateProposal(agentId, eventId, draft),
		onSuccess: (result) => {
			setProposal(result.proposal);
			setEditorOpen(false);
		},
		onError: (error: Error) => setEditorError(error.message),
	});

	const deleteProposalMutation = useMutation({
		mutationFn: (eventId: string) => api.calendarDeleteProposal(agentId, eventId),
		onSuccess: (result) => setProposal(result.proposal),
	});

	const applyProposalMutation = useMutation({
		mutationFn: (proposalId: string) => api.calendarApplyProposal(agentId, proposalId),
		onSuccess: () => {
			setProposal(null);
			queryClient.invalidateQueries({ queryKey: ["calendar-overview", agentId] });
			queryClient.invalidateQueries({ queryKey: ["calendar-occurrences", agentId] });
			queryClient.invalidateQueries({ queryKey: ["calendar-event", agentId] });
			setSelectedOccurrence(null);
		},
	});

	const occurrences = occurrencesQuery.data?.occurrences ?? [];
	const viewLabel = useMemo(() => formatViewLabel(viewMode, cursorDate), [cursorDate, viewMode]);
	const selectedEvent = selectedEventQuery.data?.event;
	const selectedCalendar = useMemo(
		() => overviewQuery.data?.calendars.find((calendar) => calendar.is_selected),
		[overviewQuery.data],
	);
	const meetingUrl = useMemo(() => extractMeetingUrl(selectedEvent), [selectedEvent]);
	const groupedOccurrences = useMemo(() => {
		const grouped = new Map<string, CalendarOccurrence[]>();
		for (const occurrence of occurrences) {
			const key = dayKey(occurrence.start_at);
			const bucket = grouped.get(key) ?? [];
			bucket.push(occurrence);
			grouped.set(key, bucket);
		}
		for (const bucket of grouped.values()) {
			bucket.sort((left, right) => left.start_at.localeCompare(right.start_at));
		}
		return grouped;
	}, [occurrences]);

	const openCreate = () => {
		setEditorMode("create");
		setFormState(defaultFormState(cursorDate, selectedCalendar?.timezone));
		setEditorError(null);
		setEditorOpen(true);
	};

	const openEdit = () => {
		if (!selectedEvent) return;
		setEditorMode("edit");
		setFormState(eventToFormState(selectedEvent));
		setEditorError(null);
		setEditorOpen(true);
	};

	const handleSubmit = () => {
		const draft = formToDraft(formState);
		if (!draft.summary) {
			setEditorError("Title is required.");
			return;
		}
		if (!draft.start_at || !draft.end_at) {
			setEditorError("Start and end are required.");
			return;
		}
		if (draft.end_at <= draft.start_at) {
			setEditorError("End must be after start.");
			return;
		}

		if (editorMode === "create") {
			createProposalMutation.mutate(draft);
			return;
		}
		if (!selectedEvent) {
			setEditorError("Select an event before editing.");
			return;
		}
		updateProposalMutation.mutate({ eventId: selectedEvent.id, draft });
	};

	const copyIcsUrl = async () => {
		const url = overviewQuery.data?.ics_export_url;
		if (!url) return;
		await navigator.clipboard.writeText(url);
		setCopiedIcs(true);
		window.setTimeout(() => setCopiedIcs(false), 1200);
	};

	const shiftCursor = (direction: -1 | 1) => {
		setCursorDate((current) =>
			viewMode === "month" ? addMonths(current, direction) : addDays(current, direction * 7),
		);
	};

	const renderMonth = () => {
		const firstMonthDay = startOfMonth(cursorDate);
		const gridStart = startOfWeek(firstMonthDay);

		return (
			<div className="grid h-full grid-cols-7 grid-rows-[auto_repeat(6,minmax(0,1fr))] border-t border-app-line">
				{WEEKDAY_LABELS.map((label) => (
					<div
						key={label}
						className="border-b border-r border-app-line px-3 py-2 text-xs uppercase tracking-[0.18em] text-ink-faint"
					>
						{label}
					</div>
				))}
				{Array.from({ length: 42 }, (_, index) => {
					const currentDay = addDays(gridStart, index);
					const dayOccurrences = groupedOccurrences.get(dayKey(currentDay)) ?? [];
					const inMonth = currentDay.getMonth() === cursorDate.getMonth();
					return (
						<button
							key={currentDay.toISOString()}
							type="button"
							className={cx(
								"flex min-h-0 flex-col border-b border-r border-app-line px-2 py-2 text-left transition-colors",
								inMonth ? "bg-app-darkBox/10" : "bg-app-darkBox/5 text-ink-faint",
								dayKey(currentDay) === dayKey(new Date()) && "bg-accent/8",
							)}
							onClick={() => {
								setCursorDate(currentDay);
								if (dayOccurrences[0]) setSelectedOccurrence(dayOccurrences[0]);
							}}
						>
							<div className="mb-2 flex items-center justify-between">
								<span className="font-plex text-sm">{currentDay.getDate()}</span>
								{dayOccurrences.length > 0 && <Badge variant="outline" size="sm">{dayOccurrences.length}</Badge>}
							</div>
							<div className="space-y-1 overflow-hidden">
								{dayOccurrences.slice(0, 3).map((occurrence) => (
									<div
										key={occurrence.occurrence_id}
										className="truncate rounded-md border border-app-line bg-app-darkBox/70 px-2 py-1 text-xs text-ink"
									>
										{occurrence.summary || "Untitled event"}
									</div>
								))}
								{dayOccurrences.length > 3 && (
									<div className="text-xs text-ink-faint">+{dayOccurrences.length - 3} more</div>
								)}
							</div>
						</button>
					);
				})}
			</div>
		);
	};

	const renderWeek = () => {
		const weekStart = startOfWeek(cursorDate);
		return (
			<div className="grid h-full grid-cols-7 border-t border-app-line">
				{Array.from({ length: 7 }, (_, index) => {
					const currentDay = addDays(weekStart, index);
					const dayOccurrences = groupedOccurrences.get(dayKey(currentDay)) ?? [];
					return (
						<div key={currentDay.toISOString()} className="flex min-h-0 flex-col border-r border-app-line">
							<div className="border-b border-app-line px-3 py-3">
								<div className="text-xs uppercase tracking-[0.18em] text-ink-faint">{WEEKDAY_LABELS[index]}</div>
								<div className="mt-1 font-plex text-lg text-ink">{currentDay.getDate()}</div>
							</div>
							<div className="flex-1 space-y-2 overflow-y-auto px-2 py-2">
								{dayOccurrences.map((occurrence) => (
									<button
										key={occurrence.occurrence_id}
										type="button"
										className={cx(
											"w-full rounded-xl border px-3 py-2 text-left transition-colors",
											selectedOccurrence?.occurrence_id === occurrence.occurrence_id
												? "border-accent bg-accent/12"
												: "border-app-line bg-app-darkBox/40 hover:bg-app-darkBox/70",
										)}
										onClick={() => setSelectedOccurrence(occurrence)}
									>
										<div className="text-xs text-ink-faint">
											{occurrence.all_day
												? "All day"
												: formatPanelTime(occurrence.start_at, false, occurrence.timezone)}
										</div>
										<div className="mt-1 text-sm text-ink">{occurrence.summary || "Untitled event"}</div>
										{occurrence.location && (
											<div className="mt-1 truncate text-xs text-ink-faint">{occurrence.location}</div>
										)}
									</button>
								))}
								{dayOccurrences.length === 0 && (
									<div className="rounded-xl border border-dashed border-app-line px-3 py-4 text-sm text-ink-faint">
										No events
									</div>
								)}
							</div>
						</div>
					);
				})}
			</div>
		);
	};

	return (
		<div className="flex h-full min-h-0 flex-col">
			<div className="flex items-center gap-2 border-b border-app-line px-6 py-3">
				<Badge variant="accent" size="md">{viewMode}</Badge>
				<Badge variant="outline" size="md">{calendarLabel(overviewQuery.data)}</Badge>
				<div className="font-plex text-sm text-ink">{viewLabel}</div>
				{overviewQuery.data?.source?.last_successful_sync_at && (
					<span className="text-xs text-ink-faint">
						synced {formatTimeAgo(overviewQuery.data.source.last_successful_sync_at)}
					</span>
				)}
				<div className="flex-1" />
				<div className="flex items-center gap-2">
					<Button variant="outline" size="sm" onClick={() => setCursorDate(startOfDay(new Date()))}>Today</Button>
					<Button variant="outline" size="sm" onClick={() => shiftCursor(-1)}>Prev</Button>
					<Button variant="outline" size="sm" onClick={() => shiftCursor(1)}>Next</Button>
					<Button variant={viewMode === "month" ? "default" : "outline"} size="sm" onClick={() => setViewMode("month")}>
						Month
					</Button>
					<Button variant={viewMode === "week" ? "default" : "outline"} size="sm" onClick={() => setViewMode("week")}>
						Week
					</Button>
					<Button
						variant="outline"
						size="sm"
						loading={syncMutation.isPending}
						leftIcon={<HugeiconsIcon icon={FlashIcon} className="h-4 w-4" />}
						onClick={() => syncMutation.mutate()}
					>
						Sync
					</Button>
					<Button size="sm" onClick={openCreate}>New Event</Button>
				</div>
			</div>

			<div className="flex min-h-0 flex-1">
				<div className="min-w-0 flex-1 overflow-hidden">
					{overviewQuery.isLoading ? (
						<div className="flex h-full items-center justify-center text-sm text-ink-faint">Loading calendar…</div>
					) : !overviewQuery.data?.enabled ? (
						<div className="flex h-full items-center justify-center px-6 text-sm text-ink-faint">
							Calendar is disabled for this agent.
						</div>
					) : !overviewQuery.data?.selected_calendar_href ? (
						<div className="flex h-full items-center justify-center px-6 text-sm text-ink-faint">
							Sync is working, but no selected calendar is pinned yet. Pick one by setting <code>calendar.selected_calendar_href</code> to one of the discovered href values shown in the drawer.
						</div>
					) : occurrencesQuery.isLoading ? (
						<div className="flex h-full items-center justify-center text-sm text-ink-faint">Loading events…</div>
					) : occurrencesQuery.isError ? (
						<div className="flex h-full items-center justify-center px-6 text-center text-sm text-red-400">
							Failed to load calendar events. {occurrencesQuery.error.message}
						</div>
					) : viewMode === "month" ? (
						renderMonth()
					) : (
						renderWeek()
					)}
				</div>

				<aside className="flex w-[24rem] flex-col border-l border-app-line bg-app-darkBox/15">
					<div className="border-b border-app-line px-5 py-4">
						<div className="text-xs uppercase tracking-[0.18em] text-ink-faint">Calendar</div>
						<div className="mt-2 font-plex text-lg text-ink">{calendarLabel(overviewQuery.data)}</div>
						{overviewQuery.data?.ics_export_url && (
							<div className="mt-3 space-y-2 rounded-xl border border-app-line bg-app-darkBox/50 p-3">
								<div className="text-xs uppercase tracking-[0.14em] text-ink-faint">Read-only ICS</div>
								<div className="break-all text-xs text-ink-dull">{overviewQuery.data.ics_export_url}</div>
								<Button variant="outline" size="sm" onClick={copyIcsUrl}>
									{copiedIcs ? "Copied" : "Copy URL"}
								</Button>
							</div>
						)}
					</div>

					<div className="flex-1 space-y-4 overflow-y-auto px-5 py-4">
						{selectedOccurrence && selectedEvent ? (
							<>
								<div>
									<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Selected Event</div>
									<h2 className="mt-2 font-plex text-xl text-ink">{selectedEvent.summary || "Untitled event"}</h2>
									<div className="mt-2 text-sm text-ink-dull">
										{formatPanelTime(
											selectedOccurrence.start_at,
											selectedOccurrence.all_day,
											selectedOccurrence.timezone ?? selectedEvent.timezone,
										)}{" "}
										to{" "}
										{formatPanelTime(
											selectedOccurrence.end_at,
											selectedOccurrence.all_day,
											selectedOccurrence.timezone ?? selectedEvent.timezone,
										)}
									</div>
									{selectedEvent.timezone && <div className="text-sm text-ink-faint">{selectedEvent.timezone}</div>}
								</div>

								<div className="flex gap-2">
									<Button
										variant="outline"
										size="sm"
										leftIcon={<HugeiconsIcon icon={PencilEdit02Icon} className="h-4 w-4" />}
										onClick={openEdit}
									>
										Edit
									</Button>
									<Button
										variant="destructive"
										size="sm"
										leftIcon={<HugeiconsIcon icon={Delete02Icon} className="h-4 w-4" />}
										onClick={() => deleteProposalMutation.mutate(selectedEvent.id)}
										loading={deleteProposalMutation.isPending}
									>
										Delete
									</Button>
									{meetingUrl && (
										<a
											href={meetingUrl}
											target="_blank"
											rel="noopener noreferrer"
											className={buttonStyles({ variant: "outline", size: "sm" })}
										>
											Open Meeting
										</a>
									)}
								</div>

								{selectedEvent.location && (
									<div>
										<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Location</div>
										<div className="mt-1 text-sm text-ink">
											{renderLinkifiedText(selectedEvent.location)}
										</div>
									</div>
								)}

								{selectedEvent.description && (
									<div>
										<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Description</div>
										<div className="mt-1 whitespace-pre-wrap text-sm text-ink-dull">
											{renderLinkifiedText(selectedEvent.description)}
										</div>
									</div>
								)}

								<div>
									<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Attendees</div>
									<div className="mt-2 space-y-2">
										{selectedEvent.attendees.length === 0 ? (
											<div className="text-sm text-ink-faint">No attendees recorded.</div>
										) : (
											selectedEvent.attendees.map((attendee) => (
												<div key={attendee.id} className="rounded-xl border border-app-line bg-app-darkBox/40 px-3 py-2">
													<div className="text-sm text-ink">
														{attendee.common_name || attendee.email || "Unknown attendee"}
													</div>
													<div className="text-xs text-ink-faint">
														{[attendee.email, attendee.role, attendee.partstat].filter(Boolean).join(" • ")}
													</div>
												</div>
											))
										)}
									</div>
								</div>
							</>
						) : (
							<>
								<div className="rounded-2xl border border-app-line bg-app-darkBox/35 p-4">
									<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Status</div>
									<div className="mt-2 flex flex-wrap gap-2">
										<Badge variant="outline" size="md">{overviewQuery.data?.provider_kind || "caldav"}</Badge>
										<Badge variant={overviewQuery.data?.read_only ? "amber" : "green"} size="md">
											{overviewQuery.data?.read_only ? "read only" : "write enabled"}
										</Badge>
										{overviewQuery.data?.source?.sync_status && (
											<Badge variant="blue" size="md">{overviewQuery.data.source.sync_status}</Badge>
										)}
									</div>
									{overviewQuery.data?.source?.last_error && (
										<div className="mt-3 text-sm text-red-400">{overviewQuery.data.source.last_error}</div>
									)}
								</div>

								<div>
									<div className="text-xs uppercase tracking-[0.16em] text-ink-faint">Discovered Calendars</div>
									<div className="mt-3 space-y-2">
										{overviewQuery.data?.calendars.map((calendar) => (
											<div key={calendar.href} className="rounded-2xl border border-app-line bg-app-darkBox/35 p-4">
												<div className="flex items-center gap-2">
													<div className="font-plex text-sm text-ink">{calendar.display_name || calendar.href}</div>
													{calendar.is_selected && <Badge variant="accent" size="sm">selected</Badge>}
												</div>
												<div className="mt-2 break-all text-xs text-ink-faint">{calendar.href}</div>
												{calendar.last_synced_at && (
													<div className="mt-2 text-xs text-ink-faint">
														last synced {formatTimeAgo(calendar.last_synced_at)}
													</div>
												)}
											</div>
										))}
									</div>
								</div>
							</>
						)}
					</div>
				</aside>
			</div>

			<Dialog open={editorOpen} onOpenChange={setEditorOpen}>
				<DialogContent className="max-w-2xl">
					<DialogHeader>
						<DialogTitle>{editorMode === "create" ? "Create Event" : "Edit Event"}</DialogTitle>
					</DialogHeader>
					<div className="grid gap-4 md:grid-cols-2">
						<div className="md:col-span-2">
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Title</label>
							<Input value={formState.summary} onChange={(event) => setFormState((current) => ({ ...current, summary: event.target.value }))} />
						</div>
						<div>
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Start</label>
							<Input
								type={formState.all_day ? "date" : "datetime-local"}
								value={formState.start_at}
								onChange={(event) => setFormState((current) => ({ ...current, start_at: event.target.value }))}
							/>
						</div>
						<div>
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">End</label>
							<Input
								type={formState.all_day ? "date" : "datetime-local"}
								value={formState.end_at}
								onChange={(event) => setFormState((current) => ({ ...current, end_at: event.target.value }))}
							/>
						</div>
						<div>
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Timezone</label>
							<Input value={formState.timezone} onChange={(event) => setFormState((current) => ({ ...current, timezone: event.target.value }))} />
						</div>
						<div className="flex items-center gap-3 pt-6">
							<Toggle
								checked={formState.all_day}
								onCheckedChange={(checked) => setFormState((current) => ({ ...current, all_day: checked }))}
							/>
							<span className="text-sm text-ink">All day</span>
						</div>
						<div className="md:col-span-2">
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Location</label>
							<Input value={formState.location} onChange={(event) => setFormState((current) => ({ ...current, location: event.target.value }))} />
						</div>
						<div className="md:col-span-2">
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Recurrence Rule</label>
							<Input value={formState.recurrence_rule} onChange={(event) => setFormState((current) => ({ ...current, recurrence_rule: event.target.value }))} placeholder="Optional RRULE, for example FREQ=WEEKLY;BYDAY=MO" />
						</div>
						<div className="md:col-span-2">
							<label className="mb-1 block text-xs uppercase tracking-[0.16em] text-ink-faint">Description</label>
							<TextArea rows={6} value={formState.description} onChange={(event) => setFormState((current) => ({ ...current, description: event.target.value }))} />
						</div>
					</div>
					{editorError && <div className="text-sm text-red-400">{editorError}</div>}
					<div className="flex justify-end gap-2">
						<Button variant="outline" onClick={() => setEditorOpen(false)}>Cancel</Button>
						<Button
							onClick={handleSubmit}
							loading={createProposalMutation.isPending || updateProposalMutation.isPending}
						>
							Review Proposal
						</Button>
					</div>
				</DialogContent>
			</Dialog>

			<Dialog open={Boolean(proposal)} onOpenChange={(open) => !open && setProposal(null)}>
				<DialogContent className="max-w-2xl">
					<DialogHeader>
						<DialogTitle>Review Calendar Change</DialogTitle>
					</DialogHeader>
					{proposal && (
						<div className="space-y-4">
							<div className="flex items-center gap-2">
								<Badge variant="accent" size="md">{proposal.action}</Badge>
								<Badge variant="outline" size="md">{proposal.status}</Badge>
								<span className="text-sm text-ink-dull">{proposal.summary}</span>
							</div>
							<pre className="max-h-[22rem] overflow-auto rounded-2xl border border-app-line bg-app-darkBox/50 p-4 text-sm text-ink whitespace-pre-wrap">
								{proposal.diff}
							</pre>
							{proposal.error && <div className="text-sm text-red-400">{proposal.error}</div>}
							<div className="flex justify-end gap-2">
								<Button variant="outline" onClick={() => setProposal(null)}>Cancel</Button>
								<Button
									variant={proposal.action === "delete" ? "destructive" : "default"}
									onClick={() => applyProposalMutation.mutate(proposal.id)}
									loading={applyProposalMutation.isPending}
								>
									{proposal.action === "delete" ? "Delete Event" : "Apply Change"}
								</Button>
							</div>
						</div>
					)}
				</DialogContent>
			</Dialog>
		</div>
	);
}
