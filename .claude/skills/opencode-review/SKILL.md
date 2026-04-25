---
name: opencode-review
description: Use the opencode CLI to get a second-opinion review from a non-Claude model. Invoke when the user asks for an "opencode review", or proactively as the second reviewer before substantive plans, or when you need an independent pass that won't share Claude's training biases.
---

# opencode-review

`opencode` is a separately-installed CLI (assumed to be in `PATH` as `opencode`). It runs a non-Claude model (MiniMax-M2.7 by default) with full file/grep/bash tools, so it can ground itself in the repo before responding.

## When to invoke

- User explicitly asks for an "opencode review", "second opinion from opencode", "opencode pass", etc.
- Proactively as a second reviewer before substantive plans or architecture decisions.
- When you want a model with a different training distribution than Claude to pressure-test a design.
- As part of the self-review-before-commit protocol (see CLAUDE.md § Self-review-before-commit protocol).

Do NOT invoke for trivial work — calling out is slow (30–90 s) and the model spends tokens grounding itself.

## How to invoke

The canonical form is one shell call via the Bash tool:

```bash
opencode run "<prompt>" 2>&1 | tail -200
```

Important details:

1. **Quote the entire prompt as a single positional argument.** Embedded newlines are fine inside the double quotes; opencode treats the whole thing as one message.
2. **Always `2>&1 | tail -200`.** opencode prints a banner, tool-use lines, and the model's actual response. The banner and tool-use lines are noise; only the trailing model response matters. `tail -200` keeps it bounded.
3. **Set `timeout: 300000` (5 min) on the Bash tool call.** opencode invocations regularly run 60–120 s; the default 2-min timeout will kill them.
4. **Pass full absolute repo paths in the prompt** so opencode can read them with its built-in tools without needing a working directory hint.
5. **Tell opencode what you want and what you don't want** in the prompt body. Say "DO NOT write a plan, only pressure-test" if that's what you want — opencode has the same tendency as any model to over-deliver.

## Prompt structure that works

```
opencode run "You are reviewing a DESIGN PROBLEM for hanui. DO NOT write a plan. Pressure-test and raise issues.

## Context
<3–5 lines of project context>

Repo: <absolute path to repo>
Read these to ground yourself:
- <absolute path>/CLAUDE.md
- <absolute path>/.claude/agents/*.md

## Problem
<the specific question, in 5–15 lines>

## What I want from you
Answer these, under 400 words, be opinionated:
1. <numbered question>
2. <numbered question>
...
Be concrete. Give counter-examples if you disagree."
```

The shape:
1. Role + anti-instructions ("DO NOT write a plan").
2. Context paragraph.
3. Repo path + file list to ground on.
4. Problem statement.
5. Numbered questions with explicit length budget and tone direction.

## Model selection

Default model is MiniMax-M2.7. To pick something else:

```bash
opencode run -m <model-id> "<prompt>" 2>&1 | tail -200
```

Run `opencode models` for the live list. For diverse-training-distribution second opinion, prefer the default over models from the same family as your primary model.

## Failure modes

- **403 Forbidden / auth error** → run `opencode providers` to check credentials. If still broken, note in `open_questions` and proceed without the review.
- **Empty response** → opencode succeeded but the model returned nothing useful; rephrase the prompt to be more specific or split into smaller questions.
- **Output is mostly tool-use noise, no model response** → the prompt was too vague; opencode kept reading files instead of answering. Add an explicit "answer in under N words" budget.
- **Hangs past 5 min** → kill with the Bash tool and retry with a shorter prompt.

## Don't

- Don't pipe through `head` — the response is at the bottom, not the top.
- Don't omit `2>&1` — error output is sometimes where the actual response lands.
- Don't invoke opencode with write intent outside of the `oc-execute.sh` dispatcher (see `opencode-execute` skill). Write-intent invocations must go through the dispatcher, which acquires an exclusive lock. Use this skill for review and analysis only.
