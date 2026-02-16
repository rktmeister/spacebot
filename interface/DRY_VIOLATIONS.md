# Interface DRY Violations & Hardcoded Patterns

A comprehensive audit of ugly hardcoded stuff and DRY violations in the interface codebase.

---

## Critical DRY Violations

### 1. Loading Spinner (10+ copies)

**Files affected:**
- `AgentChannels.tsx`
- `AgentMemories.tsx`
- `AgentCortex.tsx`
- `AgentCron.tsx`
- `AgentIngest.tsx`
- `AgentConfig.tsx`
- `Settings.tsx`
- `MemoryGraph.tsx`
- `Overview.tsx`

**Hardcoded pattern:**
```tsx
<div className="h-2 w-2 animate-pulse rounded-full bg-accent" />
```

**Fix:** Create a `LoadingDot` or `Spinner` component in `ui/`.

---

### 2. Input Styling (20+ copies)

**Pattern repeated everywhere:**
```tsx
className="w-full rounded-lg border border-app-line bg-app-darkBox px-3 py-2 text-sm text-ink placeholder:text-ink-faint focus:border-accent focus:outline-none"
```

**Files:** `AgentChannels.tsx` (8+ times), `AgentCron.tsx`, `Settings.tsx`

**Fix:** The `Input.tsx` component exists but raw inputs are used instead. Either use the component or create a shared class constant.

---

### 3. Platform Form Handling Duplication

**File:** `AgentChannels.tsx` (lines 154-198)

Huge if/else blocks for Discord vs Slack with nearly identical field handling:
- Both parse comma-separated IDs the same way
- Both have credential fields
- Both build request objects similarly

**Fix:** Abstract into a `PlatformBindingForm` component with platform-specific field configs.

---

### 4. Color/Style Maps Scattered Everywhere

#### TYPE_COLORS in `AgentMemories.tsx`:
```tsx
const TYPE_COLORS: Record<MemoryType, string> = {
  fact: "bg-blue-500/15 text-blue-400",
  // ...
};
```

#### EVENT_CATEGORY_COLORS in `AgentCortex.tsx`:
```tsx
const EVENT_CATEGORY_COLORS: Record<string, string> = {
  bulletin_generated: "bg-blue-500/15 text-blue-400",
  // ...
};
```

#### platformColor in `format.ts`:
```tsx
export function platformColor(platform: string): string {
  // similar pattern
}
```

#### StatusBadge styles in `AgentIngest.tsx`:
```tsx
const styles: Record<string, string> = {
  queued: "bg-amber-500/20 text-amber-400",
  // ...
};
```

**Fix:** Create a centralized `colors.ts` or `theme.ts` with all color mappings.

---

### 5. Stat Component Defined Multiple Times

**AgentCron.tsx** (lines 384-391):
```tsx
function Stat({ label, value, color }: { label: string; value: number; color: string }) {
  return (
    <div className="flex items-center gap-1.5">
      <span className={`font-plex text-lg font-semibold tabular-nums ${color}`}>{value}</span>
      <span className="text-xs text-ink-faint">{label}</span>
    </div>
  );
}
```

**AgentIngest.tsx** (lines 30-37): Nearly identical

**Fix:** Move to `ui/` components.

---

### 6. Field Component Duplication

**AgentChannels.tsx** (lines 663-676):
```tsx
function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <label className="text-xs font-medium text-ink-dull">{label}</label>
      {children}
    </div>
  );
}
```

**AgentCron.tsx** (lines 393-400): Nearly identical

**Fix:** Use the existing form field components in `ui/forms/` or create a shared one.

---

## Other Notable Issues

### 7. Cortex Chat Panel Toggle Duplication
The chat toggle button appears in at least 3 files with identical code:
- `AgentMemories.tsx`
- `AgentCortex.tsx`

### 8. Empty/Error/Loading State Patterns
Similar JSX structures repeated across all route files for:
- Empty states (icon + heading + description + action)
- Error states (red text in a box)
- Loading states (pulse dot + text)

### 9. Modal/Dialog Patterns
Similar dialog structures in:
- `AgentChannels.tsx` (binding modal)
- `AgentCron.tsx` (job modal)
- `Settings.tsx` (provider modal)

### 10. AnimatePresence Wrappers
Similar animation patterns repeated for:
- Expanded rows
- Chat panels
- Modal content

### 11. Config Field Components in AgentConfig.tsx
`ConfigNumberField`, `ConfigFloatField`, `ConfigToggleField` are defined inline but could be:
1. Moved to `ui/`
2. Made more generic (they're very similar)

### 12. Pagination Controls
Prev/Next button patterns appear in multiple files with similar styling.

### 13. Table Header Patterns
Grid column layouts for table headers repeated in:
- `AgentMemories.tsx`
- `AgentCortex.tsx` (event list)

---

## Summary

The **top 5 priorities** for DRY cleanup:

1. **Loading spinner component** - 10+ copies, easiest win
2. **Input styling** - Use existing Input component or create shared classes
3. **Color maps consolidation** - Centralize all color/style mappings
4. **Stat/Field components** - Move to shared UI components
5. **Platform form abstraction** - Refactor AgentChannels platform handling
