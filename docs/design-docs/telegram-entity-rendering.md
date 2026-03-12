---
summary: Plan for replacing Telegram HTML/regex formatting recovery with a tolerant entity-first renderer.
read_when:
  - You are redesigning Telegram formatting behavior.
  - You need a migration plan for model-agnostic Telegram rendering.
  - You are deciding how much of the current regex cleanup layer should survive.
---

# Telegram Entity-First Rendering Plan

## Context

The current Telegram adapter path is:

```text
LLM output -> regex cleanup -> pulldown-cmark -> Telegram HTML
```

This has improved the worst failures, but it is still structurally brittle:

- it depends on the model emitting markdown that is at least partly parseable
- it depends on regex cleanup anticipating future malformed output shapes
- it depends on Telegram HTML parsing for correctness

Recent live outputs on `mini` still show the same seam classes:

- prose token glued to a heading or section label
- bold heading glued to a list marker
- word glued to a number/date/time token
- list item tail glued to the next sentence or section
- report/table-heavy markdown being relayed too literally into chat

The long-term goal is to make Telegram formatting work predictably for future models without having to retune each model or add more phrase-specific repairs.

## Goals

- Make Telegram output readable even when model markdown is imperfect.
- Support future models without model-specific patching.
- Keep Telegram-specific degradation logic in the Telegram adapter.
- Prefer deterministic output over preserving markdown punctuation.
- Preserve code spans/blocks, links, emphasis, and list structure where feasible.
- Replace most current regex hacks with structural normalization.

## Non-Goals

- Perfect round-tripping of arbitrary GitHub-flavored markdown.
- Rewriting all adapters to use Telegram semantics.
- Solving channel-behavior issues purely in the renderer.
- Building a full markdown parser from scratch.

## Target Architecture

The target path is:

```text
LLM output
  -> tolerant structural normalization
  -> Telegram document model
  -> Telegram text + entities
```

This replaces the current "repair malformed markdown strings" design with a smaller, explicit formatting pipeline.

## Core Design

### 1. Tolerant structural normalization

Normalize only non-code content. Inline code and fenced code stay literal.

The normalizer should repair transitions, not phrases:

- prose word -> number/date/time token
- prose token -> section label
- heading/emphasis end -> ordered or bullet list marker
- list item tail -> next sentence/section
- token/URL/inline-code boundary -> sentence starter or section start

This layer should be small and generic. It may still use some regex internally, but only for token-class transitions, not content-shaped string matching.

### 2. Telegram document model

Lower the normalized text into a Telegram-safe intermediate representation:

- `Paragraph`
- `Heading`
- `BulletList`
- `NumberedList`
- `LabelValueList`
- `Quote`
- `CodeBlock`
- `Divider` (only if needed)

The adapter should degrade unsupported markdown into this smaller block vocabulary:

- markdown headings -> `Heading`
- tables -> `LabelValueList` or bullet list rows
- task lists -> bullet list with checkbox text
- nested lists -> flattened list structure when nesting would render poorly

### 3. Entity-first rendering

Render the Telegram document model into:

- plain text
- `MessageEntity` spans

This becomes the primary send path. HTML parse mode becomes secondary or removable later.

Benefits:

- avoids Telegram HTML parser quirks
- makes formatting explicit and testable
- matches Telegram's native formatting model
- makes chunk splitting easier to reason about once entity offsets are owned locally

## Why This Is Better

This design is model-agnostic in the right place:

- prompt guidance still helps, but is not required for correctness
- malformed output is degraded structurally, not cosmetically
- future models can emit different markdown-ish text and still land in the same Telegram-safe document model

## Planned Migration

### Slice 1: document the redesign and capture failure taxonomy

- write this plan
- classify current live failures into structural seam types
- replace message-derived fixtures with synthetic seam fixtures where needed

### Slice 2: introduce the Telegram document model

- define Telegram-local block/span types
- isolate current markdown lowering logic from direct HTML rendering
- keep current send path alive while the IR is introduced

### Slice 3: build tolerant structural normalization

- preserve fenced code and inline code
- replace most phrase-specific repairs with token-transition repairs
- add tests for seam classes, not copied live outputs

### Slice 4: lower markdown-ish input into Telegram blocks

- keep `pulldown-cmark` where it helps
- flatten tables, task lists, and headings into the Telegram document model
- ensure malformed-but-fixable input still becomes readable blocks

### Slice 5: render Telegram blocks to `text + entities`

- add entity-aware rendering for:
  - bold
  - italic
  - links
  - inline code
  - code blocks / pre
- keep plain-text fallback

### Slice 6: swap outbound sending to entity-first delivery

- send messages with explicit entities instead of `parse_mode = Html`
- add chunk splitting that preserves entity offsets
- retain a narrow plain-text fallback path for send failures

### Slice 7: clean up old HTML/regex salvage code

- remove obsolete regex repairs
- remove HTML-only rendering helpers that are no longer used
- keep only the minimal structural normalization still required before block lowering

## Prompt and Channel Changes

The renderer alone will not solve all Telegram UX issues. Separate prompt-level cleanup is still needed:

- reduce "Checking... Stand by." preview replies
- require branch conclusions to be relayable as chat, not mini-reports
- keep Telegram guidance specific to Telegram

These changes belong in:

- `prompts/en/adapters/telegram.md.j2`
- `prompts/en/channel.md.j2`
- branch/tool prompt files where report-shaped conclusions are currently encouraged

## Testing Strategy

### Adapter tests

Use synthetic fixtures only. Test seam classes rather than copied messages:

- `Token**Heading**1.`
- `Word12`
- `https://example.com/xYz9She`
- `label:- Item`
- `topicKey points:`

### Entity tests

Verify:

- entity ranges are valid UTF-16 offsets
- chunk splitting preserves entity boundaries
- code and links survive conversion
- unsupported structures degrade to readable plain text

### Parity checks

Telegram-specific rendering should stay Telegram-local. Other adapters should not inherit Telegram degradation rules.

## Expected Code Boundaries

Likely files:

- `src/messaging/telegram.rs`
- a new Telegram-local formatting module if `telegram.rs` grows too large
- `prompts/en/adapters/telegram.md.j2`
- `prompts/en/channel.md.j2`
- Telegram tests

## What Happens to the Current Regex Layer

Most of it should go away.

What remains:

- a small structural seam-repair layer before block lowering
- code-span protection
- a few generic transition repairs if they still prove useful

What goes away:

- phrase-specific formatting repairs
- growing sets of report-shaped regex patches
- reliance on HTML parse mode as the primary representation

## Work Split

Parallel work can be split into:

1. Failure taxonomy and synthetic fixture design
2. Telegram entity/chunking design
3. Adapter-parity and boundary review
4. Main integration in the adapter

## Exit Criteria

The redesign is successful when:

- dense report-style replies become readable Telegram chat consistently
- unsupported markdown degrades predictably
- new models do not require custom Telegram-specific regex patches
- the adapter primarily sends `text + entities`
- prompt guidance improves output quality but is no longer the main correctness mechanism
