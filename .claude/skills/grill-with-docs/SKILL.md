---
name: grill-with-docs
description: Grilling session that challenges your plan against the existing domain model, sharpens terminology, and updates documentation (CONTEXT.md, ADRs) inline as decisions crystallise. Use when user wants to stress-test a plan against their project's language and documented decisions.
---

## What to Do

Conduct thorough interviews about plan aspects, walking through design dependencies sequentially. "Ask the questions one at a time, waiting for feedback on each question before continuing." Explore codebases when questions can be answered that way rather than asking directly.

## Domain Awareness

Most repositories follow this structure:
- Root-level CONTEXT.md
- docs/adr/ directory for architectural decisions
- src/ for implementation

Multi-context systems have a CONTEXT-MAP.md pointing to where each context lives. Create documentation files only when necessary—no pre-emptive creation.

## Session Practices

- **Challenge terminology** by comparing user language against existing CONTEXT.md definitions
- **Sharpen vague terms** by proposing precise canonical alternatives
- **Test with scenarios** that probe edge cases and concept boundaries
- **Cross-reference code** to surface contradictions between stated behavior and actual implementation
- **Update CONTEXT.md immediately** when terms resolve; treat it strictly as glossary, never as spec

## ADR Criteria

Only create Architectural Decision Records when all three conditions hold: reversing the decision carries meaningful cost, the choice will perplex future readers without context, and genuine trade-offs between alternatives drove the selection.
